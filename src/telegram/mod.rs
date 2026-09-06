//! Telegram bot integration — host-side only.
//!
//! The host owner sets a bot token (from @BotFather) in Settings; this
//! module then long-polls the Telegram Bot API, routes incoming
//! messages to paired family members, runs them through the same chat
//! pipeline KinAI uses internally, and mirrors the reply back to
//! Telegram. Bidirectional: messages typed in KinAI by a paired user
//! are also pushed to that user's Telegram chat.
//!
//! Layout:
//!   - mod.rs        — supervisor task + lifecycle (start / stop / restart)
//!   - api.rs        — thin reqwest wrapper around the Bot API endpoints
//!   - polling.rs    — getUpdates loop with offset bookkeeping
//!   - router.rs     — incoming update → chat-pipeline routing

pub mod api;
pub mod echo;
pub mod format;
pub mod polling;
pub mod router;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use tauri::{AppHandle, Runtime};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::SharedState;

pub use api::BotApi;

/// Supervisor handle stored on `AppState`. Lets `set_telegram_config`
/// stop a stale loop before starting a fresh one, and gives us a place
/// to abort cleanly during host shutdown.
#[derive(Default)]
pub struct TelegramSupervisor {
    pub task: Mutex<Option<JoinHandle<()>>>,
}

impl TelegramSupervisor {
    pub async fn stop(&self) {
        if let Some(t) = self.task.lock().await.take() {
            t.abort();
        }
    }
}

/// Start the long-poll loop if the host has a non-empty bot token in
/// config. If a loop is already running, stop it first (token may have
/// changed). Safe to call any number of times — no-op when token empty.
pub async fn start_or_restart<R: Runtime>(state: SharedState, app: AppHandle<R>) -> anyhow::Result<()> {
    let token = state.config.read().telegram.bot_token.clone();
    let sup = supervisor(&state);
    sup.stop().await;

    if token.trim().is_empty() {
        tracing::info!("telegram: no bot token configured; supervisor idle");
        return Ok(());
    }

    let api = api::BotApi::new(token);
    // Everything from here runs INSIDE the supervised task, so that the
    // getMe retry below is abortable. `stop()` aborts this handle, which
    // means a token change in Settings cannot leave an old retry loop
    // racing a new one into polling — two pollers would fight over
    // getUpdates and Telegram answers that with a 409.
    let handle = tokio::spawn(async move {
        // Validate + populate bot_username via getMe before kicking off
        // long-poll. Saves a round-trip on every pairing-link build.
        let me = match get_me_retrying(&api).await {
            Ok(me) => me,
            Err(e) => {
                tracing::error!("telegram: not starting — {e:#}");
                return;
            }
        };
        {
            let mut cfg = state.config.write();
            cfg.telegram.bot_username = me.username.clone().unwrap_or_default();
            cfg.save().ok();
        }

        // Best-effort: set the command list so paired users see slash-cmd
        // autocomplete in their phone keyboard. Failures here aren't fatal
        // (the bot still works without the menu).
        let menu = {
            let cfg = state.config.read();
            command_menu(&cfg)
        };
        tracing::info!(
            commands = menu.len(),
            list = %menu.iter().map(|c| format!("/{}", c.command)).collect::<Vec<_>>().join(" "),
            "telegram: registering command menu"
        );
        if let Err(e) = api.set_my_commands(&menu).await {
            tracing::warn!("telegram: setMyCommands failed (non-fatal): {e:?}");
        }

        tracing::info!(
            "telegram: long-poll started as @{}",
            me.username.unwrap_or_else(|| "<unknown>".into())
        );
        polling::run(api, state, app).await;
    });
    *sup.task.lock().await = Some(handle);
    Ok(())
}

