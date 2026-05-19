//! Route incoming Telegram updates into KinAI's chat pipeline.
//!
//! Four cases per inbound message:
//!
//!   1. `/start <token>` — first-time pairing handshake. Redeem the
//!      pending pair → permanent link → confirmation message.
//!
//!   2. `/start` (no arg) — friendly welcome explaining how to pair
//!      from KinAI Settings.
//!
//!   3. Message from an UNpaired chat — short reply telling the user
//!      they need to pair first.
//!
//!   4. Message from a paired chat — find the dedicated Telegram
//!      thread for that peer, run the chat turn (slash commands work
//!      identically to KinAI's WS path), send the assistant reply
//!      back via sendMessage / sendPhoto.

use anyhow::Result;
use tauri::{AppHandle, Runtime};

use crate::db::telegram as tg_db;
use crate::SharedState;

use super::api::{BotApi, TelegramMessage, TelegramUpdate};

pub async fn handle_update<R: Runtime>(
    api: &BotApi,
    state: &SharedState,
    app: &AppHandle<R>,
    update: &TelegramUpdate,
) -> Result<()> {
    let Some(msg) = &update.message else {
        return Ok(()); // non-message updates filtered upstream; defensive guard
    };
    let chat_id = msg.chat.id;
    let text_or_caption = msg
        .text
        .clone()
        .or_else(|| msg.caption.clone())
        .unwrap_or_default();

    // /start handshake
    if let Some(rest) = text_or_caption.strip_prefix("/start") {
        let arg = rest.trim();
        return handle_start(api, state, chat_id, msg, arg).await;
    }

    // Anything else routes through the chat pipeline IF paired.
    let Some(peer_id) = tg_db::peer_for_chat(&state.db.pool, &chat_id.to_string()).await? else {
        api.send_message(
            chat_id,
            "👋 Hi! I'm KinAI's family bot, but this Telegram isn't linked to a KinAI account yet.\n\n\
             To pair: open KinAI on your computer → Settings → Telegram → \"Connect Telegram\" \
             → scan the QR code shown there.",
        )
        .await?;
        return Ok(());
    };

    // Routed — run the chat turn.
    let _ = app; // future: app.emit to fan-out paired peer's UI
    if let Err(e) = run_turn_for_peer(api, state, chat_id, &peer_id, &text_or_caption, msg)
        .await
    {
        tracing::warn!("telegram run_turn: {e:?}");
        let _ = api
            .send_message(
                chat_id,
                &format!("Something went wrong on KinAI's end: {e}"),
            )
            .await;
    }
    Ok(())
}

async fn handle_start(
    api: &BotApi,
    state: &SharedState,
    chat_id: i64,
    msg: &TelegramMessage,
    arg: &str,
) -> Result<()> {
    if arg.is_empty() {
        api.send_message(
            chat_id,
            "👋 Hi! I'm a family bot for KinAI — a private local AI shared between household devices.\n\n\
             To connect this Telegram to your KinAI account, open KinAI on a Mac or Windows PC, \
             go to Settings → Telegram, click \"Connect Telegram\", then scan the QR code with your \
             phone's camera.",
        )
        .await?;
        return Ok(());
    }

    let username = msg.from.as_ref().and_then(|u| u.username.as_deref());
    let first_name = msg.from.as_ref().map(|u| u.first_name.as_str()).filter(|s| !s.is_empty());

    match tg_db::redeem_pair(
        &state.db.pool,
        arg,
        &chat_id.to_string(),
        username,
        first_name,
    )
    .await?
    {
        Some(peer_id) => {
            tracing::info!("telegram: paired chat {chat_id} → peer {peer_id}");
            api.send_message(
                chat_id,
                "✅ Linked to KinAI.\n\nTry:\n• `/help` — list commands\n• `/pic a sunset over Miami` — generate an image\n• or just type any question.",
            )
            .await?;
        }
        None => {
            api.send_message(
                chat_id,
                "❌ That pairing code was invalid or expired (codes last 10 minutes). \
                 Generate a fresh QR code in KinAI Settings → Telegram and try again.",
            )
            .await?;
        }
    }
    Ok(())
}

