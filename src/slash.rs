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
pub const SLOTS: &[&str] = &["fast", "balanced", "deep"];

/// Settings for a slot label ("fast" / "balanced" / "deep").
pub fn slot_settings<'cfg>(cfg: &'cfg AppConfig, label: &str) -> &'cfg LlmSettings {
    match label {
        "deep" => &cfg.llm_deep,
        "balanced" => &cfg.llm_balanced,
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

/// Run a chat turn on the routed slot; when that slot's SERVER is down
/// (connection refused, DNS/timeout, 503-loading, stall watchdog —
/// `llm::is_server_down_error`), automatically retry on the next active
/// slot in SLOTS order with a user-visible notice, instead of dying
/// silently. Tries each active slot at most once; when every slot is
/// down, returns one honest, actionable error for all surfaces.
///
/// Vision routes pass through untouched — they have their own
/// endpoint-level failover inside `vision::run_with_route`.
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

    if matches!(route, crate::vision::Route::Vision { .. }) {
        let settings = slot_settings(cfg, routed_slot).clone();
        let llm = crate::llm::LlmClient::new(settings.clone());
        let max_tokens = max_tokens_for(&settings, &messages);
        let result = crate::vision::run_with_route(
            route,
            llm,
            &settings,
            messages,
            tools,
            tool_runtime,
            max_tokens,
            handlers,
            cancel,
        )
        .await?;
        return Ok(ServedTurn {
            slot_label: routed_slot.to_string(),
            settings,
            result,
        });
    }

    let next_untried = |tried: &[String]| {
        SLOTS
            .iter()
            .find(|s| !tried.iter().any(|t| t == **s) && slot_settings(cfg, s).is_active())
            .copied()
    };
    let all_down_error = |tried: &[String]| {
        anyhow::anyhow!(
            "🔌 No model server is answering right now (tried: {}). \
Check that your model server(s) are running — Settings → Models shows each \
slot's address — then try again.",
            tried.join(", ")
        )
    };

    let mut label = routed_slot.to_string();
    let mut notice = String::new();
    let mut tried: Vec<String> = Vec::new();
    loop {
        tried.push(label.clone());
        let settings = slot_settings(cfg, &label).clone();

        // Known-dead skip: the cached probe is bounded at 1.5s, versus a
        // full connect-timeout of silent thinking-dots per dead attempt
        // (SYN-dropping servers: host asleep, firewall drop). A stale
        // "alive" cache entry just falls through to the real attempt,
        // whose own error handling remains the safety net.
        if !slot_alive_cached(state, &label).await {
            let Some(next) = next_untried(&tried) else {
                return Err(all_down_error(&tried));
            };
            let next_model = slot_settings(cfg, next).model.clone();
            tracing::warn!("slot '{label}' failed liveness probe; skipping to '{next}'");
            let line = format!(
                "⚠️ _The **{label}** model isn't responding (server unreachable) — \
answering with **{next}** (`{next_model}`) instead._\n\n"
            );
            (handlers.on_token)(line.clone());
            notice.push_str(&line);
            label = next.to_string();
            continue;
        }

        // Re-trim the prompt for THIS slot's window. The context was
        // built against the ROUTED slot; replaying a 32k-window prompt
        // into an 8k-window failover slot would overflow its context and
        // drive computed max_tokens to zero.
        let mut attempt_msgs = messages.clone();
        crate::context::token_guard::trim_to_fit(
            &mut attempt_msgs,
            crate::context::builder::prompt_budget(settings.context_window, settings.max_tokens),
        );
        let max_tokens = max_tokens_for(&settings, &attempt_msgs);

        // Tool side effects aren't idempotent (memory writes, metered
        // web search). Track whether THIS attempt ran any tool; if the
        // server dies after tools already executed, we surface an honest
        // retry request instead of silently re-running the whole turn.
        let tools_ran = std::sync::Arc::new(AtomicBool::new(false));
        let attempt_handlers = {
            let inner = handlers.on_tool.clone();
            let flag = tools_ran.clone();
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

        let llm = crate::llm::LlmClient::new(settings.clone());
        let attempt = crate::vision::run_with_route(
            crate::vision::Route::Chat,
            llm,
            &settings,
            attempt_msgs,
            tools.clone(),
            tool_runtime.clone(),
            max_tokens,
            attempt_handlers,
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
                let Some(next) = next_untried(&tried) else {
                    return Err(all_down_error(&tried));
                };
                let reason = crate::llm::short_server_down_reason(&msg);
                let next_model = slot_settings(cfg, next).model.clone();
                tracing::warn!("slot '{label}' down ({msg}); failing over to '{next}'");
                let line = format!(
                    "⚠️ _The **{label}** model isn't responding ({reason}) — \
answering with **{next}** (`{next_model}`) instead._\n\n"
                );
                (handlers.on_token)(line.clone());
                notice.push_str(&line);
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

/// Cached liveness for a slot's model server (15s TTL). Cheap enough
/// for menus and switch confirmations without hammering the servers;
/// stale by at most the TTL, and the turn-level failover is the safety
/// net when a server dies inside that window. Inactive slots are always
/// `false`. Never holds the config lock across the probe await.
pub async fn slot_alive_cached(state: &crate::AppState, label: &str) -> bool {
    const TTL: std::time::Duration = std::time::Duration::from_secs(15);
    let settings = {
        let cfg = state.config.read();
        let s = slot_settings(&cfg, label);
        if !s.is_active() {
            return false;
        }
        s.clone()
    };
    if let Some((probed_at, alive)) = state.slot_health.lock().get(label).copied() {
        if probed_at.elapsed() < TTL {
            return alive;
        }
    }
    let alive = crate::llm::detect::probe_alive(&settings).await;
    state
        .slot_health
        .lock()
        .insert(label.to_string(), (std::time::Instant::now(), alive));
    alive
}

/// The slot to actually serve a request aimed at `wanted`: the wanted
/// slot when active, else the first active slot in SLOTS order, else
/// `wanted` itself (fail loudly downstream — a config problem should
/// surface, not silently pick a paused model).
fn effective_slot<'cfg>(cfg: &'cfg AppConfig, wanted: &'static str) -> (&'static str, &'cfg LlmSettings) {
    if slot_settings(cfg, wanted).is_active() {
        return (wanted, slot_settings(cfg, wanted));
    }
    for s in SLOTS {
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
pub fn switch_confirmation(
    cfg: &AppConfig,
    route: &ResolvedRoute,
    wanted_alive: Option<bool>,
) -> String {
    let label = route.slot_label;
    let model = &route.settings.model;
    let others: Vec<String> = SLOTS
        .iter()
        .filter(|s| **s != label && slot_settings(cfg, s).is_active())
        .map(|s| format!("`/{s}`"))
        .collect();
    let mut msg = if others.is_empty() {
        format!("Switched to the **{label}** model (`{model}`).")
    } else {
        format!(
            "Switched to the **{label}** model (`{model}`). It stays active for this conversation until you switch again with {}.",
            others.join(" or ")
        )
    };
    // Honest heads-up when the wanted slot's server failed its liveness
    // probe — the switch still sticks (the wish is recorded), but the
    // user learns NOW rather than after their next message fails over.
    if wanted_alive == Some(false) {
        if others.is_empty() {
            // Don't promise a failover that can't happen.
            msg.push_str(
                "\n\n⚠️ Heads-up: this model's server isn't responding right now, \
and no other model is configured — messages will fail until it's back.",
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
                "balanced" => format!("`/balanced` — the middle ground (`{m}`): smarter than fast, quicker than deep\n"),
                _ => format!("`/deep` — the reasoning model (`{m}`), slower but highest quality\n"),
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
        out.push_str("`/voice` — toggle spoken replies for the chat you're in: in the KinAI app replies are read aloud on this Mac, in Telegram they arrive as voice notes; `/voice on` / `/voice off` set it explicitly\n");
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
                "balanced" => format!("<code>/balanced</code> — the middle ground (<code>{m}</code>): smarter than fast, quicker than deep\n"),
                _ => format!("<code>/deep</code> — the reasoning model (<code>{m}</code>), slower but highest quality\n"),
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
        out.push_str("<code>/voice</code> — toggle spoken replies for the chat you're in: in the KinAI app replies are read aloud on this Mac, in Telegram they arrive as voice notes; <code>/voice on</code> / <code>/voice off</code> set it explicitly\n");
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

    async fn fresh_db() -> crate::db::Db {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        crate::db::migrate::run(&pool).await.expect("migrations");
        crate::db::Db { pool }
    }

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

    #[test]
    fn server_down_classification() {
        use crate::llm::is_server_down_error;
        // The three real dead-slot shapes seen in production logs.
        assert!(is_server_down_error(
            "error sending request for url (http://192.168.1.91:8081/v1/chat/completions)"
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
        assert!(md.contains("/fast") && md.contains("/balanced") && md.contains("/deep"));
        assert!(md.contains("balanced-33b"));
        let html = help_html(&cfg);
        assert!(html.contains("/balanced") && html.contains("balanced-33b"));
    }

    #[test]
    fn help_hides_models_section_with_one_slot() {
        let cfg = AppConfig::default(); // only fast active
        let md = help_markdown(&cfg);
        assert!(!md.contains("/balanced") && !md.contains("**Models**"));
    }
}
