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
///     when active (paused / unconfigured slots fall back).
///   * No prefix → fast slot first (it's the existing default),
///     deep slot otherwise.
///   * Neither active → still return the fast slot (the LLM call
///     will fail loudly downstream, which is the right UX — it
///     surfaces a config problem instead of silently picking a
///     paused model).
///
/// The stripped content has the prefix removed AND its single
/// trailing/leading space trimmed once; everything else (newlines,
/// formatting) is preserved verbatim.
pub fn route_for<'cfg>(cfg: &'cfg AppConfig, content: &str) -> ResolvedRoute<'cfg> {
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
        return ResolvedRoute { settings, stripped_content: stripped, slot_label: "fast" };
    }
    // No prefix — prefer the fast slot; fall back to deep if fast is
    // paused or empty. (Mirrors `cfg.llm` being the long-standing
    // default; users who only configure the deep slot still get
    // routed there for plain-text messages.)
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
        let comfy_on = crate::comfyui::is_configured(&cfg.comfyui.base_url);
        let fast_on = cfg.llm.is_active();
        let deep_on = cfg.llm_deep.is_active();
        let mut lines: Vec<String> = vec![
            "**Available slash commands**".into(),
            "".into(),
        ];
        // Model-selector slashes show up only when there's an actual
        // choice to make — a single-model setup hides them so the
        // /help output isn't cluttered with redundant routing.
        if fast_on && deep_on {
            lines.push(format!(
                "- `/fast <prompt>` — route this turn to the **fast** model (`{}`).",
                cfg.llm.model
            ));
            lines.push(format!(
                "- `/deep <prompt>` — route this turn to the **deep** model (`{}`). Slower but typically higher quality.",
                cfg.llm_deep.model
            ));
        }
        if comfy_on {
            lines.push("- `/pic <prompt>` — generate an image (fast, ~5s, default 1280×720)".into());
            lines.push("- `/picHQ <prompt>` — higher-quality image (slower, ~30s, default 1024×1024)".into());
            lines.push("- Add an optional `WxH` prefix to override the size, e.g. `/pic 1024x1024 a sunset over Miami` or `/picHQ 1280x720 a sunset over Miami` — any size from 64×64 to 2048×2048.".into());
        } else {
            lines.push("- `/pic`, `/picHQ` — *(image generation not configured on this host — ask the host owner to set a ComfyUI URL in Settings → Image generation)*".into());
        }
        lines.push("- `/help` or `?` — show this list".into());
        return Some(lines.join("\n"));
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
