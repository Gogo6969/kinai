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
    // The ROUTED slot's settings (fast/balanced/deep). The prompt budget and
    // system addendum must follow the model actually serving this turn — a
    // 32k deep/balanced turn used to be trimmed to the FAST slot's window.
    llm: &crate::config::LlmSettings,
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

    messages.push(system_prompt(&cfg.host.family_name, &llm.system_addendum));

    for m in recent {
        if m.id == new_message.id {
            continue;
        }
        messages.push(message_to_chat(&m));
    }

    // Facts, retrieved memories and the precise clock ride WITH the
    // newest user message instead of sitting in system messages before
    // the history. Two reasons, one of them measured:
    //
    //   * Cache. llama.cpp reuses the prompt cache only up to the first
    //     token that differs. Memory retrieval is keyed on the new
    //     question, so a memories block placed before the history
    //     changed every turn and forced a full reprocess of everything
    //     after it — the "prefill lasts longer than the answer" report.
    //     Measured on the fast slot: 2.3k tokens reprocessed per turn
    //     with the old layout, 41 with a stable prefix (20x).
    //     The newest user message is new each turn anyway, so context
    //     attached to it is the one place it can live rent-free.
    //
    //   * Strict templates (0.2.100) reject system messages after the
    //     first, so the old layout was living on borrowed time anyway.
    //
    // The block is labelled as KinAI's own notes so the model does not
    // mistake it for something the user typed.
    let mut tail = message_to_chat(new_message);
    let mut note = String::new();
    if !user_facts.is_empty() {
        note.push_str(&memory::format_user_facts(&user_facts));
        note.push('\n');
    }
    if !memories.is_empty() {
        note.push_str(&memory::format_memories(&memories));
        note.push('\n');
    }
    note.push_str(&format!(
        "Current time: {}",
        crate::tools::datetime::now_pretty()
    ));
    if let ChatMessage::User { content, .. } = &mut tail {
        content.push_str(&format!(
            "\n\n---\n(KinAI context — your own saved notes and the clock, \
not written by the user:\n{note})"
        ));
    }
    messages.push(tail);

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
    let budget = prompt_budget(llm.context_window, llm.max_tokens);
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
pub(crate) fn prompt_budget(context_window: usize, max_tokens: usize) -> usize {
    let reserve = if max_tokens > 0 {
        max_tokens
    } else {
        (context_window / 2).max(8192)
    };
    context_window
        .saturating_sub(reserve)
        .max(context_window / 4)
}

/// Resolve the per-request `max_tokens` for a chat turn.
///
/// * Explicit `max_tokens > 0` in config → cap at the remaining window
///   (`context_window - prompt - safety`), floored at 256: a failover to
///   a smaller-window slot can push the remainder toward zero, and
///   `"max_tokens": 0` makes servers emit nothing.
/// * Auto (`max_tokens == 0`) on a LOCAL server (http) → the full
///   remaining window, sent explicitly. Some local servers default LOW
///   when the field is omitted, and reasoning models need the room.
/// * Auto on a CLOUD endpoint (https) → `None`, omitting the field, so
///   the provider applies its own default — which is legal by
///   construction. Sending "everything left in the window" breaks here
///   because cloud APIs enforce a separate OUTPUT cap far below their
///   context size: DeepSeek's window is 1M but max_tokens must be
///   ≤ 393 216, so the old computation produced
///   `LLM error 400: Invalid max_tokens value` on every /online turn
///   (field report 2026-08-11). There is no portable way to know each
///   provider's cap; not naming one is the only value that is always
///   right.
///
/// ONE function on purpose. There used to be three private copies
/// (commands.rs, network/server.rs, telegram/router.rs) and they had
/// already drifted: the Telegram copy omitted the field for auto while
/// the app copies sent the full window — which is why the 400 hit app
/// turns but would not have hit Telegram.
pub fn compute_max_tokens(
    llm: &crate::config::LlmSettings,
    messages: &[crate::context::ChatMessage],
) -> Option<usize> {
    const SAFETY: usize = 128;
    let cloud = llm.base_url.trim_start().starts_with("https");
    if llm.max_tokens == 0 && cloud {
        return None;
    }
    let prompt = crate::context::token_guard::estimate_messages(messages);
    let budget = llm
        .context_window
        .saturating_sub(prompt + SAFETY)
        .max(256);
    Some(if llm.max_tokens == 0 {
        budget
    } else {
        llm.max_tokens.min(budget)
    })
}

#[cfg(test)]
mod max_tokens_tests {
    use super::compute_max_tokens;
    use crate::config::LlmSettings;
    use crate::context::ChatMessage;

