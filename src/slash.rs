//! Slash commands — intercepted BEFORE the LLM pipeline so messages
//! starting with /pic, /picHQ, /help, ? skip the chat model entirely
//! and route to native handlers (ComfyUI for image gen, a static
//! command list for help).
//!
//! Lives in its own module because two unrelated chat paths invoke it:
//!   1. `network::server::run_chat_turn`  — messages from CLIENTS over
//!      the WebSocket protocol.
//!   2. `commands::send_message`          — messages typed in the
//!      HOST's own UI, routed via Tauri IPC.
//! Both must intercept identically; without the shared module the
//! /pic command worked from a client but silently fell through to the
//! LLM when typed on the host.

use crate::config::{AppConfig, LlmSettings};

/// Which LLM slot a chat turn should run against, given the message
/// content and the active config. Plus the content stripped of any
/// model-selector prefix (`/fast `, `/deep `) so the LLM doesn't see
/// the routing token in its prompt. The second tuple element is a
/// short human-readable label for logging.
pub struct ResolvedRoute<'cfg> {
    pub settings: &'cfg LlmSettings,
    pub stripped_content: String,
    pub slot_label: &'static str,
    /// True when the user typed a bare `/fast` or `/deep` (no prompt
    /// after it) — a pure mode switch. The caller should persist the
    /// sticky slot (already done in route_for), reply with a short
    /// confirmation, and NOT run an LLM turn on the empty content.
    /// Without this, a bare `/deep` sent an empty user message to the
    /// model and got back junk / nothing.
    pub bare_switch: bool,
}

/// Pick the LlmSettings that should serve `content`:
///   * Explicit `/fast …` or `/deep …` prefix → route to that slot
///     AND persist the choice as the thread's sticky slot so
///     subsequent plain-text messages keep routing there.
///   * No prefix → use the thread's sticky slot (set by a prior
///     `/fast` or `/deep`), else the global default (fast first,
///     deep when fast isn't active).
///   * Neither active → still return the fast slot (the LLM call
///     will fail loudly downstream, which is the right UX — it
///     surfaces a config problem instead of silently picking a
///     paused model).
///
/// The "sticky" piece is what makes `/deep …` actually feel like a
/// mode switch instead of a one-shot — users were typing `/deep
/// question` and then expecting the next turn to also go deep, but
/// pre-v0.2.28 it snapped back to fast on every plain-text message.
/// Now `/deep` stays in effect for the thread until `/fast` (or a
/// new thread) flips it back.
///
/// The stripped content has the prefix removed AND its single
/// trailing/leading space trimmed once; everything else (newlines,
/// formatting) is preserved verbatim.
/// The three routable slots in fallback-priority order. One table
/// instead of per-slot copy-paste blocks — the 0.2.75 client-parity bug
/// came from exactly this kind of duplicated slot logic drifting apart.
/// Command words that are a second name for a slot: the user types the
/// left side, routing sees the right side.
///
/// 2026-09-23: the deep slot serves an uncensored model, so it is called
/// "uncensored" everywhere a person reads it. Only the NAME changed — the
/// slot key stays "deep" in config.toml, in every stored message row and
/// in each thread's sticky slot, because renaming the key would mean
/// migrating live family data for a cosmetic change. `/deep` keeps
/// working for anyone with the habit.
const COMMAND_ALIASES: &[(&str, &str)] = &[("uncensored", "deep")];

/// Rewrite a leading `/alias` to that slot's own `/command`, keeping
/// whatever followed it and any leading whitespace. `None` when the
/// message does not start with an alias.
fn rewrite_slot_alias(content: &str) -> Option<String> {
    let trimmed = content.trim_start();
    // to_ascii_lowercase is length-preserving, so byte offsets taken on
    // the lowered copy are valid in `trimmed`.
    let lower = trimmed.to_ascii_lowercase();
    for (alias, slot) in COMMAND_ALIASES {
        let p = format!("/{alias}");
        let hit = lower == p
            || lower.starts_with(&format!("{p} "))
            || lower.starts_with(&format!("{p}\n"));
        if hit {
            let leading = &content[..content.len() - trimmed.len()];
            return Some(format!("{leading}/{slot}{}", &trimmed[p.len()..]));
        }
    }
    None
}

/// What a person sees and types for a slot: its alias when it has one,
/// otherwise the key itself. Every menu, switch confirmation and the
/// Telegram command list goes through this, so the name cannot drift
/// between surfaces again — 0.2.134 renamed the help text but left
/// `/deep` in the menus, because each of those built its entries from
/// the raw key.
pub fn slot_display_name(slot: &str) -> &str {
    for (alias, key) in COMMAND_ALIASES {
        if *key == slot {
            return alias;
        }
    }
    slot
}

pub const SLOTS: &[&str] = &["fast", "balanced", "deep", "online"];

/// The slots automatic failover is allowed to reach for when a routed
/// slot's SERVER is down.
///
/// `online` is deliberately absent. Every other slot is hardware the
/// family owns; `online` is a paid endpoint outside the house. A local
/// server going down must never silently reroute a private family
/// conversation off the premises — the user gets there by asking for it
/// (`/online`, or the composer's model picker), never by accident.
pub const FAILOVER_SLOTS: &[&str] = &["fast", "balanced", "deep"];

/// Settings for a slot label ("fast" / "balanced" / "deep" / "online").
pub fn slot_settings<'cfg>(cfg: &'cfg AppConfig, label: &str) -> &'cfg LlmSettings {
    match label {
        "deep" => &cfg.llm_deep,
        "balanced" => &cfg.llm_balanced,
        "online" => &cfg.llm_online,
        _ => &cfg.llm,
    }
}

/// A finished chat turn plus which slot actually served it — the routed
/// slot, or a failover slot when the routed server was down.
pub struct ServedTurn {
    pub slot_label: String,
    pub settings: LlmSettings,
    pub result: crate::tools::loop_pipeline::PipelineResult,
}

/// Why a slot's server is not being tried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    /// Someone announced planned maintenance (see `availability`); the
    /// server is expected back at `until`.
    Maintenance { until: chrono::DateTime<chrono::Utc> },
    /// The server does not answer.
    Down,
}

/// Is `label`'s server unavailable right now, and why? Planned maintenance
/// is checked first and costs no network at all.
pub async fn slot_unavailable(state: &crate::AppState, label: &str) -> Option<Unavailable> {
    let base_url = {
        let cfg = state.config.read();
        slot_settings(&cfg, label).base_url.clone()
    };
    let authority = crate::availability::authority(&base_url);
    if let Some(m) = state.availability.lock().maintenance(&authority, chrono::Utc::now()) {
        return Some(Unavailable::Maintenance { until: m.until });
    }
    if slot_alive_cached(state, label).await {
        None
    } else {
        Some(Unavailable::Down)
    }
}

/// "16:45" in the host's own time zone — the house's clock.
fn local_hm(t: chrono::DateTime<chrono::Utc>) -> String {
    t.with_timezone(&chrono::Local).format("%H:%M").to_string()
}

/// The one line that opens an answer given by a backup model. Written ONCE
/// per answer, however many servers were skipped on the way.
///
/// `first_label` is the slot the user's message was aimed at, `why` is what
/// was wrong with it, `serving` is the slot that actually answers. The
/// opening and closing words are what `strip_failover_notice` recognises, so
/// the notice never reaches a later prompt as a standing claim.
pub(crate) fn failover_notice(
    first_label: &str,
    why: &Unavailable,
    serving_label: &str,
    serving: &LlmSettings,
) -> String {
    let wanted = if first_label == "fast" {
        "main".to_string()
    } else {
        slot_display_name(first_label).to_string()
    };
    let backup = format!(
        "**{}** (`{}`)",
        slot_display_name(serving_label),
        display_model_name(&serving.model)
    );
    match why {
        Unavailable::Maintenance { until } => format!(
            "⚠️ _The **{wanted}** model is being serviced right now (planned maintenance, \
back around {}). Answering with the backup model — {backup} — instead._\n\n",
            local_hm(*until)
        ),
        Unavailable::Down => format!(
            "⚠️ _The **{wanted}** model isn't available right now. Answering with the \
backup model — {backup} — instead._\n\n"
        ),
    }
}

/// Remove failover notices from the START of an answer that is being fed
/// back to a model as history. They describe how one answer was produced,
/// not a fact about the world: left in, a model reads "the main model isn't
/// available" in the transcript long after it is back and says so.
pub fn strip_failover_notice(text: &str) -> &str {
    const OPEN: &str = "⚠️ _The **";
    const CLOSE: &str = "instead._\n\n";
    let mut rest = text;
    while rest.starts_with(OPEN) {
        match rest.find(CLOSE) {
            // A notice is one short paragraph; refuse to swallow a long one.
            Some(i) if i < 600 => rest = &rest[i + CLOSE.len()..],
            _ => break,
        }
    }
    rest
}

/// The single error for "no server can answer". Always begins with the
/// phrase every surface passes through unchanged ("No model server is
/// answering"). `maintenance_until` adds the planned end when the cause is
/// announced maintenance; `online_hint` adds how to switch to the online
/// model by hand — never done automatically (see `FAILOVER_SLOTS`).
pub(crate) fn all_down_message(
    tried: &[String],
    maintenance_until: Option<chrono::DateTime<chrono::Utc>>,
    online_hint: bool,
    needs_sight: bool,
) -> String {
    let mut msg = if needs_sight {
        format!(
            "🔌 No model server is answering right now, and the backup model can't read \
pictures (tried: {}). Please send the picture again once the main model is back.",
            tried.join(", ")
        )
    } else {
        format!("🔌 No model server is answering right now (tried: {}).", tried.join(", "))
    };
    if let Some(until) = maintenance_until {
        msg.push_str(&format!(
            " The main model is being serviced (planned maintenance, back around {}).",
            local_hm(until)
        ));
    }
    if online_hint && !needs_sight {
        msg.push_str(
            " To use the online model instead, type /online before your question. \
It answers from the internet, so what you type then leaves your home network.",
        );
    }
    msg.push_str(
        " Otherwise check that your model server(s) are running — Settings → Models \
shows each slot's address — then try again.",
    );
    msg
}

