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
    // Validate + populate bot_username via getMe before kicking off
    // long-poll. Saves a round-trip on every pairing-link build.
    let me = api.get_me().await?;
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

    let handle = tokio::spawn(polling::run(api, state.clone(), app));
    *sup.task.lock().await = Some(handle);
    tracing::info!(
        "telegram: long-poll started as @{}",
        me.username.unwrap_or_else(|| "<unknown>".into())
    );
    Ok(())
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
