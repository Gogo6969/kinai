//! Long-term memory store (FTS5 BM25 search).
//!
//! Memory notes are summaries the LLM produces from old conversation turns,
//! used to keep long histories in context without blowing the token budget.
//! Every note is scoped to exactly one peer — when one family member's
//! chat gets summarized, the summary must never surface in another family
//! member's prompt. The `peer_id` column enforces that at the query layer.

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNote {
    pub id: String,
    pub thread_id: String,
    pub summary: String,
    pub keywords: String,
    pub created_at: String,
    #[serde(default)]
    pub peer_id: String,
}

pub async fn save(
    pool: &SqlitePool,
    peer_id: &str,
    thread_id: &str,
    summary: &str,
    keywords: &str,
) -> Result<MemoryNote> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO memory_notes (id, thread_id, summary, keywords, created_at, peer_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&id)
    .bind(thread_id)
    .bind(summary)
    .bind(keywords)
    .bind(&now)
    .bind(peer_id)
    .execute(pool)
    .await?;
    Ok(MemoryNote {
        id,
        thread_id: thread_id.into(),
        summary: summary.into(),
        keywords: keywords.into(),
        created_at: now,
        peer_id: peer_id.into(),
    })
}

pub async fn search(
    pool: &SqlitePool,
    peer_id: &str,
    thread_id: &str,
    query: &str,
    limit: i64,
) -> Result<Vec<MemoryNote>> {
    let fts_query = sanitize_fts(query);
    if !fts_query.is_empty() {
        let rows = sqlx::query(
            "SELECT m.id, m.thread_id, m.summary, m.keywords, m.created_at, m.peer_id
             FROM memory_fts
             JOIN memory_notes m ON m.rowid = memory_fts.rowid
             WHERE memory_fts MATCH ?1 AND m.thread_id = ?2 AND m.peer_id = ?3
             ORDER BY bm25(memory_fts) ASC
             LIMIT ?4",
        )
        .bind(&fts_query)
        .bind(thread_id)
        .bind(peer_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;
        let out: Vec<MemoryNote> = rows.into_iter().map(row_to_note).collect();
        if !out.is_empty() {
            return Ok(out);
        }
    }

    let rows = sqlx::query(
        "SELECT id, thread_id, summary, keywords, created_at, peer_id
         FROM memory_notes WHERE thread_id = ?1 AND peer_id = ?2
         ORDER BY created_at DESC LIMIT ?3",
    )
    .bind(thread_id)
    .bind(peer_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_note).collect())
}

fn sanitize_fts(input: &str) -> String {
    let mut tokens: Vec<String> = input
        .split_whitespace()
        .map(|t| {
            t.chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                .collect::<String>()
        })
        .filter(|t| t.len() > 2)
        .map(|t| format!("\"{}\"", t))
        .collect();
    tokens.truncate(8);
    tokens.join(" OR ")
}

fn row_to_note(r: sqlx::sqlite::SqliteRow) -> MemoryNote {
    MemoryNote {
        id: r.get("id"),
        thread_id: r.get("thread_id"),
        summary: r.get("summary"),
        keywords: r.get("keywords"),
        created_at: r.get("created_at"),
        peer_id: r.get("peer_id"),
    }
}
