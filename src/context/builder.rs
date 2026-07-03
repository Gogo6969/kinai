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
    // Context load strips history image payloads at the SQL layer — the
    // vision router only sends current-turn images anyway, and full rows
    // meant parsing tens of MB of base64 per turn in image-bearing threads.
    let recent = db
        .load_messages_for_context(peer_id, thread_id, RECENT_TURNS)
        .await?;
    let memories = db
        .relevant_memories(peer_id, thread_id, &new_message.content, TOP_MEMORIES)
        .await?;
    // User facts — the persistent per-peer memory layer. Always pulled
    // in full (the table is small, the values are short); the prompt
    // formatter caps the total so a runaway "remember everything"
    // pattern can't blow the system-prompt budget.
    let user_facts = db.user_facts_for_prompt(peer_id).await.unwrap_or_default();

    let mut messages: Vec<ChatMessage> =
        Vec::with_capacity(3 + memories.len() + recent.len() + 1);

    messages.push(system_prompt(&cfg.host.family_name, &cfg.llm.system_addendum));

    if !user_facts.is_empty() {
        messages.push(ChatMessage::System {
            content: memory::format_user_facts(&user_facts),
        });
    }

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

    // Reserve generation headroom so the prompt never fills the entire
    // context window. This is the fix for "the deep model thinks for a
    // second then goes silent":
    //
    // Old code trimmed the prompt to `context_window - max_tokens`. With
    // max_tokens = 0 (auto — the default, and what reasoning models need)
    // that's `context_window - 0 = context_window`, i.e. the prompt was
    // allowed to consume the FULL window. compute_max_tokens then derived
    // `context_window - prompt - safety`, which collapsed to its 256-token
    // floor once a thread's history grew large enough to fill the window.
    //
    // A non-reasoning model (gpt-oss) survives a 256-token budget — it
    // emits the visible answer immediately. A reasoning model (Qwen3,
    // R1) spends its whole budget on the <think> phase and hits
    // finish_reason=length with EMPTY content before producing anything
    // visible. Same prompt, same budget, opposite outcome — which is why
    // the fast slot worked and the deep slot didn't.
    //
    // So when max_tokens is auto (0), reserve HALF the context window for
    // generation (floored at 8192) instead of zero. This is the v0.2.50
    // bump from a flat 8192: the v0.2.49 reserve fixed FRESH threads but
    // a long thread still failed on /deep, because 8192 generation
    // tokens isn't enough for a reasoning model to think over a large
    // (~24k-token) context AND produce an answer — it ran out mid-think
    // and emitted nothing. Reserving half the window caps the prompt at
    // ~16k and guarantees the deep model ~16k tokens to reason + answer,
    // which holds up even when a user switches to /deep deep into a long
    // fast-model conversation.
    //
    // The cost is shallower recalled history on very long threads, but a
    // truncated/empty answer is far worse than slightly less context —
    // and the thread summary + memory layers preserve the gist of what
    // gets trimmed. When the user pins an explicit max_tokens, honor it.
    let budget = prompt_budget(cfg.llm.context_window, cfg.llm.max_tokens);
    token_guard::trim_to_fit(&mut messages, budget);
    Ok(messages)
}

/// How many tokens of the context window the PROMPT may use; the rest is
/// reserved for generation (see the long rationale above).
///
/// The v0.2.50 formula had a collapse bug on small windows: with auto
/// max_tokens the reserve was `max(cw/2, 8192)`, which on an 8k-context
/// model (the README's own starter suggestion) is the ENTIRE window —
/// prompt budget 0, so the model saw no history/memory at all and looked
/// inexplicably dumb. Floor the prompt budget at a quarter of the window so
/// every model keeps meaningful context; the same floor also rescues a
/// user-pinned max_tokens larger than the window.
fn prompt_budget(context_window: usize, max_tokens: usize) -> usize {
    let reserve = if max_tokens > 0 {
        max_tokens
    } else {
        (context_window / 2).max(8192)
    };
    context_window
        .saturating_sub(reserve)
        .max(context_window / 4)
}

#[cfg(test)]
mod budget_tests {
    use super::prompt_budget;

    #[test]
    fn small_windows_keep_context() {
        // 8k model, auto max_tokens: old formula gave 0. Floor = 2048.
        assert_eq!(prompt_budget(8192, 0), 2048);
        // 4k model: floor = 1024 (was 0).
        assert_eq!(prompt_budget(4096, 0), 1024);
    }

    #[test]
    fn large_windows_unchanged() {
        // 32k: reserve 16k, budget 16k — exactly the pre-fix behavior.
        assert_eq!(prompt_budget(32_768, 0), 16_384);
        // 16k: reserve max(8k,8k)=8k, budget 8k.
        assert_eq!(prompt_budget(16_384, 0), 8_192);
    }

    #[test]
    fn explicit_max_tokens_honored_but_floored() {
        // Pinned 4k generation on a 32k window: prompt gets the rest.
        assert_eq!(prompt_budget(32_768, 4_096), 28_672);
        // Pathological pin larger than the window: floor saves the prompt.
        assert_eq!(prompt_budget(8_192, 32_768), 2_048);
    }
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
