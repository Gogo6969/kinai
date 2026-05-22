//! Thread + message persistence (async via SQLx).

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

/// Sentinel peer_id used for the host machine's own conversations. Real
/// client peers use their invite's 6-char short_code (the JWT `sub`) as
/// peer_id, so this string is reserved and can't collide.
pub const HOST_PEER: &str = "host";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadMeta {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub peer_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Attachment {
    pub kind: String,
    pub mime: Option<String>,
    pub name: Option<String>,
    pub data_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub thread_id: String,
    pub role: String,
    pub sender: String,
    pub content: String,
    pub attachments: Vec<Attachment>,
    pub created_at: String,
    pub summarized_into: Option<String>,
    /// JSON blob for per-turn perf metrics (TurnMetricsWire). Optional.
    #[serde(default)]
    pub metrics: Option<serde_json::Value>,
}

pub async fn list_threads(pool: &SqlitePool, peer_id: &str) -> Result<Vec<ThreadMeta>> {
    let rows = sqlx::query(
        "SELECT id, title, created_at, updated_at, peer_id
         FROM threads WHERE peer_id = ?1
         ORDER BY updated_at DESC",
    )
    .bind(peer_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ThreadMeta {
            id: r.get("id"),
            title: r.get("title"),
            created_at: r.get("created_at"),
            updated_at: r.get("updated_at"),
            peer_id: r.get("peer_id"),
        })
        .collect())
}

pub async fn create_thread(
    pool: &SqlitePool,
    peer_id: &str,
    title: Option<&str>,
) -> Result<ThreadMeta> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let title = title.unwrap_or("New conversation").to_string();
    sqlx::query(
        "INSERT INTO threads (id, title, created_at, updated_at, peer_id)
         VALUES (?1, ?2, ?3, ?3, ?4)",
    )
    .bind(&id)
    .bind(&title)
    .bind(&now)
    .bind(peer_id)
    .execute(pool)
    .await?;
    Ok(ThreadMeta {
        id,
        title,
        created_at: now.clone(),
        updated_at: now,
        peer_id: peer_id.into(),
    })
}

