//! Long-term memory formatting + summary scheduling.

use anyhow::Result;

use crate::db::{Db, MemoryNote, UserFact};

use super::summarizer;

const SUMMARIZE_AFTER: i64 = 30;
const KEEP_RECENT: i64 = 12;

/// Hard cap for total prompt size of injected user facts. Each fact
/// is one short line, so 4000 chars ≈ 50–80 facts — well past what
/// any single user will accumulate, but caps the worst-case "remember
/// every line of dialog" runaway.
const MAX_FACTS_PROMPT_CHARS: usize = 4000;

pub fn format_memories(memories: &[MemoryNote]) -> String {
    let mut s = String::from("Relevant context from earlier conversations:\n");
    for (i, m) in memories.iter().enumerate() {
        s.push_str(&format!("{}. {}\n", i + 1, m.summary));
    }
    s.push_str("(Use the above only when relevant; do not mention it explicitly unless asked.)");
    s
}

/// Render the user's persistent facts into a system-prompt block.
/// Newest-first is the order list_user_facts returns; we cap at
/// MAX_FACTS_PROMPT_CHARS to bound the prompt size.
///
/// The footer tells the model two things:
///   1. These facts are AUTHORITATIVE for this user (don't second-guess).
///   2. Use the `remember` / `forget` tools to manage them — this is
///      what makes the loop self-correcting when a fact goes stale.
pub fn format_user_facts(facts: &[UserFact]) -> String {
    let mut s = String::from(
        "Persistent facts you've learned about this user across all past conversations:\n",
    );
    let mut budget = MAX_FACTS_PROMPT_CHARS;
    let mut shown = 0usize;
    for f in facts {
        let line = format!("- {}: {}\n", f.key, f.value);
        if line.len() > budget {
            break;
        }
        budget -= line.len();
        s.push_str(&line);
        shown += 1;
    }
    if shown < facts.len() {
        s.push_str(&format!(
            "(…and {} more not shown to fit context.)\n",
            facts.len() - shown
        ));
    }
    s.push_str(
        "These facts are authoritative — treat them as ground truth about the user. \
         If a fact becomes outdated (the user mentions a change), call the `remember` tool with \
         the same key and new value to update it. Use the `forget` tool when the user asks \
         you to. Do not explicitly recite these facts at the start of a reply unless asked.",
    );
    s
}

pub async fn maybe_summarize(db: &Db, peer_id: &str, thread_id: &str) -> Result<()> {
    let n = db.count_since_summary(peer_id, thread_id).await?;
    if n < SUMMARIZE_AFTER {
        return Ok(());
    }
    let to_fold = db
        .oldest_unsummarized(peer_id, thread_id, KEEP_RECENT)
        .await?;
    if to_fold.is_empty() {
        return Ok(());
    }
    let (summary, keywords) = summarizer::summarize_window(&to_fold);
    let note = db
        .save_memory(peer_id, thread_id, &summary, &keywords)
        .await?;
    let ids: Vec<String> = to_fold.iter().map(|m| m.id.clone()).collect();
    db.mark_summarized(&ids, &note.id).await?;
    Ok(())
}