/// Run a chat turn on the routed slot; when that slot's SERVER is down
/// (connection refused, DNS/timeout, 503-loading, stall watchdog —
/// `llm::is_server_down_error`) or announced as under maintenance, retry on
/// the next active slot in FAILOVER_SLOTS order and say so ONCE at the top
/// of the answer, instead of dying silently. Tries each slot at most once;
/// when no home server can answer, returns one honest, actionable error for
/// all surfaces (including how to pick the online model by hand — it is
/// never used automatically).
///
/// Servers are judged by ADDRESS (`availability::authority`): `fast` and
/// `balanced` are two names on one machine, so one failed request or
/// refused probe rules out both, and the user sees one notice, not two hops.
///
/// Pictures: a turn that carries an image only fails over to a slot whose
/// server reports it can read images (`vision::chat_handles_images`); a turn
/// already routed to a dedicated vision endpoint tries that first and, when
/// its server is down, falls through to the same search. If no backup can
/// see, the error says so plainly instead of answering blind.
///
/// Deliberately does NOT rewrite the per-thread sticky slot: the sticky
/// records the user's wish; when their model comes back, the thread
/// routes to it again on its own.
pub async fn run_turn_with_slot_failover<F>(
    route: crate::vision::Route,
    state: &crate::AppState,
    cfg: &AppConfig,
    routed_slot: &str,
    messages: Vec<crate::context::ChatMessage>,
    tools: Vec<crate::tools::registry::ToolDef>,
    tool_runtime: crate::tools::registry::ToolRuntime,
    handlers: crate::tools::loop_pipeline::PipelineHandlers,
    cancel: tokio_util::sync::CancellationToken,
    max_tokens_for: F,
) -> anyhow::Result<ServedTurn>
where
    F: Fn(&LlmSettings, &[crate::context::ChatMessage]) -> Option<usize>,
{
    use std::sync::atomic::{AtomicBool, Ordering};

    // Does the turn carry a picture the answering model must be able to see?
    let needs_sight = crate::vision::history_has_image(&messages);

    // Wraps the handlers so "a tool ran during this attempt" is recorded:
    // tool side effects are not idempotent (memory writes, metered search),
    // so a turn whose tools already ran is never silently re-run elsewhere.
    let track_tools = |flag: &std::sync::Arc<AtomicBool>| {
        let inner = handlers.on_tool.clone();
        let flag = flag.clone();
        crate::tools::loop_pipeline::PipelineHandlers {
            on_token: handlers.on_token.clone(),
            on_reasoning: handlers.on_reasoning.clone(),
            on_tool: std::sync::Arc::new(move |e| {
                if matches!(e, crate::tools::loop_pipeline::ToolEvent::Started { .. }) {
                    flag.store(true, Ordering::SeqCst);
                }
                inner(e);
            }),
        }
    };

    // A picture routed to a dedicated vision endpoint: that endpoint has its
    // own primary/failover pair. Try it — unless its server is already known
    // to be gone — and fall through to the slot search below only when the
    // SERVER is down. Any other failure is the caller's to see.
    if let crate::vision::Route::Vision { primary, .. } = &route {
        let vision_authority = crate::availability::authority(&primary.base_url);
        let known_down = {
            let a = state.availability.lock();
            a.maintenance(&vision_authority, chrono::Utc::now()).is_some()
                || a.failed_within(&vision_authority, crate::availability::FAILURE_MEMORY, std::time::Instant::now())
        };
        if !known_down {
            let settings = slot_settings(cfg, routed_slot).clone();
            let llm = crate::llm::LlmClient::new(settings.clone());
            let max_tokens = max_tokens_for(&settings, &messages);
            let tools_ran = std::sync::Arc::new(AtomicBool::new(false));
            let attempt = crate::vision::run_with_route(
                route.clone(),
                cfg,
                llm,
                &settings,
                messages.clone(),
                tools.clone(),
                tool_runtime.clone(),
                max_tokens,
                track_tools(&tools_ran),
                cancel.clone(),
            )
            .await;
            match attempt {
                Ok(result) => {
                    return Ok(ServedTurn {
                        slot_label: routed_slot.to_string(),
                        settings,
                        result,
                    })
                }
                Err(e) => {
                    let msg = e.to_string();
                    if cancel.is_cancelled()
                        || !crate::llm::is_server_down_error(&msg)
                        || tools_ran.load(Ordering::SeqCst)
                    {
                        return Err(e);
                    }
                    state
                        .availability
                        .lock()
                        .note_failed(&vision_authority, std::time::Instant::now());
                    tracing::warn!(
                        "vision endpoint down ({}); looking for a backup model that can read the picture",
                        crate::logsafe::error(&msg)
                    );
                }
            }
        }
        // Fall through: the slot search below, with `needs_sight` set.
    }

    let next_untried = |tried: &[String]| {
        FAILOVER_SLOTS
            .iter()
            .find(|s| !tried.iter().any(|t| t == **s) && slot_settings(cfg, s).is_active())
            .copied()
    };
    // Called when the search runs out of slots.
    let all_down_error = |tried: &[String]| {
        let routed_authority = crate::availability::authority(&slot_settings(cfg, routed_slot).base_url);
        let maintenance_until = state
            .availability
            .lock()
            .maintenance(&routed_authority, chrono::Utc::now())
            .map(|m| m.until);
        let online_hint = slot_settings(cfg, "online").is_active()
            && !tried.iter().any(|t| t == "online");
        anyhow::anyhow!(all_down_message(tried, maintenance_until, online_hint, needs_sight))
    };

    let mut label = routed_slot.to_string();
    // What was wrong with the first slot that could not serve — the reason
    // the one notice gives. `None` until something fails.
    let mut first_issue: Option<(String, Unavailable)> = None;
    let mut notice = String::new();
    let mut tried: Vec<String> = Vec::new();
    loop {
        tried.push(label.clone());
        let settings = slot_settings(cfg, &label).clone();
        let authority = crate::availability::authority(&settings.base_url);

        // Known unavailable (planned maintenance, or a refused probe, or a
        // failure moments ago on this very server): skip without a request.
        // A slot other than the routed one must also be able to read a
        // picture when the turn carries one — the routed slot was already
        // judged by `vision::decide`.
        let mut issue = slot_unavailable(state, &label).await;
        let mut cannot_see = false;
        if issue.is_none()
            && needs_sight
            && label != routed_slot
            && !crate::vision::chat_handles_images(&settings).await
        {
            cannot_see = true;
        }
        if issue.is_some() || cannot_see {
            let Some(next) = next_untried(&tried) else {
                return Err(all_down_error(&tried));
            };
            if let Some(i) = issue.take() {
                tracing::warn!("slot '{label}' unavailable ({i:?}); skipping to '{next}'");
                if first_issue.is_none() {
                    first_issue = Some((label.clone(), i));
                }
            } else {
                tracing::info!("slot '{label}' cannot read pictures; skipping to '{next}'");
            }
            label = next.to_string();
            continue;
        }

        // Re-trim the prompt for THIS slot's window. The context was
        // built against the ROUTED slot; replaying a 32k-window prompt
        // into an 8k-window failover slot would overflow its context and
        // drive computed max_tokens to zero.
        let mut attempt_msgs = messages.clone();
        // The prompt was built naming the ROUTED slot; if failover moved
        // the turn, the model must name the slot that actually answers,
        // not the one the user asked for.
        crate::context::retarget_active_model(&mut attempt_msgs, cfg, &label);
        crate::context::token_guard::trim_to_fit(
            &mut attempt_msgs,
            crate::context::builder::prompt_budget(settings.context_window, settings.max_tokens),
        );
        let max_tokens = max_tokens_for(&settings, &attempt_msgs);

        // One notice, written when a backup slot is about to answer. If that
        // attempt dies too, the text is REPLACED for the next backup, so the
        // stored answer carries a single line.
        if label != routed_slot {
            if let Some((first, why)) = &first_issue {
                notice = failover_notice(first, why, &label, &settings);
                (handlers.on_token)(notice.clone());
            }
        }

        let tools_ran = std::sync::Arc::new(AtomicBool::new(false));
        let llm = crate::llm::LlmClient::new(settings.clone());
        let attempt = crate::vision::run_with_route(
            crate::vision::Route::Chat,
            cfg,
            llm,
            &settings,
            attempt_msgs,
            tools.clone(),
            tool_runtime.clone(),
            max_tokens,
            track_tools(&tools_ran),
            cancel.clone(),
        )
        .await;
        match attempt {
            Ok(mut result) => {
                // A rescue model can still return an EMPTY completion (a
                // reasoning model burning its budget "thinking"). The
                // callers' empty-reply guards run BEFORE our prepend
                // would land, so handle emptiness here — the notice must
                // always be followed by an answer or an explanation.
                if result.final_content.trim().is_empty() {
                    result.final_content =
                        crate::tools::loop_pipeline::EMPTY_REPLY_NOTE.to_string();
                }
                if !notice.is_empty() {
                    // The streamed copy of the notice lives only in the live
                    // bubble; final_content is what gets persisted / edited
                    // into Telegram — prepend so the notice survives.
                    result.final_content = format!("{notice}{}", result.final_content);
                }
                state.availability.lock().note_ok(&authority);
                return Ok(ServedTurn {
                    slot_label: label,
                    settings,
                    result,
                });
            }
            Err(e) => {
                let msg = e.to_string();
                if cancel.is_cancelled() || !crate::llm::is_server_down_error(&msg) {
                    return Err(e);
                }
                if tools_ran.load(Ordering::SeqCst) {
                    return Err(anyhow::anyhow!(
                        "🔌 The **{label}** model stopped responding partway through \
your question, after it had already run tools (searches, memory). To avoid \
running those twice, KinAI didn't retry automatically — please send your \
message again."
                    ));
                }
                // Believe the failure for a few seconds, for every slot on
                // this server — the next message must not wait for it again.
                {
                    let now = std::time::Instant::now();
                    state.availability.lock().note_failed(&authority, now);
                    state.slot_health.lock().insert(label.clone(), (now, false));
                }
                let Some(next) = next_untried(&tried) else {
                    return Err(all_down_error(&tried));
                };
                tracing::warn!(
                    "slot '{label}' down ({}); failing over to '{next}'",
                    crate::logsafe::error(&msg)
                );
                if first_issue.is_none() {
                    first_issue = Some((label.clone(), Unavailable::Down));
                }
                label = next.to_string();
            }
        }
    }
}