/// Insert a thread with a caller-supplied ID, no-op if it already exists.
/// Used on the host when a client sends a message for a thread that lives
/// only in the client's local DB — without this, the FK on `messages` would
/// reject the insert. The `peer_id` is the JWT-bound short_code of the
/// connecting client, so each family member's threads land in their own
/// bucket and never get listed under another peer.
pub async fn upsert_thread(
    pool: &SqlitePool,
    peer_id: &str,
    id: &str,
    title: &str,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT OR IGNORE INTO threads (id, title, created_at, updated_at, peer_id)
         VALUES (?1, ?2, ?3, ?3, ?4)",
    )
    .bind(id)
    .bind(title)
    .bind(&now)
    .bind(peer_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Rename only if the thread belongs to the calling peer. Returns Ok even
/// on a no-match so callers can't probe whether a thread_id exists under
/// some other peer.
pub async fn rename_thread(
    pool: &SqlitePool,
    peer_id: &str,
    id: &str,
    title: &str,
) -> Result<()> {
    sqlx::query("UPDATE threads SET title = ?1, updated_at = ?2 WHERE id = ?3 AND peer_id = ?4")
        .bind(title)
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .bind(peer_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_thread(pool: &SqlitePool, peer_id: &str, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM threads WHERE id = ?1 AND peer_id = ?2")
        .bind(id)
        .bind(peer_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Read the per-thread sticky LLM-slot ("fast" / "deep" / None).
/// None = use the global default. Cheap one-column lookup; called
/// once per chat turn so the user doesn't have to repeat /fast or
/// /deep on every message after they switch.
pub async fn thread_active_slot(
    pool: &SqlitePool,
    peer_id: &str,
    id: &str,
) -> Result<Option<String>> {
    let row = sqlx::query("SELECT active_slot FROM threads WHERE id = ?1 AND peer_id = ?2")
        .bind(id)
        .bind(peer_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|r| r.try_get::<Option<String>, _>("active_slot").ok().flatten()))
}

/// Set (or clear with `None`) the sticky LLM-slot for `id`. Idempotent;
/// no-op when the row doesn't exist or belongs to a different peer.
pub async fn set_thread_active_slot(
    pool: &SqlitePool,
    peer_id: &str,
    id: &str,
    slot: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE threads SET active_slot = ?1 WHERE id = ?2 AND peer_id = ?3")
        .bind(slot)
        .bind(id)
        .bind(peer_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn load(
    pool: &SqlitePool,
    peer_id: &str,
    thread_id: &str,
    limit: i64,
) -> Result<Vec<Message>> {
    // Cross-peer access guard: join with threads so a peer can only read
    // messages of threads they actually own. Without the JOIN, a peer who
    // somehow learned another peer's thread_id (UUID guessing aside, it
    // could leak through a forwarded link, an old backup, etc.) could
    // request their messages over the WebSocket.
    //
    // Pagination semantic — read carefully: we want the MOST RECENT `limit`
    // messages, returned in chronological (oldest-first) order. The old
    // SQL was `ORDER BY created_at ASC LIMIT N` which silently gave the
    // OLDEST N once the thread exceeded N messages — meaning the LLM
    // context-builder (which calls this with limit=50) saw the very first
    // 50 turns of the thread forever, and never the recent ones. That's
    // what was causing "model thinks I asked for a joke five months ago"
    // and the matching Telegram context-loss bug. Switch to DESC + reverse
    // so we always return the tail in ASC order.
    let rows = sqlx::query(
        "SELECT m.id, m.thread_id, m.role, m.sender, m.content, m.attachments,
                m.created_at, m.summarized_into, m.metrics
         FROM messages m
         JOIN threads t ON t.id = m.thread_id
         WHERE m.thread_id = ?1 AND t.peer_id = ?2
         ORDER BY m.created_at DESC LIMIT ?3",
    )
    .bind(thread_id)
    .bind(peer_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut messages: Vec<Message> = rows.into_iter().map(row_to_message).collect();
    messages.reverse();
    Ok(messages)
}

pub async fn append(
    pool: &SqlitePool,
    thread_id: &str,
    role: &str,
    sender: &str,
    content: &str,
    attachments: &[Attachment],
) -> Result<Message> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let att = serde_json::to_string(attachments)?;
    sqlx::query(
        "INSERT INTO messages (id, thread_id, role, sender, content, attachments, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(&id)
    .bind(thread_id)
    .bind(role)
    .bind(sender)
    .bind(content)
    .bind(&att)
    .bind(&now)
    .execute(pool)
    .await?;
    sqlx::query("UPDATE threads SET updated_at = ?1 WHERE id = ?2")
        .bind(&now)
        .bind(thread_id)
        .execute(pool)
        .await?;
    Ok(Message {
        id,
        thread_id: thread_id.into(),
        role: role.into(),
        sender: sender.into(),
        content: content.into(),
        attachments: attachments.to_vec(),
        created_at: now,
        summarized_into: None,
        metrics: None,
    })
}

/// Attach perf metrics to an already-persisted assistant message.
pub async fn set_metrics(
    pool: &SqlitePool,
    id: &str,
    metrics: &serde_json::Value,
) -> Result<()> {
    sqlx::query("UPDATE messages SET metrics = ?1 WHERE id = ?2")
        .bind(metrics.to_string())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_content(pool: &SqlitePool, id: &str, new_content: &str) -> Result<()> {
    sqlx::query("UPDATE messages SET content = ?1 WHERE id = ?2")
        .bind(new_content)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn count_since_summary(
    pool: &SqlitePool,
    peer_id: &str,
    thread_id: &str,
) -> Result<i64> {
    let row = sqlx::query(
        "SELECT COUNT(*) as n FROM messages m
         JOIN threads t ON t.id = m.thread_id
         WHERE m.thread_id = ?1 AND t.peer_id = ?2 AND m.summarized_into IS NULL",
    )
    .bind(thread_id)
    .bind(peer_id)
    .fetch_one(pool)
    .await?;
    Ok(row.get::<i64, _>("n"))
}

pub async fn oldest_unsummarized(
    pool: &SqlitePool,
    peer_id: &str,
    thread_id: &str,
    keep_recent: i64,
) -> Result<Vec<Message>> {
    let rows = sqlx::query(
        "SELECT m.id, m.thread_id, m.role, m.sender, m.content, m.attachments,
                m.created_at, m.summarized_into, m.metrics
         FROM messages m
         JOIN threads t ON t.id = m.thread_id
         WHERE m.thread_id = ?1 AND t.peer_id = ?2 AND m.summarized_into IS NULL
         ORDER BY m.created_at ASC",
    )
    .bind(thread_id)
    .bind(peer_id)
    .fetch_all(pool)
    .await?;

    let mut all: Vec<Message> = rows.into_iter().map(row_to_message).collect();
    let len = all.len() as i64;
    if len <= keep_recent {
        return Ok(Vec::new());
    }
    all.truncate((len - keep_recent) as usize);
    Ok(all)
}

pub async fn mark_summarized(pool: &SqlitePool, ids: &[String], summary_id: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    for id in ids {
        sqlx::query("UPDATE messages SET summarized_into = ?1 WHERE id = ?2")
            .bind(summary_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn row_to_message(r: sqlx::sqlite::SqliteRow) -> Message {
    let att: String = r.get("attachments");
    let metrics_str: Option<String> = r.try_get("metrics").ok();
    let metrics = metrics_str
        .and_then(|s| serde_json::from_str(&s).ok());
    Message {
        id: r.get("id"),
        thread_id: r.get("thread_id"),
        role: r.get("role"),
        sender: r.get("sender"),
        content: r.get("content"),
        attachments: serde_json::from_str(&att).unwrap_or_default(),
        created_at: r.get("created_at"),
        summarized_into: r.try_get("summarized_into").ok(),
        metrics,
    }
}