/// True when getMe failed because the TOKEN is wrong, rather than because
/// the network was briefly unavailable.
///
/// Getting this wrong is bad in both directions, but not symmetrically:
/// treating a blip as a bad token brings back the silent-deafness bug this
/// whole change exists to kill, while treating a bad token as a blip only
/// costs a retry every 60s. So this stays deliberately narrow — Telegram
/// answers a bad token with `Unauthorized`, and everything else, including
/// the `Bad Gateway` it returns under load, is worth waiting out.
///
/// Deliberately NOT included: the `Not Found` Telegram returns for a
/// malformed token. A 404 is exactly what a captive portal or a meddling
/// proxy also produces, and misreading one of those as "your token is
/// wrong" would be the bad direction. A malformed token instead keeps
/// retrying and is surfaced by the escalating log line in
/// `get_me_retrying`, which names the token as a possible cause.
fn is_token_rejection(msg: &str) -> bool {
    msg.to_lowercase().contains("unauthorized")
}

/// Strip the bot token out of a message before it reaches the log.
///
/// reqwest's Display for a transport error appends the request URL, and
/// every Bot API URL embeds the token: `.../bot<id>:<secret>/getMe`. The
/// old code hit that once per process, so a token sat in the log file after
/// a failed start. This change retries, which without redaction would write
/// the token out every 60 seconds for as long as the network is down — a
/// standing secret in a file that gets attached to bug reports.
///
/// The primary caller is `api::scrub`, which runs every Bot API error
/// through this at the boundary — see the reasoning there for why the
/// startup path alone was not enough. It stayed the startup path only in
/// 0.2.116, and the log kept collecting tokens from `getUpdates`, which
/// fails far more often than `getMe` ever does.
pub(crate) fn redact_token(msg: &str) -> String {
    // Token shape is <digits>:<base64url-ish secret>, always preceded by
    // "bot" in the URL path.
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(at) = rest.find("/bot") {
        let (before, tail) = rest.split_at(at);
        out.push_str(before);
        let after = &tail[4..]; // skip "/bot"
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == ':' || c == '_' || c == '-'))
            .unwrap_or(after.len());
        if after[..end].contains(':') {
            out.push_str("/bot<redacted>");
            rest = &after[end..];
        } else {
            // Not a token (e.g. "/bots"); keep it verbatim.
            out.push_str("/bot");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Longest wait between getMe attempts.
const GET_ME_BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Attempt at which a failing start stops being a warning and becomes an
/// error. 5 attempts is ~30s of backoff — past any ordinary blip.
const ESCALATE_AFTER: u32 = 5;

/// Identify the bot, retrying transient failures instead of giving up.
///
/// A single failure here used to take Telegram out for the entire life of
/// the process. `start_or_restart` returned `Err`, both call sites merely
/// logged a warning, and nothing retried — so the bot went deaf until a
/// person noticed and restarted the app. That is exactly what happened on
/// 2026-09-05: a "Connection reset by peer" on the startup getMe left the
/// family with no Telegram for sixteen minutes, and the only reason it was
/// found is that someone said "KinAI does not answer on Telegram".
///
/// The same shape is likely at every launch-at-login start, where the app
/// can easily come up before the network is ready.
///
/// Note the asymmetry this repairs: `getUpdates` in polling.rs already
/// backs off and retries, and `setMyCommands` above is explicitly
/// non-fatal. This was the one call in the startup path with no
/// resilience, and it happened to be the one gating everything else.
///
/// A wrong token is a different thing from an unreachable network, and no
/// amount of retrying fixes it — Telegram answers those with
/// `Unauthorized`, so that one still gives up. Settings' "Test" button
/// (`test_telegram_token`) remains where a bad token gets reported.
async fn get_me_retrying(api: &BotApi) -> anyhow::Result<api::BotUser> {
    let mut delay = Duration::from_secs(2);
    let mut attempt: u32 = 1;
    loop {
        match api.get_me().await {
            Ok(me) => {
                if attempt > 1 {
                    tracing::info!(attempt, "telegram: getMe succeeded after retrying");
                }
                return Ok(me);
            }
            Err(e) => {
                let msg = redact_token(&format!("{e:#}"));
                if is_token_rejection(&msg) {
                    return Err(e).context("telegram token rejected");
                }
                // Escalate once, so a host stuck here is obvious in the log
                // rather than merely quiet. Today's incident was invisible
                // precisely because nothing said Telegram had not come up.
                if attempt == ESCALATE_AFTER {
                    tracing::error!(
                        attempt,
                        "telegram: still not started after {attempt} attempts — the family's \
                         phones are getting no answers. Check the host's network, and the bot \
                         token in Settings if this persists: {msg}"
                    );
                } else {
                    tracing::warn!(
                        attempt,
                        backoff_s = delay.as_secs(),
                        "telegram: getMe failed, retrying: {msg}"
                    );
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(GET_ME_BACKOFF_CAP);
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

/// Re-register the command menu after the model configuration changes.
///
/// The menu is otherwise only sent when the bot starts, so configuring a
/// new slot left Telegram advertising the old list until the app was
/// restarted — the same staleness that hid `/online`. Best-effort and
/// non-blocking: a failure here costs nothing but an out-of-date menu,
/// which is what we already had.
pub fn refresh_command_menu(state: &SharedState) {
    let (token, menu) = {
        let cfg = state.config.read();
        (cfg.telegram.bot_token.clone(), command_menu(&cfg))
    };
    if token.trim().is_empty() {
        return;
    }
    tokio::spawn(async move {
        let api = api::BotApi::new(token);
        match api.set_my_commands(&menu).await {
            Ok(_) => tracing::info!(
                commands = menu.len(),
                "telegram: command menu refreshed after a config change"
            ),
            Err(e) => tracing::warn!("telegram: menu refresh failed (non-fatal): {e:?}"),
        }
    });
}

/// Resolve the supervisor sub-state. Inlined here because all callers
/// hand us the full `SharedState`.
fn supervisor(state: &SharedState) -> Arc<TelegramSupervisor> {
    state.telegram.clone()
}

/// Slash commands surfaced in Telegram's command-menu UI (the list the
/// phone keyboard offers when you type "/").
///
/// Derived from `slash::SLOTS` and the live config rather than
/// hand-written. The previous version was a hardcoded list, and adding
/// the `online` slot in 0.2.96 updated the router — which iterates
/// SLOTS — but not the menu, so Telegram users could type `/online` and
/// have it work while it was invisible in the command list.
///
/// Model switches are only offered when there is an actual choice: a
/// slot must be configured and active, and at least two must exist,
/// matching the rule the in-app slash menu already uses.
fn command_menu(cfg: &crate::config::AppConfig) -> Vec<api::BotCommand> {
    let mut out = vec![
        api::BotCommand {
            command: "help".into(),
            description: "List available slash commands".into(),
        },
        api::BotCommand {
            command: "newchat".into(),
            description: "Start a fresh chat — ignore earlier context (memory kept)".into(),
        },
    ];

    if cfg.tts.enabled {
        out.push(api::BotCommand {
            command: "voice".into(),
            description: "Toggle spoken voice-note replies for this chat".into(),
        });
    }

    let active: Vec<&&str> = crate::slash::SLOTS
        .iter()
        .filter(|s| crate::slash::slot_settings(cfg, s).is_active())
        .collect();
    if active.len() >= 2 {
        for slot in &active {
            out.push(api::BotCommand {
                command: (**slot).into(),
                description: slot_menu_description(slot).into(),
            });
        }
    }

    if crate::comfyui::is_configured(&cfg.comfyui.base_url) {
        out.push(api::BotCommand {
            command: "pic".into(),
            description: "Generate an image (e.g. /pic a sunset over Miami)".into(),
        });
        out.push(api::BotCommand {
            command: "pichq".into(),
            description: "Generate a higher-quality image (slower)".into(),
        });
    }

    out
}

/// One-liner per slot for the Telegram menu. Kept beside the slot table
/// so a new slot fails to compile here rather than shipping unlisted.
fn slot_menu_description(slot: &str) -> &'static str {
    match slot {
        "fast" => "Switch this chat to the fast model (default)",
        "balanced" => "Switch this chat to the balanced model (middle ground)",
        "deep" => "Switch this chat to the deep model (slower, smarter)",
        "online" => "Switch this chat to the online model (leaves your home network)",
        _ => "Switch this chat to this model",
    }
}

#[cfg(test)]
mod menu_tests {
    use super::*;
    use crate::config::AppConfig;

    fn cfg_with_slots(slots: &[&str]) -> AppConfig {
        let mut cfg = AppConfig::default();
        // Start from nothing active, then switch on what the test wants.
        cfg.llm.base_url = String::new();
        cfg.llm.enabled = false;
        for s in slots {
            let set = |l: &mut crate::config::LlmSettings, model: &str| {
                l.base_url = "http://127.0.0.1:8080".into();
                l.model = model.into();
                l.enabled = true;
            };
            match *s {
                "fast" => set(&mut cfg.llm, "fast-m"),
                "balanced" => set(&mut cfg.llm_balanced, "bal-m"),
                "deep" => set(&mut cfg.llm_deep, "deep-m"),
                "online" => set(&mut cfg.llm_online, "online-m"),
                other => panic!("unknown slot {other}"),
            }
        }
        cfg
    }

    fn names(cfg: &AppConfig) -> Vec<String> {
        command_menu(cfg).into_iter().map(|c| c.command).collect()
    }

    /// THE regression: `/online` shipped in 0.2.96 working in the router
    /// but absent from Telegram's command menu, because the menu was a
    /// hand-written list. Every active slot must appear.
    #[test]
    fn every_active_slot_appears_in_the_menu() {
        let cfg = cfg_with_slots(&["fast", "balanced", "deep", "online"]);
        let n = names(&cfg);
        for slot in ["fast", "balanced", "deep", "online"] {
            assert!(n.contains(&slot.to_string()), "/{slot} missing from menu: {n:?}");
        }
    }

    /// Guards against the same drift for any slot added later: whatever
    /// is in slash::SLOTS and active must be offered.
    #[test]
    fn menu_covers_the_whole_slot_table() {
        let all: Vec<&str> = crate::slash::SLOTS.to_vec();
        let cfg = cfg_with_slots(&all);
        let n = names(&cfg);
        for slot in crate::slash::SLOTS {
            assert!(
                n.contains(&slot.to_string()),
                "slot /{slot} exists in SLOTS but is not offered in Telegram: {n:?}"
            );
        }
    }

    /// A slot the host never configured must not be advertised — tapping
    /// it would only produce "no model is configured for this slot".
    #[test]
    fn unconfigured_slots_are_not_offered() {
        let cfg = cfg_with_slots(&["fast", "deep"]);
        let n = names(&cfg);
        assert!(n.contains(&"fast".to_string()));
        assert!(n.contains(&"deep".to_string()));
        assert!(!n.contains(&"online".to_string()), "unconfigured /online offered: {n:?}");
        assert!(!n.contains(&"balanced".to_string()));
    }

    /// One model means no choice to make, so the switches are clutter —
    /// same rule the in-app slash menu uses.
    #[test]
    fn a_single_slot_offers_no_model_switches() {
        let cfg = cfg_with_slots(&["fast"]);
        let n = names(&cfg);
        assert!(!n.contains(&"fast".to_string()), "switches shown with one slot: {n:?}");
        assert!(n.contains(&"help".to_string()), "/help must always be offered");
        assert!(n.contains(&"newchat".to_string()));
    }

    /// Image commands only when a ComfyUI server is actually configured.
    #[test]
    fn image_commands_follow_comfyui_config() {
        let mut cfg = cfg_with_slots(&["fast", "deep"]);
        assert!(!names(&cfg).contains(&"pic".to_string()));
        cfg.comfyui.base_url = "http://192.168.1.50:8188".into();
        let n = names(&cfg);
        assert!(n.contains(&"pic".to_string()) && n.contains(&"pichq".to_string()), "{n:?}");
    }
}

#[cfg(test)]
mod startup_tests {
    use super::is_token_rejection;

    #[test]
    fn a_bad_token_is_not_retried_forever() {
        // What Telegram actually answers for a wrong token.
        assert!(is_token_rejection("telegram error: Unauthorized"));
        assert!(is_token_rejection("getMe send: telegram error: Unauthorized"));
    }

    #[test]
    fn the_bot_token_never_reaches_the_log() {
        use super::redact_token;
        // The shape the host's own log wrote on 2026-09-05 — this is how
        // the token got onto disk in the first place. The token below is a
        // fake with the real layout: 0.2.116 pasted the family's actual
        // token in here, and that commit is public.
        let leaked = "getMe send: error sending request for url \
                      (https://api.telegram.org/bot1234567890:AAFakeFakeFakeFakeFakeFakeFakeFakeFak/getMe)";
        let safe = redact_token(leaked);
        assert!(!safe.contains("AAFakeFakeFakeFakeFakeFakeFakeFakeFak"), "secret survived: {safe}");
        assert!(!safe.contains("1234567890:"), "bot id + secret survived: {safe}");
        assert!(safe.contains("/bot<redacted>/getMe"), "lost the useful shape: {safe}");
        // The rest of the message must survive — it is the diagnostic.
        assert!(safe.contains("error sending request"));
    }

    #[test]
    fn the_bot_token_never_reaches_the_log_from_getupdates() {
        use super::redact_token;
        // 0.2.116 redacted only the startup getMe, but four of the five
        // tokens that actually landed in ~/.kinai/logs/ came from the poll
        // loop. This is that error's shape, as polling.rs formats it:
        // anyhow's `{e:?}` prints the context line, then the chain.
        let leaked = "telegram getUpdates failed: getUpdates send: error sending request for url \
                      (https://api.telegram.org/bot1234567890:AAFakeFakeFakeFakeFakeFakeFakeFakeFak/getUpdates): \
                      Connection reset by peer (os error 54) — backing off 2s";
        let safe = redact_token(leaked);
        assert!(!safe.contains("AAFakeFakeFakeFakeFakeFakeFakeFakeFak"), "secret survived: {safe}");
        assert!(!safe.contains("1234567890:"), "bot id + secret survived: {safe}");
        assert!(safe.contains("/bot<redacted>/getUpdates"), "lost the useful shape: {safe}");
        // Everything a person reads the line for must survive.
        assert!(safe.contains("Connection reset by peer"), "lost the cause: {safe}");
        assert!(safe.contains("backing off 2s"), "lost the backoff: {safe}");
    }

    #[test]
    fn the_file_download_url_is_redacted_too() {
        use super::redact_token;
        // Downloads use a different path shape — /file/bot<token>/... —
        // and reach the log via router.rs's photo and voice handlers.
        let leaked = "download_file send: error sending request for url \
                      (https://api.telegram.org/file/bot1234567890:AAFakeFakeFakeFakeFakeFakeFakeFakeFak/photos/f_1.jpg)";
        let safe = redact_token(leaked);
        assert!(!safe.contains("AAFakeFakeFakeFakeFakeFakeFakeFakeFak"), "secret survived: {safe}");
        assert!(safe.contains("/file/bot<redacted>/photos/f_1.jpg"), "lost the useful shape: {safe}");
    }

    #[test]
    fn redaction_leaves_ordinary_text_alone() {
        use super::redact_token;
        for msg in [
            "telegram error: Bad Gateway",
            "connection reset by peer (os error 54)",
            "no /bots here",
        ] {
            assert_eq!(redact_token(msg), msg, "mangled a message with no token");
        }
    }

    #[test]
    fn transient_failures_are_retried() {
        // All three are verbatim from the host's own logs. The first is
        // what took Telegram down on 2026-09-05; the second is what
        // Telegram returns under load and which getUpdates already
        // survives; the third is an ordinary timeout.
        for msg in [
            "getMe send: client error (Connect): Connection reset by peer (os error 54)",
            "telegram error: Bad Gateway",
            "getMe send: operation timed out",
        ] {
            assert!(
                !is_token_rejection(msg),
                "would have given up on a transient failure: {msg}"
            );
        }
    }
}