/// Active slots advertised to client peers in the `Welcome` envelope,
/// in SLOTS order. Clients have no local LLM config, so this is the
/// only way their slash menu can know which model switches exist.
/// `alive` starts as `None` (unknown) — callers with async room fill it
/// via [`slot_alive_cached`] before sending.
pub fn active_slot_wires(cfg: &AppConfig) -> Vec<crate::network::protocol::HostSlotWire> {
    SLOTS
        .iter()
        .filter(|s| slot_settings(cfg, s).is_active())
        .map(|s| crate::network::protocol::HostSlotWire {
            slug: (*s).into(),
            model: slot_settings(cfg, s).model.clone(),
            alive: None,
        })
        .collect()
}

/// Cached liveness for a slot's model server. Cheap enough for menus and
/// switch confirmations without hammering the servers; the turn-level
/// failover is the safety net for a server that dies inside the window.
/// Inactive slots are always `false`. Never holds the config lock across the
/// probe await.
///
/// * A server under announced MAINTENANCE is down at once, with no probe.
/// * "Alive" is believed for 15 s, "down" only for 5 s, so a server that
///   comes back is noticed within seconds.
/// * A request that just failed against the server counts for every slot on
///   that server (`availability`), so `fast` and `balanced` — one machine —
///   are not probed, or tried, twice.
pub async fn slot_alive_cached(state: &crate::AppState, label: &str) -> bool {
    const ALIVE_TTL: std::time::Duration = std::time::Duration::from_secs(15);
    const DEAD_TTL: std::time::Duration = std::time::Duration::from_secs(5);
    let settings = {
        let cfg = state.config.read();
        let s = slot_settings(&cfg, label);
        if !s.is_active() {
            return false;
        }
        s.clone()
    };
    let authority = crate::availability::authority(&settings.base_url);
    if state
        .availability
        .lock()
        .maintenance(&authority, chrono::Utc::now())
        .is_some()
    {
        return false;
    }
    if let Some((probed_at, alive)) = state.slot_health.lock().get(label).copied() {
        let ttl = if alive { ALIVE_TTL } else { DEAD_TTL };
        if probed_at.elapsed() < ttl {
            return alive;
        }
    }
    if state.availability.lock().failed_within(
        &authority,
        crate::availability::FAILURE_MEMORY,
        std::time::Instant::now(),
    ) {
        return false;
    }
    let alive = crate::llm::detect::probe_alive(&settings).await;
    let now = std::time::Instant::now();
    state.slot_health.lock().insert(label.to_string(), (now, alive));
    {
        let mut a = state.availability.lock();
        if alive {
            a.note_ok(&authority);
        } else {
            a.note_failed(&authority, now);
        }
    }
    alive
}

/// Whether automatic failover could actually cover for `label` going
/// down: some OTHER active slot must exist in FAILOVER_SLOTS.
///
/// One function, used by both surfaces that make the promise (the app's
/// switch confirmation AND the Telegram bare-switch reply). The list of
/// slots a user can SWITCH to includes `online`; the list failover may
/// substitute from does not — and the 0.2.96 pre-tag audit caught the
/// Telegram copy of this check still using the wrong list. Shared so the
/// two can never disagree again.
pub fn failover_available(cfg: &AppConfig, label: &str) -> bool {
    FAILOVER_SLOTS
        .iter()
        .any(|s| *s != label && slot_settings(cfg, s).is_active())
}

/// The slot to actually serve a request aimed at `wanted`: the wanted
/// slot when active, else the first active slot in FAILOVER_SLOTS order,
/// else `wanted` itself (fail loudly downstream — a config problem should
/// surface, not silently pick a paused model).
///
/// The substitute is drawn from FAILOVER_SLOTS, not SLOTS: an unconfigured
/// `/deep` falls back to a machine at home, never out to the paid `online`
/// endpoint.
fn effective_slot<'cfg>(cfg: &'cfg AppConfig, wanted: &'static str) -> (&'static str, &'cfg LlmSettings) {
    if slot_settings(cfg, wanted).is_active() {
        return (wanted, slot_settings(cfg, wanted));
    }
    for s in FAILOVER_SLOTS {
        if slot_settings(cfg, s).is_active() {
            return (s, slot_settings(cfg, s));
        }
    }
    (wanted, slot_settings(cfg, wanted))
}

pub async fn route_for<'cfg>(
    db: &crate::db::Db,
    cfg: &'cfg AppConfig,
    peer_id: &str,
    thread_id: &str,
    content: &str,
) -> ResolvedRoute<'cfg> {
    // Map `/uncensored` onto the internal `/deep` before anything else
    // reads the text (see COMMAND_ALIASES).
    let aliased = rewrite_slot_alias(content);
    let content: &str = aliased.as_deref().unwrap_or(content);
    let lower = content.trim_start().to_ascii_lowercase();

    // Explicit `/fast …` / `/balanced …` / `/deep …` prefix: route there,
    // persist as the thread's sticky slot.
    for slot in SLOTS {
        let prefix = format!("/{slot}");
        if lower.starts_with(&format!("{prefix} "))
            || lower.starts_with(&format!("{prefix}\n"))
            || lower == prefix
        {
            let stripped = strip_prefix(content, &prefix);
            // NOTE: the sticky slot records what the user ASKED for, even
            // if it's currently inactive — matches the old behaviour and
            // means enabling the slot later makes the thread route there.
            let _ = db.set_thread_active_slot(peer_id, thread_id, Some(slot)).await;
            let (label, settings) = effective_slot(cfg, slot);
            let bare_switch = stripped.trim().is_empty();
            return ResolvedRoute { settings, stripped_content: stripped, slot_label: label, bare_switch };
        }
    }

    // No prefix — the thread's sticky slot wins when its model is active.
    let sticky = db
        .thread_active_slot(peer_id, thread_id)
        .await
        .ok()
        .flatten();
    if let Some(s) = sticky.as_deref() {
        if let Some(slot) = SLOTS.iter().find(|x| **x == s) {
            if slot_settings(cfg, slot).is_active() {
                return ResolvedRoute {
                    settings: slot_settings(cfg, slot),
                    stripped_content: content.to_string(),
                    slot_label: slot,
                    bare_switch: false,
                };
            }
        }
    }

    // Global default — first active slot in priority order (fast, then
    // balanced, then deep); fast when nothing is active (fails loudly).
    let (label, settings) = effective_slot(cfg, "fast");
    ResolvedRoute { settings, stripped_content: content.to_string(), slot_label: label, bare_switch: false }
}

/// Confirmation text for a bare `/fast` / `/balanced` / `/deep` mode
/// switch. Shown to the user instead of running an empty LLM turn, so
/// switching models gives visible feedback. Only ACTIVE other slots are
/// advertised as switch targets — naming a command that would silently
/// fall back is worse than naming none.
/// Trim a model id down to something worth reading in a chat message:
/// drop the directory (POSIX or Windows), drop the `.gguf`, and cap the
/// length. Purely cosmetic — the configured id is still what gets sent to
/// the backend.
pub(crate) fn display_model_name(model: &str) -> String {
    let mut s = model.trim();
    if let Some(i) = s.rfind(['/', '\\']) {
        s = &s[i + 1..];
    }
    let s = s.strip_suffix(".gguf").or_else(|| s.strip_suffix(".GGUF")).unwrap_or(s);
    if s.chars().count() > 40 {
        let head: String = s.chars().take(39).collect();
        format!("{head}…")
    } else {
        s.to_string()
    }
}

