//! Long-term memory formatting + summary scheduling.

use anyhow::Result;

use crate::db::{Db, MemoryNote};

use super::summarizer;

const SUMMARIZE_AFTER: i64 = 30;
const KEEP_RECENT: i64 = 12;

pub fn format_memories(memories: &[MemoryNote]) -> String {
    let mut s = String::from("Relevant context from earlier conversations:\n");
    for (i, m) in memories.iter().enumerate() {
        s.push_str(&format!("{}. {}\n", i + 1, m.summary));
    }
    s.push_str("(Use the above only when relevant; do not mention it explicitly unless asked.)");
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
