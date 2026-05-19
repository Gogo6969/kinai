//! KinAI → Telegram echo.
//!
//! When the assistant generates a reply on a *peer's Telegram thread*
//! (the deterministic thread id created by `router::telegram_thread_id_for_peer`),
//! we mirror that reply to Telegram so the conversation stays in sync
//! on the user's phone — not just inside KinAI's UI.
//!
//! Directionality recap:
//!   - Telegram → KinAI: `router.rs` persists the user's Telegram
//!     message into the same thread and runs the LLM. KinAI clients
//!     viewing that thread see it live.
//!   - KinAI → Telegram: **this module.** Called from the host-side
//!     chat paths (`commands::send_message` for host chats,
//!     `network::server::run_chat_turn` for client peer chats) once
//!     the assistant reply has been persisted. If the active thread is
//!     a Telegram thread for the originating peer, push the reply to
//!     the user's Telegram chat.
//!
//! Best-effort: if the bot token is missing, the chat_id lookup
//! returns None, or the Telegram API call fails, we just log and move
//! on — the in-KinAI flow already succeeded.

use crate::SharedState;

use super::router::{
    extract_local_pic_path, strip_inline_image_markdown, telegram_thread_id_for_peer,
};

/// If `thread_id` is `peer_id`'s Telegram thread AND the peer has a
/// paired Telegram chat, push `reply` to that chat via the bot. Returns
/// quickly when any precondition isn't met — safe to call after every
/// assistant message.
///
/// The function never returns an error: Telegram-echo failures are
/// logged but mustn't taint the in-app chat path.
pub async fn maybe_echo_assistant(
    state: &SharedState,
    peer_id: &str,
    thread_id: &str,
    reply: &str,
) {
    // Fast paths first — none of these need to touch the network.
    if reply.trim().is_empty() {
        return;
    }
    if thread_id != telegram_thread_id_for_peer(peer_id) {
        return; // not a Telegram thread, nothing to echo
    }
    let token = state.config.read().telegram.bot_token.clone();
    if token.trim().is_empty() {
        return; // bot disabled
    }
    let chat_id = match crate::db::telegram::chat_for_peer(&state.db.pool, peer_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return, // peer hasn't paired their Telegram
        Err(e) => {
            tracing::warn!("telegram echo: chat_for_peer({peer_id}) failed: {e:?}");
            return;
        }
    };
    let chat_id_i64: i64 = match chat_id.parse() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("telegram echo: invalid chat_id {chat_id:?}: {e}");
            return;
        }
    };
    let api = super::api::BotApi::new(token);

    // Same image-detection trick the router uses for the reverse
    // direction: if the reply contains a `![](.../v1/pic/<file>)`
    // markdown reference, upload the underlying file as a Telegram
    // photo (with the rest of the reply as the caption) so the user
    // sees the image inline instead of just a link.
    if let Some(local_path) = extract_local_pic_path(state, reply) {
        let caption = strip_inline_image_markdown(reply);
        let caption_opt = if caption.trim().is_empty() {
            None
        } else {
            Some(caption.as_str())
        };
        if let Err(e) = api.send_photo_file(chat_id_i64, &local_path, caption_opt).await {
            tracing::warn!("telegram echo: send_photo_file failed: {e:?}");
        }
        return;
    }

    if let Err(e) = api.send_message(chat_id_i64, reply).await {
        tracing::warn!("telegram echo: send_message failed: {e:?}");
    }
}

/// Mirror a user message typed inside KinAI to Telegram, prefixed so
/// the chat history makes sense to the human reader.
///
/// Telegram bots can only send messages *as themselves* — they can't
/// impersonate the human user — so the user's KinAI-typed input would
/// otherwise look indistinguishable from the bot replying to nothing.
/// We prefix with `💬 You:` to make the speaker explicit. Users reading
/// the Telegram chat then see:
///
///   • their original Telegram input (sent by them, right-aligned),
///   • bot-as-themselves echo when they typed from KinAI ("💬 You: …"),
///   • bot reply (no prefix), same as before.
///
/// Same gating rules as `maybe_echo_assistant`: only fires on Telegram
/// threads for a paired peer; all failures are best-effort + logged.
pub async fn maybe_echo_user(
    state: &SharedState,
    peer_id: &str,
    thread_id: &str,
    user_sender: &str,
    content: &str,
) {
    if content.trim().is_empty() {
        return;
    }
    if thread_id != telegram_thread_id_for_peer(peer_id) {
        return;
    }
    // Skip when the originating channel WAS Telegram — the user just
    // typed this on their phone, no need to bounce it back to themselves.
    // Router persists Telegram-originated user messages with
    // sender = "Telegram" (see router::run_turn_for_peer).
    if user_sender == "Telegram" {
        return;
    }
    let token = state.config.read().telegram.bot_token.clone();
    if token.trim().is_empty() {
        return;
    }
    let chat_id = match crate::db::telegram::chat_for_peer(&state.db.pool, peer_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("telegram echo: chat_for_peer({peer_id}) failed: {e:?}");
            return;
        }
    };
    let chat_id_i64: i64 = match chat_id.parse() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("telegram echo: invalid chat_id {chat_id:?}: {e}");
            return;
        }
    };
    let api = super::api::BotApi::new(token);
    let body = format!("💬 You: {content}");
    if let Err(e) = api.send_message(chat_id_i64, &body).await {
        tracing::warn!("telegram echo: send_message (user) failed: {e:?}");
    }
}