/// Run a chat turn for `peer_id` on the dedicated Telegram thread,
/// send the reply back via Telegram, and also persist + emit
/// the messages so any open KinAI client for that peer sees the
/// exchange in real time.
async fn run_turn_for_peer(
    api: &BotApi,
    state: &SharedState,
    chat_id: i64,
    peer_id: &str,
    content: &str,
    _msg: &TelegramMessage,
) -> Result<()> {
    if content.trim().is_empty() {
        return Ok(()); // ignore stickers / unknown payloads for v1
    }

    // Ensure a dedicated thread exists for this peer's Telegram convo.
    // The id is deterministic so the same chat always lands in the
    // same thread (across host restarts).
    let thread_id = telegram_thread_id_for_peer(peer_id);
    state
        .db
        .upsert_thread(peer_id, &thread_id, "Telegram")
        .await
        .ok();

    // Persist user message.
    let sender = "Telegram".to_string();
    let user_msg = state
        .db
        .append_message(&thread_id, "user", &sender, content, &[])
        .await?;
    let _ = user_msg; // currently no UI fan-out for Telegram-originated user msgs

    let cfg = state.config.read().clone();

    // Slash commands intercept BEFORE the LLM.
    if let Some(reply) = crate::slash::handle(&cfg, content).await {
        send_assistant_reply(api, state, &thread_id, chat_id, &reply).await?;
        return Ok(());
    }

    // Regular chat — build context, run the LLM, capture the final
    // assistant content. This duplicates a chunk of run_chat_turn from
    // network::server because we don't have a client mpsc to fan-out
    // streaming tokens to. Future cleanup: extract the shared core.
    let messages = crate::context::builder::build_context(
        &state.db,
        &cfg,
        peer_id,
        &thread_id,
        &state
            .db
            .append_message(&thread_id, "user", &sender, "", &[])
            .await
            .ok()
            .as_ref()
            .map(|m| crate::db::Message {
                id: m.id.clone(),
                thread_id: m.thread_id.clone(),
                role: m.role.clone(),
                sender: m.sender.clone(),
                content: content.to_string(),
                attachments: vec![],
                created_at: m.created_at.clone(),
                summarized_into: None,
                metrics: None,
            })
            .unwrap_or(crate::db::Message {
                id: String::new(),
                thread_id: thread_id.clone(),
                role: "user".into(),
                sender: sender.clone(),
                content: content.to_string(),
                attachments: vec![],
                created_at: chrono::Utc::now().to_rfc3339(),
                summarized_into: None,
                metrics: None,
            }),
    )
    .await?;
    let _ = messages; // placeholder; full LLM path completed in next milestone

    // For this first slice we only handle slash commands end-to-end;
    // non-slash messages will get a friendly placeholder until the
    // LLM-path refactor lands in the next commit.
    api.send_message(
        chat_id,
        "🚧 Plain-text chat through Telegram is coming in the next update — for now I can only \
        handle slash commands like `/pic`, `/picHQ`, `/help`. Try one of those!",
    )
    .await?;
    Ok(())
}

/// Send the assistant reply back to Telegram. For slash-command
/// replies that contain a `![alt](url)` image reference, we ALSO
/// upload the underlying file as a Telegram photo so users see the
/// image inline in their chat rather than just a clickable link.
async fn send_assistant_reply(
    api: &BotApi,
    state: &SharedState,
    thread_id: &str,
    chat_id: i64,
    reply: &str,
) -> Result<()> {
    // Persist the assistant message so it shows up in KinAI clients
    // viewing this peer's Telegram thread.
    state
        .db
        .append_message(thread_id, "assistant", "KinAI", reply, &[])
        .await
        .ok();

    // Detect a `![alt](http://host/v1/pic/<uuid>.png)` from ComfyUI
    // output. When found, upload the local file as a photo and send
    // the rest of the text as a separate message.
    if let Some(local_path) = extract_local_pic_path(state, reply) {
        let caption = strip_inline_image_markdown(reply);
        let caption_opt = if caption.trim().is_empty() { None } else { Some(caption.as_str()) };
        api.send_photo_file(chat_id, &local_path, caption_opt).await?;
        return Ok(());
    }

    api.send_message(chat_id, reply).await?;
    Ok(())
}

/// If `reply` contains `![…](http://<our-host>/v1/pic/<uuid>.<ext>)`,
/// return the absolute filesystem path to that image. None if the
/// reply has no such reference.
fn extract_local_pic_path(state: &SharedState, reply: &str) -> Option<std::path::PathBuf> {
    let re = regex::Regex::new(r"!\[[^\]]*\]\(http[s]?://[^/]+/v1/pic/([A-Za-z0-9_\-]+\.[a-z]+)\)").ok()?;
    let caps = re.captures(reply)?;
    let filename = caps.get(1)?.as_str();
    let dir = crate::comfyui::pics_dir();
    let path = dir.join(filename);
    if path.exists() {
        let _ = state; // unused; future: lookup peer-specific dir
        Some(path)
    } else {
        None
    }
}

fn strip_inline_image_markdown(reply: &str) -> String {
    let re = match regex::Regex::new(r"!\[[^\]]*\]\(http[s]?://[^)]+\)\s*\n*") {
        Ok(r) => r,
        Err(_) => return reply.to_string(),
    };
    re.replace_all(reply, "").trim().to_string()
}

/// Deterministic thread id for `<peer>`'s Telegram conversation. UUID
/// shape so the host's threads table accepts it; first segment encodes
/// the peer so it's easy to debug (`grep telegram-` against the DB).
fn telegram_thread_id_for_peer(peer_id: &str) -> String {
    // We can't put `peer_id` directly into a UUID slot, so fold it
    // into a stable hash and format as a UUID-shaped string.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    "telegram".hash(&mut h);
    peer_id.hash(&mut h);
    let lo = h.finish();
    "telegram".hash(&mut h);
    peer_id.hash(&mut h);
    let hi = h.finish();
    format!(
        "telegram-{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (hi >> 32) as u32,
        ((hi >> 16) & 0xffff) as u16,
        (hi & 0xffff) as u16,
        ((lo >> 48) & 0xffff) as u16,
        lo & 0xffff_ffff_ffff
    )
}