pub fn switch_confirmation(
    cfg: &AppConfig,
    route: &ResolvedRoute,
    wanted_alive: Option<bool>,
) -> String {
    let label = route.slot_label;
    let name = slot_display_name(label);
    // Show the model the way the rest of the UI does. llama-server reports
    // whatever `-m` it was given unless an alias was set, so a slot can be
    // called `/home/olares/models/Qwen3.6-35B-A3B-MTP-UD-Q4_K_S.gguf` — a
    // filesystem path dumped into the family's chat. The per-message footer
    // has always abbreviated; this line did not.
    let model = display_model_name(&route.settings.model);
    let others: Vec<String> = SLOTS
        .iter()
        .filter(|s| **s != label && slot_settings(cfg, s).is_active())
        .map(|s| format!("`/{}`", slot_display_name(s)))
        .collect();
    let mut msg = if others.is_empty() {
        format!("Switched to the **{name}** model (`{model}`).")
    } else {
        format!(
            "Switched to the **{name}** model (`{model}`). It stays active for this conversation until you switch again with {}.",
            others.join(" or ")
        )
    };
    let failover_available = failover_available(cfg, label);
    // Honest heads-up when the wanted slot's server failed its liveness
    // probe — the switch still sticks (the wish is recorded), but the
    // user learns NOW rather than after their next message fails over.
    if wanted_alive == Some(false) {
        if !failover_available {
            // Don't promise a failover that can't happen.
            msg.push_str(
                "\n\n⚠️ Heads-up: this model's server isn't responding right now, \
and no other model can take over automatically — messages will fail until it's \
back, unless you switch model yourself.",
            );
        } else {
            msg.push_str(
                "\n\n⚠️ Heads-up: this model's server isn't responding right now. \
Your messages will automatically use another available model until it's back.",
            );
        }
    }
    msg
}

/// Remove a leading slash command (`/fast` / `/deep`) plus the one
/// separating whitespace character after it. We deliberately don't
/// trim the rest of the message — the user's spacing / indentation
/// after the routing prefix is theirs to keep.
fn strip_prefix(content: &str, prefix: &str) -> String {
    let leading_ws_len = content.len() - content.trim_start().len();
    let after_prefix = &content.trim_start()[prefix.len()..];
    let after = after_prefix.strip_prefix(' ').unwrap_or(after_prefix);
    let mut out = String::with_capacity(leading_ws_len + after.len());
    out.push_str(&content[..leading_ws_len]);
    out.push_str(after);
    out
}

/// If `content` is a slash command we handle natively, return the
/// assistant's reply text. Returns `None` if the message should fall
/// through to the regular LLM pipeline.
pub async fn handle(cfg: &AppConfig, content: &str) -> Option<String> {
    let trimmed = content.trim();

    // /help and ? — always available.
    if trimmed.eq_ignore_ascii_case("/help") || trimmed == "?" {
        return Some(help_markdown(cfg));
    }

    // /pic and /picHQ
    if let Some((model, width, height, prompt)) = crate::comfyui::parse_slash(trimmed) {
        if !crate::comfyui::is_configured(&cfg.comfyui.base_url) {
            return Some(
                "**Image generation isn't configured on this host.**\n\nThe host owner can enable it in **Settings → Image generation** by pointing it at a ComfyUI server (e.g. `http://192.168.1.50:8188`).".into()
            );
        }
        if prompt.is_empty() {
            return Some(format!(
                "Usage: `/{slug} [WxH] <prompt>`\n\nExample: `/{slug} 1280x720 a sunset over Miami`\n\nDefault size is 1280×720 (or 1024×1024 for /picHQ).",
                slug = model.slug()
            ));
        }
        let started = std::time::Instant::now();
        match crate::comfyui::generate(
            &cfg.comfyui.base_url,
            model,
            &prompt,
            width,
            height,
        )
        .await
        {
            Ok(img) => {
                let host_http = http_origin_for(cfg)
                    .unwrap_or_else(|| String::from("http://127.0.0.1:4847"));
                let url = format!("{host_http}{}", img.url_path);
                Some(format!(
                    "![{alt}]({url})\n\n{prompt}\n\n_{label} · {w}×{h} · {secs:.1}s_",
                    alt = prompt.chars().take(120).collect::<String>(),
                    url = url,
                    prompt = prompt,
                    label = model.label(),
                    w = width,
                    h = height,
                    secs = img.elapsed_secs,
                ))
            }
            Err(e) => {
                let elapsed = started.elapsed().as_secs_f64();
                Some(format!(
                    "**/{} failed** after {:.1}s: {}",
                    model.slug(),
                    elapsed,
                    e
                ))
            }
        }
    } else {
        None
    }
}

/// HTTP origin clients can use to fetch a saved image from the host's
/// `/v1/pic/:filename` route. Mirrors the host_url stamped into invite
/// JWT audiences so every paired device (Mac and Windows) can reach it.
fn http_origin_for(cfg: &AppConfig) -> Option<String> {
    let host = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| cfg.host.bind_addr.clone());
    Some(format!("http://{host}:{}", cfg.host.port))
}

/// Markdown-flavored `/help` text. Rendered by the desktop chat (which
/// runs the message through `marked`) and persisted to the DB so the
/// thread keeps a readable transcript regardless of which client
/// rendered it.
///
/// Section-grouped (Models / Image generation / Info) so it scans at
/// a glance instead of being one wall of bullet points like the old
/// version was.
pub fn help_markdown(cfg: &AppConfig) -> String {
    let comfy_on = crate::comfyui::is_configured(&cfg.comfyui.base_url);
    let active: Vec<&&str> = SLOTS.iter().filter(|s| slot_settings(cfg, s).is_active()).collect();
    let mut out = String::from("**KinAI Commands**\n");

    // The Models section only earns its place when there's a choice.
    if active.len() >= 2 {
        out.push_str("\n**Models**\n");
        for slot in &active {
            let m = &slot_settings(cfg, slot).model;
            let line = match **slot {
                "fast" => format!("`/fast` — the everyday model (`{m}`), answers instantly\n"),
                "balanced" => format!("`/balanced` — the middle ground (`{m}`): smarter than fast, still quick\n"),
                "online" => format!("`/online` — an online model (`{m}`) on someone else's servers: use it when the question needs more than the models at home can give. What you type goes over the internet\n"),
                _ => format!("`/uncensored` — the uncensored model (`{m}`): answers what the others decline, slower\n"),
            };
            out.push_str(&line);
        }
    }

    out.push_str("\n**Image generation**\n");
    if comfy_on {
        out.push_str("`/pic [WxH] <prompt>` — Z-Image Turbo (fast, ~5s, default 1280×720)\n");
        out.push_str("`/picHQ [WxH] <prompt>` — Z-Image Base HQ (slower, ~30s, default 1024×1024)\n");
        out.push_str("Optional `WxH` overrides the size, e.g. `/picHQ 1280x720 a sunset over Miami` — any size 64×64 to 2048×2048.\n");
    } else {
        out.push_str("*(image generation not configured on this host — ask the host owner to set a ComfyUI URL in Settings → Image generation)*\n");
    }

    out.push_str("\n**Conversation**\n");
    out.push_str("`/newchat` — start a fresh chat so a new question doesn't reuse earlier context (your saved memory is kept). Add a question to ask it right away: `/newchat what's the capital of Japan?`\n");
    if cfg.tts.enabled {
        out.push_str("`/voice` — toggle spoken replies for the chat you're in: in the KinAI app replies are read aloud on the host machine (macOS only), in Telegram they arrive as voice notes; `/voice on` / `/voice off` set it explicitly\n");
    }

    out.push_str("\n**Info**\n");
    out.push_str("`/help` or `?` — show this list\n");
    out
}

/// Telegram-HTML version of `/help`. Sent with `parse_mode=HTML` so
/// section headers come through as proper bold and command names
/// render as inline code blocks. Without this, the same content sent
/// as plain text via `sendMessage` produces literal asterisks and
/// backticks in the bubble — visible noise instead of formatting.
///
/// Mirrors the structure of `help_markdown` so both renderings stay
/// in sync; only the inline syntax differs (`<b>`/`<code>` vs.
/// `**`/`` ` ``).
pub fn help_html(cfg: &AppConfig) -> String {
    let comfy_on = crate::comfyui::is_configured(&cfg.comfyui.base_url);
    let active: Vec<&&str> = SLOTS.iter().filter(|s| slot_settings(cfg, s).is_active()).collect();
    let esc = telegram_html_escape;
    let mut out = String::from("<b>KinAI Commands</b>\n");

    if active.len() >= 2 {
        out.push_str("\n<b>Models</b>\n");
        for slot in &active {
            let m = esc(&slot_settings(cfg, slot).model);
            let line = match **slot {
                "fast" => format!("<code>/fast</code> — the everyday model (<code>{m}</code>), answers instantly\n"),
                "balanced" => format!("<code>/balanced</code> — the middle ground (<code>{m}</code>): smarter than fast, still quick\n"),
                "online" => format!("<code>/online</code> — an online model (<code>{m}</code>) on someone else's servers: use it when the question needs more than the models at home can give. What you type goes over the internet\n"),
                _ => format!("<code>/uncensored</code> — the uncensored model (<code>{m}</code>): answers what the others decline, slower\n"),
            };
            out.push_str(&line);
        }
    }

    out.push_str("\n<b>Image generation</b>\n");
    if comfy_on {
        out.push_str("<code>/pic [WxH] &lt;prompt&gt;</code> — Z-Image Turbo (fast, ~5s, default 1280×720)\n");
        out.push_str("<code>/picHQ [WxH] &lt;prompt&gt;</code> — Z-Image Base HQ (slower, ~30s, default 1024×1024)\n");
        out.push_str("Optional <code>WxH</code> overrides the size, e.g. <code>/picHQ 1280x720 a sunset over Miami</code> — any size 64×64 to 2048×2048.\n");
    } else {
        out.push_str("<i>(image generation not configured on this host — ask the host owner to set a ComfyUI URL in Settings → Image generation)</i>\n");
    }

    out.push_str("\n<b>Conversation</b>\n");
    out.push_str("<code>/newchat</code> — start a fresh chat so a new question doesn't reuse earlier context (your saved memory is kept). Add a question to ask it right away: <code>/newchat what's the capital of Japan?</code>\n");
    if cfg.tts.enabled {
        out.push_str("<code>/voice</code> — toggle spoken replies for the chat you're in: in the KinAI app replies are read aloud on the host machine (macOS only), in Telegram they arrive as voice notes; <code>/voice on</code> / <code>/voice off</code> set it explicitly\n");
    }

    out.push_str("\n<b>Info</b>\n");
    out.push_str("<code>/help</code> or <code>?</code> — show this list\n");
    out
}

