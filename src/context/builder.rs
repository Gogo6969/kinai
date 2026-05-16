//! Build the per-request message list from system prompt + memory + history.

use anyhow::Result;

use crate::config::AppConfig;
use crate::db::{Db, Message};

use super::{memory, system_prompt, token_guard, ChatMessage};

const RECENT_TURNS: i64 = 50;
const TOP_MEMORIES: i64 = 8;

pub async fn build_context(
    db: &Db,
    cfg: &AppConfig,
    peer_id: &str,
    thread_id: &str,
    new_message: &Message,
) -> Result<Vec<ChatMessage>> {
    let recent = db.load_messages(peer_id, thread_id, RECENT_TURNS).await?;
    let memories = db
        .relevant_memories(peer_id, thread_id, &new_message.content, TOP_MEMORIES)
        .await?;

    let mut messages: Vec<ChatMessage> =
        Vec::with_capacity(2 + memories.len() + recent.len() + 1);

    messages.push(system_prompt(&cfg.host.family_name, &cfg.llm.system_addendum));

    if !memories.is_empty() {
        messages.push(ChatMessage::System {
            content: memory::format_memories(&memories),
        });
    }

    for m in recent {
        if m.id == new_message.id {
            continue;
        }
        messages.push(message_to_chat(&m));
    }

    messages.push(message_to_chat(new_message));

    let budget = cfg.llm.context_window.saturating_sub(cfg.llm.max_tokens);
    token_guard::trim_to_fit(&mut messages, budget);
    Ok(messages)
}

fn message_to_chat(m: &Message) -> ChatMessage {
    match m.role.as_str() {
        "user" => ChatMessage::User {
            content: format_user(m),
            name: Some(sanitize_name(&m.sender)),
            // Image data URLs ride along separately from the text body —
            // the LLM serializer emits multipart `content` when this is
            // non-empty and the endpoint understands vision. Endpoints
            // that don't get a plain string content and these images are
            // silently dropped at the wire layer (the model still sees
            // the "[attached image: ...]" hint from format_user).
            image_data_urls: crate::vision::image_data_urls(&m.attachments),
        },
        "assistant" => ChatMessage::Assistant {
            content: m.content.clone(),
            tool_calls: Vec::new(),
        },
        "summary" => ChatMessage::System {
            content: format!("[Earlier conversation summary]\n{}", m.content),
        },
        _ => ChatMessage::System {
            content: m.content.clone(),
        },
    }
}

fn format_user(m: &Message) -> String {
    if m.attachments.is_empty() {
        return m.content.clone();
    }
    // Pull readable text out of supported attachments (PDFs today). The
    // user's typed prose stays in front; extracted content follows in a
    // clearly-fenced block so the model knows which is which.
    let extracted = crate::attachments::extract_text(&m.attachments).unwrap_or_default();
    // For attachments we can't extract text from (images, etc.), drop a
    // short note so the model knows something was attached and can defer
    // until the vision pipeline routes it.
    let unread: Vec<String> = m
        .attachments
        .iter()
        .filter(|a| !is_pdf(a))
        .map(|a| {
            format!(
                "[attached {}: {}]",
                a.kind,
                a.name.clone().unwrap_or_default()
            )
        })
        .collect();
    let mut parts = vec![m.content.clone()];
    if !extracted.is_empty() {
        parts.push(extracted);
    }
    if !unread.is_empty() {
        parts.push(unread.join("\n"));
    }
    parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n")
}

fn is_pdf(a: &crate::db::Attachment) -> bool {
    let mime_is_pdf = a
        .mime
        .as_deref()
        .map(|m| m.eq_ignore_ascii_case("application/pdf"))
        .unwrap_or(false);
    let name_is_pdf = a
        .name
        .as_deref()
        .map(|n| n.to_ascii_lowercase().ends_with(".pdf"))
        .unwrap_or(false);
    mime_is_pdf || name_is_pdf
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}
