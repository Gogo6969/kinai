//! Answers a family member flagged with the Report button.
//!
//! Privacy note (this is the whole point of the design): KinAI promises
//! that a family member's conversations are invisible to the host. A
//! report is the ONE deliberate exception — the reporter hands over a
//! snapshot of a single question/answer pair so the host can fix what
//! went wrong. The host therefore never reads the peer's thread to build
//! a report; it stores exactly what was handed to it, and nothing else
//! from that conversation becomes visible.

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub id: String,
    /// Who reported it — `db::HOST_PEER` for the host's own flags.
    pub peer_id: String,
    /// Display name at report time ("Mom", "Wolf"), so the host sees who
    /// hit the button even after a peer reconnects with a new id.
    pub reporter: String,
    /// Id of the assistant message on the REPORTER's side. Kept for
    /// deduplication, not used to look anything up on the host.
    pub message_id: String,
    pub question: String,
    pub answer: String,
    pub model: String,
    pub slot: String,
    pub created_at: String,
    /// Set when the host marks it handled; open reports drive the badge.
    pub reviewed_at: Option<String>,
}

fn row_to_report(r: &sqlx::sqlite::SqliteRow) -> Report {
    Report {
        id: r.get("id"),
        peer_id: r.get("peer_id"),
        reporter: r.get("reporter"),
        message_id: r.get("message_id"),
        question: r.get("question"),
        answer: r.get("answer"),
        model: r.get("model"),
        slot: r.get("slot"),
        created_at: r.get("created_at"),
        reviewed_at: r.get("reviewed_at"),
    }
}

/// Store a report. Re-reporting the same message by the same peer
/// refreshes the existing row (and re-opens it if it was reviewed)
/// instead of stacking duplicates — a user pressing the button twice
/// shouldn't spam the host's list.
#[allow(clippy::too_many_arguments)]
pub async fn add(
    pool: &SqlitePool,
    peer_id: &str,
    reporter: &str,
    message_id: &str,
    question: &str,
    answer: &str,
    model: &str,
    slot: &str,
) -> Result<Report> {
    let now = Utc::now().to_rfc3339();
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM reports WHERE peer_id = ? AND message_id = ?",
    )
    .bind(peer_id)
    .bind(message_id)
    .fetch_optional(pool)
    .await?;

    let id = existing.unwrap_or_else(|| Uuid::new_v4().to_string());
    sqlx::query(
        "INSERT INTO reports (id, peer_id, reporter, message_id, question, answer,
                              model, slot, created_at, reviewed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)
         ON CONFLICT(id) DO UPDATE SET
            reporter = excluded.reporter,
            question = excluded.question,
            answer   = excluded.answer,
            model    = excluded.model,
            slot     = excluded.slot,
            created_at = excluded.created_at,
            reviewed_at = NULL",
    )
    .bind(&id)
    .bind(peer_id)
    .bind(reporter)
    .bind(message_id)
    .bind(question)
    .bind(answer)
    .bind(model)
    .bind(slot)
    .bind(&now)
    .execute(pool)
    .await?;

    Ok(Report {
        id,
        peer_id: peer_id.into(),
        reporter: reporter.into(),
        message_id: message_id.into(),
        question: question.into(),
        answer: answer.into(),
        model: model.into(),
        slot: slot.into(),
        created_at: now,
        reviewed_at: None,
    })
}

/// Newest first; open reports before reviewed ones.
pub async fn list(pool: &SqlitePool, limit: i64) -> Result<Vec<Report>> {
    let rows = sqlx::query(
        "SELECT * FROM reports
         ORDER BY (reviewed_at IS NOT NULL), created_at DESC
         LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_report).collect())
}

/// How many reports still need the host's attention (drives the badge).
pub async fn open_count(pool: &SqlitePool) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM reports WHERE reviewed_at IS NULL")
            .fetch_one(pool)
            .await?,
    )
}

/// Mark handled (or re-open with `reviewed = false`).
pub async fn set_reviewed(pool: &SqlitePool, id: &str, reviewed: bool) -> Result<()> {
    let stamp = reviewed.then(|| Utc::now().to_rfc3339());
    sqlx::query("UPDATE reports SET reviewed_at = ? WHERE id = ?")
        .bind(stamp)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM reports WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let p = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::migrate::run(&p).await.unwrap();
        p
    }

    #[tokio::test]
    async fn add_list_and_review_roundtrip() {
        let p = pool().await;
        assert_eq!(open_count(&p).await.unwrap(), 0);

        let r = add(&p, "peer-1", "Mom", "msg-1", "Who won?", "Nonsense answer", "m", "fast")
            .await
            .unwrap();
        assert_eq!(open_count(&p).await.unwrap(), 1);

        // Re-reporting the same message updates in place, never duplicates.
        let again = add(&p, "peer-1", "Mom", "msg-1", "Who won?", "Still nonsense", "m", "fast")
            .await
            .unwrap();
        assert_eq!(again.id, r.id);
        assert_eq!(list(&p, 50).await.unwrap().len(), 1);
        assert_eq!(list(&p, 50).await.unwrap()[0].answer, "Still nonsense");

        // A different peer reporting the same message id is its own row.
        add(&p, "peer-2", "Dad", "msg-1", "Who won?", "Nonsense", "m", "fast")
            .await
            .unwrap();
        assert_eq!(open_count(&p).await.unwrap(), 2);

        set_reviewed(&p, &r.id, true).await.unwrap();
        assert_eq!(open_count(&p).await.unwrap(), 1);
        // Reviewed rows sort behind open ones.
        assert_eq!(list(&p, 50).await.unwrap()[0].peer_id, "peer-2");

        // Re-reporting a reviewed message re-opens it — the user is saying
        // "this is still broken".
        add(&p, "peer-1", "Mom", "msg-1", "Who won?", "Nonsense again", "m", "fast")
            .await
            .unwrap();
        assert_eq!(open_count(&p).await.unwrap(), 2);

        delete(&p, &r.id).await.unwrap();
        assert_eq!(list(&p, 50).await.unwrap().len(), 1);
    }
}
