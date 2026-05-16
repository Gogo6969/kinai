//! Trim messages to fit the model's context window.
//!
//! Rules:
//!   1. Keep the system prompts at the front.
//!   2. Always keep the last message (current user turn) intact.
//!   3. Reserve `max_tokens` for the response.
//!   4. Drop the oldest non-system messages until we fit.
//!   5. Last resort: truncate the current message's content.

use once_cell::sync::Lazy;
use tiktoken_rs::{cl100k_base, CoreBPE};

use super::ChatMessage;

const PER_MESSAGE_OVERHEAD: usize = 4;

static BPE: Lazy<CoreBPE> = Lazy::new(|| cl100k_base().expect("cl100k tokenizer"));

pub fn count_tokens(s: &str) -> usize {
    BPE.encode_with_special_tokens(s).len()
}

pub fn estimate_messages(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| count_tokens(m.content()) + PER_MESSAGE_OVERHEAD)
        .sum::<usize>()
        + 2
}

pub fn trim_to_fit(messages: &mut Vec<ChatMessage>, budget: usize) {
    let budget = budget.max(512);

    if estimate_messages(messages) <= budget {
        return;
    }

    let mut i = 0;
    while estimate_messages(messages) > budget && messages.len() > 2 {
        let last_idx = messages.len().saturating_sub(1);
        let is_system = matches!(messages.get(i), Some(ChatMessage::System { .. }));
        if i == last_idx || is_system {
            i += 1;
            if i >= messages.len() {
                break;
            }
            continue;
        }
        messages.remove(i);
    }

    if estimate_messages(messages) > budget {
        if let Some(last) = messages.last_mut() {
            let content = match last {
                ChatMessage::System { content } => content,
                ChatMessage::User { content, .. } => content,
                ChatMessage::Assistant { content, .. } => content,
                ChatMessage::Tool { content, .. } => content,
            };
            let allowed = (budget.saturating_sub(128) * 3).max(512);
            if content.chars().count() > allowed {
                *content = content.chars().take(allowed).collect();
                content.push_str("\n…(truncated)");
            }
        }
    }
}
