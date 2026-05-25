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
pub async fn route_for<'cfg>(
    db: &crate::db::Db,
    cfg: &'cfg AppConfig,
    peer_id: &str,
    thread_id: &str,
    content: &str,
) -> ResolvedRoute<'cfg> {
    let lower = content.trim_start().to_ascii_lowercase();
    if lower.starts_with("/deep ") || lower.starts_with("/deep\n") || lower == "/deep" {
        let stripped = strip_prefix(content, "/deep");
        let settings = if cfg.llm_deep.is_active() {
            &cfg.llm_deep
        } else if cfg.llm.is_active() {
            &cfg.llm
        } else {
            &cfg.llm_deep
        };
        // Persist the switch on the thread row. Best-effort: a DB
        // failure shouldn't block the user's question.
        let _ = db.set_thread_active_slot(peer_id, thread_id, Some("deep")).await;
        return ResolvedRoute { settings, stripped_content: stripped, slot_label: "deep" };
    }
    if lower.starts_with("/fast ") || lower.starts_with("/fast\n") || lower == "/fast" {
        let stripped = strip_prefix(content, "/fast");
        let settings = if cfg.llm.is_active() {
            &cfg.llm
        } else if cfg.llm_deep.is_active() {
            &cfg.llm_deep
        } else {
            &cfg.llm
        };
        let _ = db.set_thread_active_slot(peer_id, thread_id, Some("fast")).await;
        return ResolvedRoute { settings, stripped_content: stripped, slot_label: "fast" };
    }
    // No prefix — consult the thread's sticky slot first. If the
    // user previously typed `/deep` in this thread, keep routing
    // there. Bad/unknown values fall through to the global default.
    let sticky = db
        .thread_active_slot(peer_id, thread_id)
        .await
        .ok()
        .flatten();
    match sticky.as_deref() {
        Some("deep") if cfg.llm_deep.is_active() => {
            return ResolvedRoute {
                settings: &cfg.llm_deep,
                stripped_content: content.to_string(),
                slot_label: "deep",
            };
        }
        Some("fast") if cfg.llm.is_active() => {
            return ResolvedRoute {
                settings: &cfg.llm,
                stripped_content: content.to_string(),
                slot_label: "fast",
            };
        }
        _ => {}
    }
    // Global default — prefer the fast slot; fall back to deep if
    // fast is paused or empty. (Mirrors `cfg.llm` being the
    // long-standing default; users who only configure the deep slot
    // still get routed there for plain-text messages.)
    let settings = if cfg.llm.is_active() {
        &cfg.llm
    } else if cfg.llm_deep.is_active() {
        &cfg.llm_deep
    } else {
        &cfg.llm
    };
    ResolvedRoute { settings, stripped_content: content.to_string(), slot_label: "fast" }
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
                "**Image generation isn't configured on this host.**\n\nThe host owner can enable it in **Settings → Image generation** by pointing it at a ComfyUI server (e.g. `http://192.168.1.25:8188`).".into()
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
    let fast_on = cfg.llm.is_active();
    let deep_on = cfg.llm_deep.is_active();
    let mut out = String::from("**KinAI Commands**\n");

    if fast_on && deep_on {
        out.push_str("\n**Models**\n");
        out.push_str(&format!(
            "`/fast` — route this turn to the fast model (`{}`)\n",
            cfg.llm.model
        ));
        out.push_str(&format!(
            "`/deep` — route this turn to the deep model (`{}`), slower but higher quality\n",
            cfg.llm_deep.model
        ));
    }

    out.push_str("\n**Image generation**\n");
    if comfy_on {
        out.push_str("`/pic [WxH] <prompt>` — Z-Image Turbo (fast, ~5s, default 1280×720)\n");
        out.push_str("`/picHQ [WxH] <prompt>` — Z-Image Base HQ (slower, ~30s, default 1024×1024)\n");
        out.push_str("Optional `WxH` overrides the size, e.g. `/picHQ 1280x720 a sunset over Miami` — any size 64×64 to 2048×2048.\n");
    } else {
        out.push_str("*(image generation not configured on this host — ask the host owner to set a ComfyUI URL in Settings → Image generation)*\n");
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
    let fast_on = cfg.llm.is_active();
    let deep_on = cfg.llm_deep.is_active();
    let esc = telegram_html_escape;
    let mut out = String::from("<b>KinAI Commands</b>\n");

    if fast_on && deep_on {
        out.push_str("\n<b>Models</b>\n");
        out.push_str(&format!(
            "<code>/fast</code> — route this turn to the fast model (<code>{}</code>)\n",
            esc(&cfg.llm.model)
        ));
        out.push_str(&format!(
            "<code>/deep</code> — route this turn to the deep model (<code>{}</code>), slower but higher quality\n",
            esc(&cfg.llm_deep.model)
        ));
    }

    out.push_str("\n<b>Image generation</b>\n");
    if comfy_on {
        out.push_str("<code>/pic [WxH] &lt;prompt&gt;</code> — Z-Image Turbo (fast, ~5s, default 1280×720)\n");
        out.push_str("<code>/picHQ [WxH] &lt;prompt&gt;</code> — Z-Image Base HQ (slower, ~30s, default 1024×1024)\n");
        out.push_str("Optional <code>WxH</code> overrides the size, e.g. <code>/picHQ 1280x720 a sunset over Miami</code> — any size 64×64 to 2048×2048.\n");
    } else {
        out.push_str("<i>(image generation not configured on this host — ask the host owner to set a ComfyUI URL in Settings → Image generation)</i>\n");
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