    fn slot(base_url: &str, context_window: usize, max_tokens: usize) -> LlmSettings {
        let mut s = LlmSettings::default_empty();
        s.base_url = base_url.into();
        s.model = "m".into();
        s.context_window = context_window;
        s.max_tokens = max_tokens;
        s.enabled = true;
        s
    }

    fn msgs() -> Vec<ChatMessage> {
        vec![ChatMessage::User {
            content: "what colour is a stop sign?".into(),
            name: None,
            image_data_urls: vec![],
        }]
    }

    /// The 2026-08-11 field report: a DeepSeek slot with a 1M context
    /// window and auto max_tokens sent ~999k as max_tokens, and DeepSeek
    /// answered `400: Invalid max_tokens value, the valid range of
    /// max_tokens is [1, 393216]`. Auto on a cloud endpoint must omit
    /// the field so the provider's own default applies.
    #[test]
    fn auto_on_cloud_omits_the_field() {
        let s = slot("https://api.deepseek.com", 1_000_000, 0);
        assert_eq!(compute_max_tokens(&s, &msgs()), None);
    }

    /// Local servers keep the explicit remaining-window budget — some
    /// default LOW when the field is missing, and local reasoning
    /// models need the room.
    #[test]
    fn auto_on_local_sends_the_remaining_window() {
        let s = slot("http://192.168.1.50:8084", 32_768, 0);
        let got = compute_max_tokens(&s, &msgs()).expect("local auto must be explicit");
        assert!(got > 30_000 && got < 32_768, "remaining window, got {got}");
    }

    /// An explicit user cap is respected everywhere — including cloud.
    #[test]
    fn explicit_cap_is_kept_on_cloud() {
        let s = slot("https://api.deepseek.com", 1_000_000, 4096);
        assert_eq!(compute_max_tokens(&s, &msgs()), Some(4096));
    }

    /// Failover to a smaller-window slot: the floor keeps the request
    /// from degenerating to `max_tokens: 0` (servers emit nothing).
    #[test]
    fn small_window_floors_at_256() {
        let s = slot("http://localhost:11434", 300, 8192);
        assert_eq!(compute_max_tokens(&s, &msgs()), Some(256));
    }
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

#[cfg(test)]
mod cache_stability_tests {
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

    /// THE property the fast slot's speed depends on: across consecutive
    /// turns in one thread, every prompt message except the newest user
    /// turn must be BYTE-identical. llama.cpp reuses its prompt cache
    /// only up to the first differing token, so a clock in the system
    /// prompt or a per-turn memories block before the history forced a
    /// full ~2.3k-token reprocess every turn (measured; 41 tokens with a
    /// stable prefix). Prefill was taking longer than the answer.
    #[tokio::test]
    async fn prompt_prefix_is_stable_across_turns() {
        let db = fresh_db().await;
        let cfg = crate::config::AppConfig::default();
        let mut llm = cfg.llm.clone();
        // The real fast slot's window. On the 8k default the system
        // prompt alone overruns the trim budget and history is evicted
        // before this test can observe prefix stability.
        llm.context_window = 32768;
        let t = db.create_thread("host", Some("t")).await.unwrap();

        let m1 = db
            .append_message(&t.id, "user", "Wolf", "what colour is grass?", &[])
            .await
            .unwrap();
        let ctx1 = build_context(&db, &cfg, &llm, "host", &t.id, &m1).await.unwrap();

        db.append_message(&t.id, "assistant", "KinAI", "Green.", &[])
            .await
            .unwrap();
        let m2 = db
            .append_message(&t.id, "user", "Wolf", "and the sky?", &[])
            .await
            .unwrap();
        let ctx2 = build_context(&db, &cfg, &llm, "host", &t.id, &m2).await.unwrap();

        // ctx2 must extend ctx1: same system prompt, same history bytes,
        // with only the trailing turns appended/replaced.
        assert!(ctx2.len() > ctx1.len(), "second turn should be longer");
        let shared = ctx1.len() - 1; // everything except turn 1's newest message
        for i in 0..shared {
            assert_eq!(
                ctx1[i].content(),
                ctx2[i].content(),
                "message {i} changed between turns — prefix cache destroyed"
            );
        }

        // The system prompt must not contain a clock (minutes) — the date
        // is fine, minute resolution is what invalidated per-minute.
        let sys = ctx1[0].content();
        assert!(
            !sys.contains(" AM") && !sys.contains(" PM"),
            "system prompt carries a clock again: {}",
            &sys[..sys.len().min(400)]
        );

        // The clock and context notes ride with the newest user message.
        let tail = ctx2.last().unwrap().content();
        assert!(tail.contains("and the sky?"));
        assert!(tail.contains("Current time:"), "clock missing from tail");
    }
}