/// Escape the three special characters Telegram's HTML parse mode
/// reserves: `<`, `>`, `&`. Used on values we substitute into the
/// help template (model names from config). Static strings in the
/// template are already pre-escaped.
fn telegram_html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[test]
    fn uncensored_is_another_name_for_the_deep_slot() {
        // The user-facing rename (2026-09-23): typing /uncensored must land
        // on the same slot as /deep, with the rest of the message intact.
        assert_eq!(rewrite_slot_alias("/uncensored hello").as_deref(), Some("/deep hello"));
        assert_eq!(rewrite_slot_alias("/UNCENSORED Hello There").as_deref(), Some("/deep Hello There"));
        assert_eq!(rewrite_slot_alias("/uncensored").as_deref(), Some("/deep"));
        assert_eq!(rewrite_slot_alias("/uncensored\nline two").as_deref(), Some("/deep\nline two"));
        // Leading whitespace is the user's, keep it.
        assert_eq!(rewrite_slot_alias("  /uncensored x").as_deref(), Some("  /deep x"));
    }

    #[test]
    fn things_that_only_resemble_the_alias_are_left_alone() {
        // Not a whole command word.
        assert_eq!(rewrite_slot_alias("/uncensoredx hi"), None);
        // Not at the start of the message.
        assert_eq!(rewrite_slot_alias("please /uncensored this"), None);
        // /deep routes on its own; it is not an alias of itself.
        assert_eq!(rewrite_slot_alias("/deep hello"), None);
        assert_eq!(rewrite_slot_alias("plain question"), None);
    }

    async fn fresh_db() -> crate::db::Db {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        crate::db::migrate::run(&pool).await.expect("migrations");
        crate::db::Db { pool }
    }

    // Not compiled on Windows: constructing AppState in the LIB test
    // binary links tauri's dialog/window code, whose comctl32-v6-only
    // import (TaskDialogIndirect) kills an unmanifested cargo-test
    // executable at load (STATUS_ENTRYPOINT_NOT_FOUND) — cargo has no
    // link-arg key for lib tests to embed a manifest. The logic under
    // test is platform-independent and stays covered on macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    async fn fresh_state(cfg: &AppConfig) -> crate::AppState {
        crate::AppState {
            handle: parking_lot::RwLock::new(None),
            config: parking_lot::RwLock::new(cfg.clone()),
            db: fresh_db().await,
            llm: tokio::sync::Mutex::new(crate::llm::LlmClient::new(cfg.llm.clone())),
            net: std::sync::Arc::new(tokio::sync::Mutex::new(
                crate::network::NetState::default(),
            )),
            stats: parking_lot::RwLock::new(crate::RuntimeStats::default()),
            telegram: std::sync::Arc::new(crate::telegram::TelegramSupervisor::default()),
            pending_turns: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            slot_health: parking_lot::Mutex::new(std::collections::HashMap::new()),
            availability: parking_lot::Mutex::new(crate::availability::Availability::default()),
            fact_checks_running: parking_lot::Mutex::new(std::collections::HashSet::new()),
            tts_child: parking_lot::Mutex::new(None),
        }
    }

    fn cfg_three_slots() -> AppConfig {
        let mut cfg = AppConfig::default(); // fast active by default
        cfg.llm_balanced.base_url = "http://192.168.1.50:8084".into();
        cfg.llm_balanced.model = "balanced-33b".into();
        cfg.llm_balanced.enabled = true;
        cfg.llm_deep.base_url = "http://192.168.1.50:8081".into();
        cfg.llm_deep.model = "deep-35b".into();
        cfg.llm_deep.enabled = true;
        cfg
    }

    #[test]
    fn active_slot_wires_lists_active_slots_in_order() {
        let cfg = cfg_three_slots();
        let wires = active_slot_wires(&cfg);
        let slugs: Vec<&str> = wires.iter().map(|w| w.slug.as_str()).collect();
        assert_eq!(slugs, vec!["fast", "balanced", "deep"]);
        assert_eq!(wires[1].model, "balanced-33b");
        assert_eq!(wires[2].model, "deep-35b");

        // Disabling a slot removes it; empty base_url removes another.
        let mut cfg = cfg_three_slots();
        cfg.llm_balanced.enabled = false;
        cfg.llm_deep.base_url = String::new();
        let wires = active_slot_wires(&cfg);
        let slugs: Vec<&str> = wires.iter().map(|w| w.slug.as_str()).collect();
        assert_eq!(slugs, vec!["fast"]);
    }

    #[test]
    fn welcome_without_host_slots_still_deserializes() {
        // A 0.2.79-or-older host omits `host_slots` — a new client must
        // parse its Welcome and default to an empty slot list (menu
        // falls back to local config, i.e. stays empty on clients).
        let old_wire = r#"{"type":"welcome","family_name":"Fam","host_version":"0.2.79","host_model":"m"}"#;
        let env: crate::network::protocol::Envelope = serde_json::from_str(old_wire).unwrap();
        match env {
            crate::network::protocol::Envelope::Welcome { host_slots, .. } => {
                assert!(host_slots.is_empty());
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn explicit_balanced_routes_strips_and_sticks() {
        let db = fresh_db().await;
        let cfg = cfg_three_slots();
        let t = db.create_thread("host", Some("t")).await.unwrap();

        let r = route_for(&db, &cfg, "host", &t.id, "/balanced what is 2+2?").await;
        assert_eq!(r.slot_label, "balanced");
        assert_eq!(r.settings.model, "balanced-33b");
        assert_eq!(r.stripped_content, "what is 2+2?");
        assert!(!r.bare_switch);

        // Sticky: the NEXT plain message keeps routing to balanced.
        let r2 = route_for(&db, &cfg, "host", &t.id, "and 3+3?").await;
        assert_eq!(r2.slot_label, "balanced");
        assert_eq!(r2.stripped_content, "and 3+3?");
    }

    #[test]
    fn the_deep_slot_is_named_uncensored_and_the_others_keep_their_keys() {
        assert_eq!(slot_display_name("deep"), "uncensored");
        for s in ["fast", "balanced", "online"] {
            assert_eq!(slot_display_name(s), s);
        }
    }

    #[tokio::test]
    async fn switching_confirms_the_name_people_use_not_the_key() {
        let db = fresh_db().await;
        let cfg = cfg_three_slots();
        let t = db.create_thread("host", Some("t")).await.unwrap();
        // A bare /uncensored is a mode switch onto the deep slot...
        let r = route_for(&db, &cfg, "host", &t.id, "/uncensored").await;
        assert!(r.bare_switch);
        assert_eq!(r.slot_label, "deep");
        let msg = switch_confirmation(&cfg, &r, None);
        assert!(msg.contains("**uncensored**"), "{msg}");
        assert!(!msg.contains("**deep**"), "{msg}");
        // ...and switching AWAY offers it under its name, not `/deep`.
        let r2 = route_for(&db, &cfg, "host", &t.id, "/balanced").await;
        let msg2 = switch_confirmation(&cfg, &r2, None);
        assert!(msg2.contains("`/uncensored`"), "{msg2}");
        assert!(!msg2.contains("`/deep`"), "{msg2}");
    }

    #[tokio::test]
    async fn bare_balanced_is_a_mode_switch() {
        let db = fresh_db().await;
        let cfg = cfg_three_slots();
        let t = db.create_thread("host", Some("t")).await.unwrap();
        let r = route_for(&db, &cfg, "host", &t.id, "/balanced").await;
        assert!(r.bare_switch);
        assert_eq!(r.slot_label, "balanced");
        let msg = switch_confirmation(&cfg, &r, None);
        assert!(msg.contains("balanced") && msg.contains("balanced-33b"));
        assert!(!msg.contains("Heads-up"), "no liveness warning when unknown");

        // Probe says the wanted slot is down → the confirmation carries
        // the heads-up but the switch itself still sticks.
        let warned = switch_confirmation(&cfg, &r, Some(false));
        assert!(warned.contains("Heads-up") && warned.contains("another available model"));
        let healthy = switch_confirmation(&cfg, &r, Some(true));
        assert!(!healthy.contains("Heads-up"));
    }

    /// `cfg_three_slots` plus a configured ONLINE slot.
    fn cfg_with_online() -> AppConfig {
        let mut cfg = cfg_three_slots();
        cfg.llm_online.base_url = "https://api.deepseek.com".into();
        cfg.llm_online.model = "deepseek-v4-flash".into();
        cfg.llm_online.api_key = Some("sk-test".into());
        cfg.llm_online.enabled = true;
        cfg
    }

    #[tokio::test]
    async fn explicit_online_routes_and_sticks() {
        let db = fresh_db().await;
        let cfg = cfg_with_online();
        let t = db.create_thread("host", Some("t")).await.unwrap();

        let r = route_for(&db, &cfg, "host", &t.id, "/online who won the toss?").await;
        assert_eq!(r.slot_label, "online");
        assert_eq!(r.settings.model, "deepseek-v4-flash");
        assert_eq!(r.stripped_content, "who won the toss?");

        // Sticky like every other slot.
        let r2 = route_for(&db, &cfg, "host", &t.id, "and the score?").await;
        assert_eq!(r2.slot_label, "online");
    }

    /// The whole point of FAILOVER_SLOTS: a dead local model must never
    /// silently push the family's conversation out to a paid endpoint.
    #[tokio::test]
    async fn online_is_never_an_automatic_fallback() {
        let db = fresh_db().await;
        // Only `online` is configured — every local slot is off.
        let mut cfg = AppConfig::default();
        cfg.llm.base_url = String::new();
        cfg.llm.enabled = false;
        cfg.llm_online.base_url = "https://api.deepseek.com".into();
        cfg.llm_online.model = "deepseek-v4-flash".into();
        cfg.llm_online.enabled = true;
        let t = db.create_thread("host", Some("t")).await.unwrap();

        // A plain message with no local model available must NOT land on
        // `online`; it stays on `fast` and fails loudly downstream.
        let r = route_for(&db, &cfg, "host", &t.id, "hello").await;
        assert_eq!(
            r.slot_label, "fast",
            "a plain message must never be routed to the paid online slot on its own"
        );

        // Same for an explicit `/deep` whose slot isn't configured: the
        // substitute comes from the machines at home, never from online.
        let r = route_for(&db, &cfg, "host", &t.id, "/deep think about this").await;
        assert_ne!(r.slot_label, "online");
    }

    #[test]
    fn online_is_routable_but_outside_the_failover_chain() {
        assert!(SLOTS.contains(&"online"), "online must be pickable");
        assert!(
            !FAILOVER_SLOTS.contains(&"online"),
            "online must never be an automatic failover target"
        );
        // Every failover slot is also routable.
        for s in FAILOVER_SLOTS {
            assert!(SLOTS.contains(s), "{s} missing from SLOTS");
        }
        // And the mapping resolves to its own config, not fast's.
        let cfg = cfg_with_online();
        assert_eq!(slot_settings(&cfg, "online").model, "deepseek-v4-flash");
    }

    #[test]
    fn online_appears_in_help_with_its_own_wording() {
        let cfg = cfg_with_online();
        let md = help_markdown(&cfg);
        assert!(md.contains("`/online`"), "online missing from help");
        // The old catch-all arm would have described it as the reasoning
        // model — check it got its own line, not deep's.
        let online_line = md
            .lines()
            .find(|l| l.starts_with("`/online`"))
            .expect("no /online line");
        assert!(
            !online_line.contains("reasoning model"),
            "online is being described as the deep model: {online_line}"
        );
        assert!(online_line.contains("deepseek-v4-flash"));
    }

    /// When the only OTHER configured slot is `online`, a dead model must
    /// not promise that KinAI will cover automatically — it won't.
    #[tokio::test]
    async fn switch_warning_does_not_promise_a_failover_online_cannot_do() {
        let db = fresh_db().await;
        let mut cfg = AppConfig::default(); // fast active
        cfg.llm_online.base_url = "https://api.deepseek.com".into();
        cfg.llm_online.model = "deepseek-v4-flash".into();
        cfg.llm_online.enabled = true;
        let t = db.create_thread("host", Some("t")).await.unwrap();

        let r = route_for(&db, &cfg, "host", &t.id, "/fast").await;
        let warned = switch_confirmation(&cfg, &r, Some(false));
        assert!(warned.contains("Heads-up"));
        assert!(
            !warned.contains("automatically use another available model"),
            "promised an automatic failover that only `online` could serve: {warned}"
        );
    }

    #[test]
    fn server_down_classification() {
        use crate::llm::is_server_down_error;
        // The three real dead-slot shapes seen in production logs.
        assert!(is_server_down_error(
            "error sending request for url (http://192.168.1.211:8081/v1/chat/completions)"
        ));
        assert!(is_server_down_error("LLM error 503 Service Unavailable: Loading model"));
        assert!(is_server_down_error(
            "llm stream: tcp connect error: Connection refused (os error 61)"
        ));
        assert!(is_server_down_error("the model server went silent for 5 minutes"));
        // Content-level failures must NOT trigger slot failover.
        assert!(!is_server_down_error("LLM error 400 Bad Request: context length exceeded"));
        assert!(!is_server_down_error("LLM error 404: model not found"));
    }

    /// Both slots point at dead localhost ports → the failover walks
    /// them, then returns the honest all-down error (fast: connection
    /// refused fails in milliseconds on loopback).
    /// (cfg-gated off Windows with fresh_state — see its comment.)
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn failover_all_slots_down_yields_honest_error() {
        let mut cfg = AppConfig::default();
        cfg.llm.base_url = "http://127.0.0.1:1".into();
        cfg.llm.model = "dead-fast".into();
        cfg.llm.enabled = true;
        cfg.llm_deep.base_url = "http://127.0.0.1:2".into();
        cfg.llm_deep.model = "dead-deep".into();
        cfg.llm_deep.enabled = true;

        let noop = std::sync::Arc::new(|_: String| {});
        let handlers = crate::tools::loop_pipeline::PipelineHandlers {
            on_token: noop.clone(),
            on_reasoning: noop,
            on_tool: std::sync::Arc::new(|_| {}),
        };
        let state = fresh_state(&cfg).await;
        let result = run_turn_with_slot_failover(
            crate::vision::Route::Chat,
            &state,
            &cfg,
            "fast",
            vec![crate::context::ChatMessage::User {
                content: "hi".into(),
                name: None,
                image_data_urls: vec![],
            }],
            vec![],
            crate::tools::registry::ToolRuntime::default(),
            handlers,
            tokio_util::sync::CancellationToken::new(),
            |_, _| None,
        )
        .await;
        let Err(err) = result else {
            panic!("all slots dead must error")
        };
        let msg = err.to_string();
        assert!(msg.contains("No model server is answering"), "got: {msg}");
        assert!(msg.contains("fast") && msg.contains("deep"), "lists tried slots: {msg}");
    }

    #[tokio::test]
    async fn inactive_balanced_falls_back_to_fast() {
        let db = fresh_db().await;
        let mut cfg = cfg_three_slots();
        cfg.llm_balanced.enabled = false;
        let t = db.create_thread("host", Some("t")).await.unwrap();
        let r = route_for(&db, &cfg, "host", &t.id, "/balanced hello").await;
        assert_eq!(r.slot_label, "fast", "inactive slot must fall back");
        assert_eq!(r.settings.model, cfg.llm.model);
        // The sticky records the WISH — enabling balanced later routes there.
        let mut cfg2 = cfg.clone();
        cfg2.llm_balanced.enabled = true;
        let r2 = route_for(&db, &cfg2, "host", &t.id, "again").await;
        assert_eq!(r2.slot_label, "balanced");
    }

    #[tokio::test]
    async fn fast_and_deep_behave_as_before() {
        let db = fresh_db().await;
        let cfg = cfg_three_slots();
        let t = db.create_thread("host", Some("t")).await.unwrap();
        let d = route_for(&db, &cfg, "host", &t.id, "/deep think hard").await;
        assert_eq!(d.slot_label, "deep");
        assert_eq!(d.stripped_content, "think hard");
        let f = route_for(&db, &cfg, "host", &t.id, "/fast quick one").await;
        assert_eq!(f.slot_label, "fast");
        // Plain message after /fast stays fast.
        let p = route_for(&db, &cfg, "host", &t.id, "plain").await;
        assert_eq!(p.slot_label, "fast");
    }

    #[test]
    fn help_lists_all_three_when_active() {
        let cfg = cfg_three_slots();
        let md = help_markdown(&cfg);
        assert!(md.contains("/fast") && md.contains("/balanced") && md.contains("/uncensored"));
        // The deep slot is advertised under its user-facing name; /deep still
        // routes (see COMMAND_ALIASES) but is no longer what the help teaches.
        assert!(!md.contains("`/deep`"));
        assert!(md.contains("balanced-33b"));
        let html = help_html(&cfg);
        assert!(html.contains("/balanced") && html.contains("balanced-33b"));
        // Telegram renders the HTML help — it must carry the rename too.
        assert!(html.contains("/uncensored") && !html.contains("<code>/deep</code>"));
    }

    #[test]
    fn help_hides_models_section_with_one_slot() {
        let cfg = AppConfig::default(); // only fast active
        let md = help_markdown(&cfg);
        assert!(!md.contains("/balanced") && !md.contains("**Models**"));
    }
}

#[cfg(test)]
mod display_name_tests {
    use super::display_model_name;

    #[test]
    fn paths_are_trimmed_to_the_model_name() {
        // the owner's three slots, verbatim from what each server reports.
        assert_eq!(
            display_model_name("/home/olares/models/Qwen3.6-35B-A3B-MTP-UD-Q4_K_S.gguf"),
            "Qwen3.6-35B-A3B-MTP-UD-Q4_K_S"
        );
        assert_eq!(display_model_name("Laguna-XS-2.1"), "Laguna-XS-2.1");
        assert_eq!(
            display_model_name("C:\\models\\Qwen3.6-35B-A3B-uncensored-heretic-Native-MTP-Preserved-Q6_K.gguf"),
            // 60 chars after trimming path + .gguf, so it caps.
            "Qwen3.6-35B-A3B-uncensored-heretic-Nati…"
        );
    }

    #[test]
    fn vendor_prefixed_ids_keep_their_model_part() {
        assert_eq!(display_model_name("olares/gpt-oss-20b"), "gpt-oss-20b");
        assert_eq!(display_model_name("meta-llama/llama-4-scout-17b-16e-instruct"),
                   "llama-4-scout-17b-16e-instruct");
    }

    #[test]
    fn short_names_and_edge_cases_are_untouched() {
        assert_eq!(display_model_name("qwen3:14b"), "qwen3:14b");
        assert_eq!(display_model_name(""), "");
        assert_eq!(display_model_name("  spaced  "), "spaced");
        // A path ending in a separator must not panic or slice mid-char.
        assert_eq!(display_model_name("/models/"), "");
        assert_eq!(display_model_name("модель-очень-длинное-имя-которое-точно-превышает-лимит"),
                   "модель-очень-длинное-имя-которое-точно-…");
    }
}

/// End-to-end scenarios for the slot failover: real `run_turn_with_slot_failover`
/// against FAKE model servers that count what they are asked. Cfg-gated off
/// Windows for the reason `tests::fresh_state` is (constructing AppState in the
/// lib test binary links tauri's window code there).
#[cfg(all(test, not(target_os = "windows")))]
mod failover_scenario_tests {
    use super::*;
    use axum::routing::{get, post};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// A stand-in llama.cpp server: `/health`, `/props` and a streaming
    /// `/v1/chat/completions` that answers with its own name.
    struct FakeLlm {
        base_url: String,
        chat_hits: Arc<AtomicUsize>,
        health_hits: Arc<AtomicUsize>,
        last_body: Arc<Mutex<String>>,
    }

    async fn fake_llm(name: &'static str, can_see: bool, healthy: bool) -> FakeLlm {
        let chat_hits = Arc::new(AtomicUsize::new(0));
        let health_hits = Arc::new(AtomicUsize::new(0));
        let last_body = Arc::new(Mutex::new(String::new()));
        let (c, h, b) = (chat_hits.clone(), health_hits.clone(), last_body.clone());
        let app = axum::Router::new()
            .route(
                "/health",
                get(move || {
                    let h = h.clone();
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        if healthy {
                            (axum::http::StatusCode::OK, "{\"status\":\"ok\"}")
                        } else {
                            (axum::http::StatusCode::BAD_GATEWAY, "{\"error\":\"engine unreachable\"}")
                        }
                    }
                }),
            )
            .route(
                "/props",
                get(move || async move {
                    axum::Json(serde_json::json!({ "modalities": { "vision": can_see, "audio": false } }))
                }),
            )
            .route(
                "/v1/chat/completions",
                post(move |body: String| {
                    let c = c.clone();
                    let b = b.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        *b.lock().unwrap() = body;
                        let chunk = serde_json::json!({
                            "choices": [{ "index": 0, "delta": { "role": "assistant", "content": format!("answer from {name}") }, "finish_reason": null }]
                        });
                        let stop = serde_json::json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] });
                        (
                            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                            format!("data: {chunk}\n\ndata: {stop}\n\ndata: [DONE]\n\n"),
                        )
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeLlm { base_url: format!("http://127.0.0.1:{port}"), chat_hits, health_hits, last_body }
    }

    /// Nothing listens here: connection refused, in milliseconds.
    const DEAD: &str = "http://127.0.0.1:1";

    fn slot(cfg_slot: &mut LlmSettings, base_url: &str, model: &str) {
        cfg_slot.provider = "llamacpp".into();
        cfg_slot.base_url = base_url.into();
        cfg_slot.model = model.into();
        cfg_slot.enabled = true;
        cfg_slot.image_recognition = "auto".into();
    }

    /// fast + balanced share one server (`main_url`), deep is the backup.
    fn cfg(main_url: &str, backup_url: &str) -> AppConfig {
        let mut cfg = AppConfig::default();
        slot(&mut cfg.llm, main_url, "glm-fast");
        slot(&mut cfg.llm_balanced, main_url, "glm-balanced");
        slot(&mut cfg.llm_deep, backup_url, "qwen-backup");
        cfg.llm_online.enabled = false;
        cfg
    }

    async fn state(cfg: &AppConfig) -> crate::AppState {
        crate::test_state(cfg).await
    }

    fn user(text: &str, images: Vec<String>) -> Vec<crate::context::ChatMessage> {
        vec![crate::context::ChatMessage::User {
            content: text.into(),
            name: None,
            image_data_urls: images,
        }]
    }

    /// Run a turn; returns the result plus every chunk streamed to the user.
    async fn run(
        state: &crate::AppState,
        cfg: &AppConfig,
        route: crate::vision::Route,
        routed: &str,
        messages: Vec<crate::context::ChatMessage>,
    ) -> (anyhow::Result<ServedTurn>, Vec<String>) {
        let streamed = Arc::new(Mutex::new(Vec::<String>::new()));
        let s2 = streamed.clone();
        let noop = Arc::new(|_: String| {});
        let handlers = crate::tools::loop_pipeline::PipelineHandlers {
            on_token: Arc::new(move |t: String| s2.lock().unwrap().push(t)),
            on_reasoning: noop,
            on_tool: Arc::new(|_| {}),
        };
        let result = run_turn_with_slot_failover(
            route,
            state,
            cfg,
            routed,
            messages,
            vec![],
            crate::tools::registry::ToolRuntime::default(),
            handlers,
            tokio_util::sync::CancellationToken::new(),
            |_, _| None,
        )
        .await;
        let chunks = streamed.lock().unwrap().clone();
        (result, chunks)
    }

    #[tokio::test]
    async fn a_dead_main_server_falls_over_to_the_backup_with_one_notice() {
        let backup = fake_llm("backup", false, true).await;
        let cfg = cfg(DEAD, &backup.base_url); // fast AND balanced are dead: one machine
        let st = state(&cfg).await;
        let (res, streamed) = run(&st, &cfg, crate::vision::Route::Chat, "fast", user("hi", vec![])).await;
        let served = res.expect("the backup must answer");
        assert_eq!(served.slot_label, "deep");
        let text = &served.result.final_content;
        assert!(text.contains("answer from backup"), "{text}");
        assert!(
            text.starts_with("⚠️ _The **main** model isn't available right now. Answering with the backup model"),
            "{text}"
        );
        assert_eq!(text.matches("⚠️").count(), 1, "ONE notice, not one per skipped slot: {text}");
        assert_eq!(
            streamed.iter().filter(|c| c.contains("⚠️")).count(),
            1,
            "and it is shown live exactly once: {streamed:?}"
        );
        assert_eq!(backup.chat_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn announced_maintenance_switches_over_before_the_server_is_asked_anything() {
        let main = fake_llm("main", false, true).await; // perfectly healthy…
        let backup = fake_llm("backup", false, true).await;
        let cfg = cfg(&main.base_url, &backup.base_url);
        let st = state(&cfg).await;
        let auth = crate::availability::authority(&main.base_url);
        st.availability.lock().begin(&auth, chrono::Utc::now(), 30, "engine test");

        let (res, _) = run(&st, &cfg, crate::vision::Route::Chat, "fast", user("hi", vec![])).await;
        let served = res.expect("the backup must answer");
        assert_eq!(served.slot_label, "deep");
        let text = &served.result.final_content;
        assert!(text.contains("being serviced right now (planned maintenance"), "{text}");
        assert!(text.contains("back around"), "the end time is given: {text}");
        assert!(!text.contains("engine test"), "the caller's reason is not shown to the family: {text}");
        assert_eq!(main.chat_hits.load(Ordering::SeqCst), 0, "no request went to the server under maintenance");
        assert_eq!(main.health_hits.load(Ordering::SeqCst), 0, "not even a probe");
    }

    #[tokio::test]
    async fn the_slot_is_back_the_moment_maintenance_ends() {
        let main = fake_llm("main", false, true).await;
        let backup = fake_llm("backup", false, true).await;
        let cfg = cfg(&main.base_url, &backup.base_url);
        let st = state(&cfg).await;
        let auth = crate::availability::authority(&main.base_url);
        st.availability.lock().begin(&auth, chrono::Utc::now(), 30, "");
        assert!(matches!(slot_unavailable(&st, "fast").await, Some(Unavailable::Maintenance { .. })));
        assert!(matches!(slot_unavailable(&st, "balanced").await, Some(Unavailable::Maintenance { .. })));
        assert!(slot_unavailable(&st, "deep").await.is_none(), "another server is unaffected");

        assert_eq!(st.availability.lock().end(Some(&auth), chrono::Utc::now()), 1);
        assert_eq!(slot_unavailable(&st, "fast").await, None);
        let (res, _) = run(&st, &cfg, crate::vision::Route::Chat, "fast", user("hi", vec![])).await;
        let served = res.unwrap();
        assert_eq!(served.slot_label, "fast");
        assert!(!served.result.final_content.contains("⚠️"), "no notice when the main model answers");
    }

    #[tokio::test]
    async fn one_refused_probe_is_believed_for_both_slots_on_that_server() {
        let main = fake_llm("main", false, false).await; // /health says 502: engine down behind the gateway
        let backup = fake_llm("backup", false, true).await;
        let cfg = cfg(&main.base_url, &backup.base_url);
        let st = state(&cfg).await;
        assert!(!slot_alive_cached(&st, "fast").await);
        assert!(!slot_alive_cached(&st, "balanced").await, "same machine, same verdict");
        assert_eq!(
            main.health_hits.load(Ordering::SeqCst),
            1,
            "balanced was not probed again — fast's answer covers the whole server"
        );
    }

    #[tokio::test]
    async fn a_request_that_fails_marks_the_server_so_the_next_message_skips_it() {
        // Healthy on /health but the chat route is gone: the probe says alive, the request dies.
        let backup = fake_llm("backup", false, true).await;
        let cfg = cfg(DEAD, &backup.base_url);
        let st = state(&cfg).await;
        // Pretend an earlier probe said "alive" (the 15 s window), then the server died.
        st.slot_health.lock().insert("fast".into(), (std::time::Instant::now(), true));
        st.slot_health.lock().insert("balanced".into(), (std::time::Instant::now(), true));
        let (res, _) = run(&st, &cfg, crate::vision::Route::Chat, "fast", user("one", vec![])).await;
        assert_eq!(res.unwrap().slot_label, "deep");
        // The failed request is now remembered for the whole server…
        assert!(!slot_alive_cached(&st, "fast").await);
        assert!(!slot_alive_cached(&st, "balanced").await);
        // …so the next message goes straight to the backup.
        let (res, _) = run(&st, &cfg, crate::vision::Route::Chat, "balanced", user("two", vec![])).await;
        assert_eq!(res.unwrap().slot_label, "deep");
    }

    #[tokio::test]
    async fn when_no_home_server_answers_the_online_model_is_offered_never_used() {
        let online = fake_llm("online", false, true).await;
        let mut cfg = cfg(DEAD, "http://127.0.0.1:2");
        slot(&mut cfg.llm_online, &online.base_url, "online-model");
        let st = state(&cfg).await;
        let (res, streamed) = run(&st, &cfg, crate::vision::Route::Chat, "fast", user("hi", vec![])).await;
        let msg = res.err().expect("nothing at home answers").to_string();
        assert!(msg.contains("No model server is answering"), "{msg}");
        assert!(msg.contains("/online"), "how to pick the online model by hand: {msg}");
        assert!(msg.contains("leaves your home network"), "and what that means: {msg}");
        assert_eq!(online.chat_hits.load(Ordering::SeqCst), 0, "the online model was NOT used automatically");
        assert_eq!(online.health_hits.load(Ordering::SeqCst), 0);
        assert!(streamed.iter().all(|c| !c.contains("answer from")), "{streamed:?}");
    }

    #[tokio::test]
    async fn without_an_online_model_the_error_does_not_offer_one() {
        let cfg = cfg(DEAD, "http://127.0.0.1:2"); // online disabled
        let st = state(&cfg).await;
        let (res, _) = run(&st, &cfg, crate::vision::Route::Chat, "fast", user("hi", vec![])).await;
        let msg = res.err().unwrap().to_string();
        assert!(msg.contains("No model server is answering"), "{msg}");
        assert!(!msg.contains("/online"), "nothing to offer: {msg}");
    }

    #[tokio::test]
    async fn the_all_down_error_names_planned_maintenance_when_that_is_the_cause() {
        let backup = fake_llm("backup", false, true).await;
        let cfg = cfg(DEAD, &backup.base_url);
        let st = state(&cfg).await;
        // Both servers under maintenance.
        st.availability.lock().begin(&crate::availability::authority(DEAD), chrono::Utc::now(), 20, "");
        st.availability.lock().begin(&crate::availability::authority(&backup.base_url), chrono::Utc::now(), 20, "");
        let (res, _) = run(&st, &cfg, crate::vision::Route::Chat, "fast", user("hi", vec![])).await;
        let msg = res.err().unwrap().to_string();
        assert!(msg.contains("planned maintenance, back around"), "{msg}");
        assert_eq!(backup.chat_hits.load(Ordering::SeqCst), 0);
    }

    fn picture() -> Vec<String> {
        vec!["data:image/png;base64,iVBORw0KGgo=".to_string()]
    }

    #[tokio::test]
    async fn a_picture_goes_to_a_backup_that_can_read_it() {
        let backup = fake_llm("backup", true, true).await; // reports vision: true
        let cfg = cfg(DEAD, &backup.base_url);
        let st = state(&cfg).await;
        let (res, _) = run(&st, &cfg, crate::vision::Route::Chat, "fast", user("what is this?", picture())).await;
        let served = res.expect("the backup can see, so it answers");
        assert_eq!(served.slot_label, "deep");
        assert!(served.result.final_content.contains("answer from backup"));
        assert!(
            backup.last_body.lock().unwrap().contains("image_url"),
            "the picture itself reached the backup model"
        );
    }

    #[tokio::test]
    async fn a_picture_is_never_sent_to_a_backup_that_cannot_read_it() {
        let backup = fake_llm("backup", false, true).await; // vision: false
        let cfg = cfg(DEAD, &backup.base_url);
        let st = state(&cfg).await;
        let (res, _) = run(&st, &cfg, crate::vision::Route::Chat, "fast", user("what is this?", picture())).await;
        let msg = res.err().expect("must say so, not answer blind").to_string();
        assert!(msg.contains("No model server is answering"), "{msg}");
        assert!(msg.contains("can't read pictures"), "{msg}");
        assert!(msg.contains("send the picture again"), "{msg}");
        assert_eq!(backup.chat_hits.load(Ordering::SeqCst), 0, "no blind answer was attempted");
    }

    #[tokio::test]
    async fn a_picture_routed_to_a_dead_vision_endpoint_finds_a_backup_that_can_see() {
        let backup = fake_llm("backup", true, true).await;
        let mut cfg = cfg(DEAD, &backup.base_url);
        cfg.vision.enabled = true;
        cfg.vision.primary.base_url = DEAD.into();
        cfg.vision.primary.model = "glm-fast".into();
        cfg.vision.failover.base_url = DEAD.into();
        cfg.vision.failover.model = "glm-fast".into();
        let st = state(&cfg).await;
        let route = crate::vision::Route::Vision {
            primary: cfg.vision.primary.clone(),
            failover: Some(cfg.vision.failover.clone()),
        };
        let (res, _) = run(&st, &cfg, route, "fast", user("what is this?", picture())).await;
        let served = res.expect("the dead vision endpoint must not end the turn");
        assert_eq!(served.slot_label, "deep");
        assert!(served.result.final_content.contains("answer from backup"));
    }

    #[tokio::test]
    async fn maintenance_on_the_vision_endpoint_skips_it_without_a_request() {
        let main = fake_llm("main", true, true).await; // healthy and able to see…
        let backup = fake_llm("backup", true, true).await;
        let mut cfg = cfg(&main.base_url, &backup.base_url);
        cfg.vision.enabled = true;
        cfg.vision.primary.base_url = main.base_url.clone();
        cfg.vision.primary.model = "glm-fast".into();
        cfg.vision.failover.base_url = main.base_url.clone();
        cfg.vision.failover.model = "glm-fast".into();
        let st = state(&cfg).await;
        st.availability.lock().begin(&crate::availability::authority(&main.base_url), chrono::Utc::now(), 30, "");
        let route = crate::vision::Route::Vision {
            primary: cfg.vision.primary.clone(),
            failover: Some(cfg.vision.failover.clone()),
        };
        let (res, _) = run(&st, &cfg, route, "fast", user("what is this?", picture())).await;
        assert_eq!(res.unwrap().slot_label, "deep");
        assert_eq!(main.chat_hits.load(Ordering::SeqCst), 0, "the image reader under maintenance was not asked");
    }

    // ---- notice text and history hygiene (pure) ----

    #[test]
    fn the_notice_wording_names_who_was_aimed_at_and_who_answers() {
        let mut s = LlmSettings::default();
        s.model = "qwen3.8-27b-uncensored".into();
        let down = failover_notice("fast", &Unavailable::Down, "deep", &s);
        assert!(down.starts_with("⚠️ _The **main** model isn't available right now."), "{down}");
        assert!(down.contains("**uncensored** (`qwen3.8-27b-uncensored`)"), "{down}");
        assert!(down.ends_with("instead._\n\n"), "{down}");
        let balanced = failover_notice("balanced", &Unavailable::Down, "deep", &s);
        assert!(balanced.contains("The **balanced** model"), "{balanced}");
        let planned = failover_notice(
            "fast",
            &Unavailable::Maintenance { until: chrono::Utc::now() + chrono::Duration::minutes(20) },
            "deep",
            &s,
        );
        assert!(planned.contains("planned maintenance, back around"), "{planned}");
    }

    #[test]
    fn a_notice_is_stripped_from_history_but_nothing_else_is() {
        let mut s = LlmSettings::default();
        s.model = "m".into();
        let notice = failover_notice("fast", &Unavailable::Down, "deep", &s);
        let answered = format!("{notice}The capital is Paris.");
        assert_eq!(strip_failover_notice(&answered), "The capital is Paris.");
        // Stacked notices (older builds wrote one per hop) all go.
        let stacked = format!("{notice}{notice}Done.");
        assert_eq!(strip_failover_notice(&stacked), "Done.");
        // The pre-0.2.146 wording is recognised too.
        let old = "⚠️ _The **fast** model isn't responding (server unreachable) — answering with **balanced** (`glm-balanced`) instead._\n\nHello";
        assert_eq!(strip_failover_notice(old), "Hello");
        // Text that merely mentions the words is untouched.
        for plain in ["The main model isn't available, I said.", "⚠️ careful", "", "⚠️ _The **x** note"] {
            assert_eq!(strip_failover_notice(plain), plain);
        }
    }

    #[test]
    fn the_all_down_message_always_starts_with_the_passthrough_phrase() {
        let tried = vec!["fast".to_string(), "balanced".to_string(), "deep".to_string()];
        for (until, online, sight) in [(None, false, false), (None, true, false), (Some(chrono::Utc::now()), true, false), (None, true, true)] {
            let m = all_down_message(&tried, until, online, sight);
            assert!(m.contains("No model server is answering"), "{m}");
            assert!(m.contains("tried: fast, balanced, deep"), "{m}");
        }
        assert!(all_down_message(&tried, None, true, false).contains("/online"));
        assert!(!all_down_message(&tried, None, false, false).contains("/online"));
        assert!(!all_down_message(&tried, None, true, true).contains("/online"), "a picture can't be offered to a model that can't see");
    }
}
