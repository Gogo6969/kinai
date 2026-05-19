//! KinAI → Telegram echo.
//!
//! When the user types a question inside KinAI on a peer's Telegram
//! thread (the deterministic id from `router::telegram_thread_id_for_peer`),
//! we mirror the **whole Q&A turn** to Telegram so the conversation
//! stays in sync on the user's phone.
//!
//! Why combined and not two separate messages: Telegram bots can only
//! send messages *as themselves* — they can't impersonate the human
//! user. Sending the question as one bot message and the answer as a
//! second bot message makes the question look like the bot talking to
//! itself. Instead we wrap the question in a Telegram `<blockquote>`
//! (HTML parse mode) and put the answer right after, so the phone chat
//! sees ONE bot message structured as "quote of your question → bot's
//! reply". That mirrors the visual pattern Telegram users already
//! recognise from forwarded messages and reply previews.
//!
//! Direction recap:
//!   - Telegram → KinAI: handled by `router.rs`. The router persists
//!     the user's Telegram message with sender = "Telegram"; the user
//!     already typed it on Telegram so there's no point echoing it
//!     back to themselves. The reply goes via `send_assistant_reply`.
//!   - KinAI → Telegram: **this module.** Called from the two chat
//!     paths (`commands::send_message` for host chats,
//!     `network::server::run_chat_turn` for client peer chats):
//!       * `maybe_show_typing` before the LLM runs (UX nicety —
//!         shows "Bot is typing…" on the phone)
//!       * `maybe_echo_qa` after the LLM finishes, with both the
//!         user's question text and the assistant reply.
//!
//! Best-effort throughout: bad token / missing pairing / Telegram
//! network errors are logged and swallowed — the in-KinAI flow always
//! commits before we attempt to mirror.

use crate::SharedState;

use super::router::{
    extract_local_pic_path, strip_inline_image_markdown, telegram_thread_id_for_peer,
};

/// Send a one-shot "typing…" indicator to the user's Telegram chat.
/// Lasts ~5s on Telegram's side; we send it once at the start of the
/// turn so the phone user gets immediate feedback that something is
/// happening on the KinAI side even before the LLM produces tokens.
pub async fn maybe_show_typing(state: &SharedState, peer_id: &str, thread_id: &str) {
    let Some((api, chat_id)) = prepare_send(state, peer_id, thread_id).await else {
        return;
    };
    if let Err(e) = api.send_chat_action(chat_id, "typing").await {
        tracing::debug!("telegram echo: send_chat_action failed: {e:?}");
    }
}

/// Mirror a complete Q&A turn (user question + assistant reply) to the
/// user's Telegram chat as a SINGLE bot message. The question is
/// wrapped in `<blockquote>` so it visually pops from a normal bot
/// reply; the answer follows in plain text below.
///
/// `user_sender` controls the question-quote behaviour: when it equals
/// `"Telegram"` the user typed it on Telegram already and we skip the
/// quote (sending only the reply, same as the v0.2.13 behaviour). Any
/// other sender means the user typed in KinAI's UI and we render the
/// full Q&A.
///
/// For slash-command replies that resolve to a `/v1/pic/<uuid>` URL,
/// we send a Telegram photo with the same combined block as a caption
/// instead of two separate posts.
pub async fn maybe_echo_qa(
    state: &SharedState,
    peer_id: &str,
    thread_id: &str,
    user_sender: &str,
    user_question: &str,
    assistant_reply: &str,
) {
    if assistant_reply.trim().is_empty() {
        return;
    }
    let Some((api, chat_id)) = prepare_send(state, peer_id, thread_id).await else {
        return;
    };
    let include_question = user_sender != "Telegram" && !user_question.trim().is_empty();

    // Image branch — slash command output that ComfyUI generated.
    if let Some(local_path) = extract_local_pic_path(state, assistant_reply) {
        let caption_text = strip_inline_image_markdown(assistant_reply);
        let caption_html = format_qa_html(include_question, user_question, &caption_text);
        let caption_opt = if caption_html.trim().is_empty() {
            None
        } else {
            Some(caption_html.as_str())
        };
        if let Err(e) = api
            .send_photo_file(chat_id, &local_path, caption_opt, Some("HTML"))
            .await
        {
            tracing::warn!("telegram echo: send_photo_file failed: {e:?}");
        }
        return;
    }

    // Text branch — wrap question in <blockquote>, answer below in plain.
    let html = format_qa_html(include_question, user_question, assistant_reply);
    if let Err(e) = api.send_message_html(chat_id, &html).await {
        tracing::warn!("telegram echo: send_message_html failed: {e:?}");
    }
}

/// Common gating + setup. Returns the bot API client + chat id when we
/// should attempt to send to Telegram, None when we shouldn't.
async fn prepare_send(
    state: &SharedState,
    peer_id: &str,
    thread_id: &str,
) -> Option<(super::api::BotApi, i64)> {
    if thread_id != telegram_thread_id_for_peer(peer_id) {
        return None; // not a Telegram thread
    }
    let token = state.config.read().telegram.bot_token.clone();
    if token.trim().is_empty() {
        return None; // bot disabled
    }
    let chat_id = match crate::db::telegram::chat_for_peer(&state.db.pool, peer_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return None, // peer hasn't paired their Telegram
        Err(e) => {
            tracing::warn!("telegram echo: chat_for_peer({peer_id}) failed: {e:?}");
            return None;
        }
    };
    let chat_id_i64: i64 = match chat_id.parse() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("telegram echo: invalid chat_id {chat_id:?}: {e}");
            return None;
        }
    };
    Some((super::api::BotApi::new(token), chat_id_i64))
}

/// Build the HTML body for a combined Q&A echo. When `include_question`
/// is false (Telegram-originated user message), returns just the
/// HTML-escaped answer.
fn format_qa_html(include_question: bool, question: &str, answer: &str) -> String {
    let answer_escaped = html_escape(answer);
    if !include_question {
        return answer_escaped;
    }
    let q_escaped = html_escape(question.trim());
    format!("<blockquote>{q_escaped}</blockquote>\n\n{answer_escaped}")
}

/// Escape the three characters that Telegram's HTML parse mode treats
/// as special. Everything else (newlines, Unicode, emoji) is passed
/// through verbatim. Deliberately tiny so we don't pull in `html-escape`
/// for what's effectively three `replace` calls.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
