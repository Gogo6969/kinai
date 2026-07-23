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

/// `load_messages`, but for the LLM context builder: history rows drop their
/// image payloads AT THE SQL LAYER. Measured on a real family DB, 75MB of an
/// 81MB messages table was attachment base64 — and the context builder was
/// pulling, allocating, and JSON-parsing those multi-MB blobs on EVERY turn
/// in an image-bearing thread, only for the vision router to discard them
/// (vision sends current-turn images only).
///
/// The CASE keeps two things intact:
///   * PDF-bearing rows pass through whole — `format_user` re-extracts their
///     text each turn, so follow-up questions about an uploaded PDF keep
///     working;
///   * image rows collapse to a tiny placeholder attachment (no data_url),
///     so the model still sees "[attached image: …]" continuity markers
///     without the megabytes.
/// The UI listing path (`load_messages`) is untouched — thumbnails still
/// render from full rows.
pub async fn load_for_context(
    pool: &SqlitePool,
    peer_id: &str,
    thread_id: &str,
    limit: i64,
) -> Result<Vec<Message>> {
    // PDF detection matches the serialized `"mime":"application/pdf"` field
    // or a PDF data-URL prefix — NOT the bare substring 'application/pdf',
    // which a filename could contain (an image named "see application/pdf
    // notes.png" must still get its payload stripped, not ride through).
    let rows = sqlx::query(
        "SELECT m.id, m.thread_id, m.role, m.sender, m.content,
                CASE
                  WHEN m.attachments LIKE '%\"mime\":\"application/pdf\"%'
                    OR m.attachments LIKE '%data:application/pdf%' THEN m.attachments
                  WHEN m.attachments LIKE '%data:image%'
                    THEN '[{\"kind\":\"image\",\"name\":\"(earlier image omitted)\"}]'
                  ELSE m.attachments
                END AS attachments,
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

/// One cross-thread search result: the matching message plus its thread's
/// title and a highlighted snippet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub thread_id: String,
    pub thread_title: String,
    pub message_id: String,
    pub snippet: String,
    pub created_at: String,
}

/// Full-text search over ONE peer's messages, across all their threads.
/// Peer scoping via the `threads` join is the privacy boundary — a peer
/// can never match another peer's (or the host's) messages. Ranked by
/// bm25; `snippet()` builds a `[match]`-highlighted excerpt.
pub async fn search(
    pool: &SqlitePool,
    peer_id: &str,
    query: &str,
    limit: i64,
) -> Result<Vec<SearchHit>> {
    // Prefix mode ("inves"* matches "investment") — the sidebar searches
    // live while the user types, so whole-token matching would return
    // nothing until the word is typed out exactly.
    let fts_query = crate::db::memory::sanitize_fts_prefix(query);
    if fts_query.is_empty() {
        return Ok(Vec::new()); // empty / too-short query → no results
    }
    let rows = sqlx::query(
        "SELECT m.id        AS message_id,
                m.thread_id  AS thread_id,
                t.title      AS thread_title,
                m.created_at AS created_at,
                snippet(messages_fts, 0, '[', ']', '…', 12) AS snippet
         FROM messages_fts
         JOIN messages m ON m.rowid = messages_fts.rowid
         JOIN threads  t ON t.id    = m.thread_id
         WHERE messages_fts MATCH ?1 AND t.peer_id = ?2
         ORDER BY bm25(messages_fts) ASC
         LIMIT ?3",
    )
    .bind(&fts_query)
    .bind(peer_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SearchHit {
            thread_id: r.get("thread_id"),
            thread_title: r.get("thread_title"),
            message_id: r.get("message_id"),
            snippet: r.get("snippet"),
            created_at: r.get("created_at"),
        })
        .collect())
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

/// Delete every message in `thread_id` with `created_at >= from_created_at`
/// (inclusive). Peer-scoped via the `IN (SELECT … WHERE peer_id = ?)`
/// subquery — SQLite has no `DELETE … JOIN`, so this is the idiomatic
/// equivalent and preserves the same ownership guarantee as `load`. Bumps
/// the thread's `updated_at` so the sidebar re-sorts. Returns rows removed.
/// Used by regenerate/edit-and-resend to truncate a turn before re-running.
pub async fn delete_from(
    pool: &SqlitePool,
    peer_id: &str,
    thread_id: &str,
    from_created_at: &str,
) -> Result<u64> {
    let res = sqlx::query(
        "DELETE FROM messages
         WHERE thread_id = ?1
           AND created_at >= ?2
           AND thread_id IN (SELECT id FROM threads WHERE id = ?1 AND peer_id = ?3)",
    )
    .bind(thread_id)
    .bind(from_created_at)
    .bind(peer_id)
    .execute(pool)
    .await?;
    sqlx::query("UPDATE threads SET updated_at = ?1 WHERE id = ?2 AND peer_id = ?3")
        .bind(Utc::now().to_rfc3339())
        .bind(thread_id)
        .bind(peer_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Fetch a single message by id, but only if it lives in a thread owned by
/// `peer_id`. Returns `None` for a wrong peer or unknown id — the
/// regenerate/edit commands use this to resolve the cut point AND to
/// re-confirm ownership before mutating, so a forged id can't reach
/// another peer's data.
pub async fn get_by_id(
    pool: &SqlitePool,
    peer_id: &str,
    id: &str,
) -> Result<Option<Message>> {
    let row = sqlx::query(
        "SELECT m.id, m.thread_id, m.role, m.sender, m.content, m.attachments,
                m.created_at, m.summarized_into, m.metrics
         FROM messages m
         JOIN threads t ON t.id = m.thread_id
         WHERE m.id = ?1 AND t.peer_id = ?2",
    )
    .bind(id)
    .bind(peer_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_message))
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
    // `'[]' AS attachments`: the summarizer reads only role/content — never
    // pay the multi-MB image-blob parse for rows headed into a text summary.
    let rows = sqlx::query(
        "SELECT m.id, m.thread_id, m.role, m.sender, m.content,
                '[]' AS attachments,
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    /// In-memory DB with the REAL migrations applied (threads + messages +
    /// the FTS index + sync triggers), so these tests also validate that
    /// the new `messages_fts` DDL is well-formed. `max_connections(1)` is
    /// required: each connection to `sqlite::memory:` is a separate DB.
    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        super::super::migrate::run(&pool).await.expect("apply migrations");
        pool
    }

    async fn seed_thread(pool: &SqlitePool, peer: &str, title: &str) -> String {
        create_thread(pool, peer, Some(title)).await.unwrap().id
    }

    #[tokio::test]
    async fn delete_from_is_inclusive_and_peer_scoped() {
        let pool = fresh_pool().await;
        let tid = seed_thread(&pool, HOST_PEER, "t").await;
        let user = append(&pool, &tid, "user", "me", "hello there", &[]).await.unwrap();
        let asst = append(&pool, &tid, "assistant", "KinAI", "hi back", &[]).await.unwrap();

        // Wrong peer can't truncate: 0 rows removed.
        let removed = delete_from(&pool, "BOB", &tid, &asst.created_at).await.unwrap();
        assert_eq!(removed, 0, "cross-peer delete must be a no-op");
        assert_eq!(load(&pool, HOST_PEER, &tid, 50).await.unwrap().len(), 2);

        // Owner truncates from the assistant's timestamp (inclusive): the
        // assistant goes, the earlier user message stays.
        let removed = delete_from(&pool, HOST_PEER, &tid, &asst.created_at).await.unwrap();
        assert_eq!(removed, 1);
        let remaining = load(&pool, HOST_PEER, &tid, 50).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, user.id);
    }

    #[tokio::test]
    async fn get_by_id_is_peer_scoped() {
        let pool = fresh_pool().await;
        let tid = seed_thread(&pool, HOST_PEER, "t").await;
        let m = append(&pool, &tid, "user", "me", "secret note", &[]).await.unwrap();
        assert!(get_by_id(&pool, HOST_PEER, &m.id).await.unwrap().is_some());
        assert!(
            get_by_id(&pool, "BOB", &m.id).await.unwrap().is_none(),
            "a foreign peer must not be able to fetch another peer's message"
        );
    }

    #[tokio::test]
    async fn search_is_peer_scoped() {
        let pool = fresh_pool().await;
        let host_t = seed_thread(&pool, HOST_PEER, "Host thread").await;
        let bob_t = seed_thread(&pool, "BOB", "Bob thread").await;
        append(&pool, &host_t, "user", "me", "the budget meeting is friday", &[]).await.unwrap();
        append(&pool, &bob_t, "user", "bob", "budget budget budget", &[]).await.unwrap();

        let host_hits = search(&pool, HOST_PEER, "budget", 50).await.unwrap();
        assert_eq!(host_hits.len(), 1);
        assert_eq!(host_hits[0].thread_id, host_t);

        let bob_hits = search(&pool, "BOB", "budget", 50).await.unwrap();
        assert_eq!(bob_hits.len(), 1);
        assert_eq!(bob_hits[0].thread_id, bob_t);
    }

    #[tokio::test]
    async fn search_index_syncs_on_delete_and_edit() {
        let pool = fresh_pool().await;
        let tid = seed_thread(&pool, HOST_PEER, "t").await;
        let m = append(&pool, &tid, "user", "me", "pineapple upside down", &[]).await.unwrap();
        assert_eq!(search(&pool, HOST_PEER, "pineapple", 50).await.unwrap().len(), 1);

        // Edit retracts the old body, indexes the new one (messages_au).
        update_content(&pool, &m.id, "oranges and lemons").await.unwrap();
        assert_eq!(search(&pool, HOST_PEER, "pineapple", 50).await.unwrap().len(), 0);
        assert_eq!(search(&pool, HOST_PEER, "oranges", 50).await.unwrap().len(), 1);

        // Delete retracts the rowid (messages_ad) — no ghost hit.
        delete_from(&pool, HOST_PEER, &tid, &m.created_at).await.unwrap();
        assert_eq!(search(&pool, HOST_PEER, "oranges", 50).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn search_matches_word_prefixes() {
        let pool = fresh_pool().await;
        let tid = seed_thread(&pool, HOST_PEER, "t").await;
        append(&pool, &tid, "user", "me", "what is the best investment right now", &[])
            .await
            .unwrap();
        // The sidebar searches live while typing: every prefix of the
        // word the user is typing must already match.
        for q in ["in", "inv", "invest", "investmen", "investment", "INVEST"] {
            assert_eq!(
                search(&pool, HOST_PEER, q, 50).await.unwrap().len(),
                1,
                "prefix {q:?} should match 'investment'"
            );
        }
        assert!(search(&pool, HOST_PEER, "investmentx", 50).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_neutralizes_fts_syntax_and_short_queries() {
        let pool = fresh_pool().await;
        let tid = seed_thread(&pool, HOST_PEER, "t").await;
        append(&pool, &tid, "user", "me", "zebra quantum flux", &[]).await.unwrap();
        // Injection attempts, single chars, bare operators, and
        // punctuation-only tokens sanitize to an empty or harmless MATCH
        // → never a SQL error. ("OR" survives as the quoted literal
        // prefix '"OR"*', not the FTS boolean operator — it just doesn't
        // match this content.)
        for q in ["\") OR 1=1 --", "a", "   ", "*", "OR", "--", "_", "...."] {
            assert!(
                search(&pool, HOST_PEER, q, 50).await.unwrap().is_empty(),
                "query {q:?} should yield no results and no error"
            );
        }
        // And the quoted-operator semantics cut the other way too: a
        // legitimate 2-char prefix that happens to spell an FTS operator
        // is treated as text. "or" must find "ordinary".
        append(&pool, &tid, "user", "me", "ordinary content here", &[]).await.unwrap();
        assert_eq!(search(&pool, HOST_PEER, "OR", 50).await.unwrap().len(), 1);
    }

    fn img_attachment(name: &str) -> Attachment {
        Attachment {
            kind: "image".into(),
            mime: Some("image/png".into()),
            name: Some(name.into()),
            data_url: Some(format!("data:image/png;base64,{}", "A".repeat(64))),
        }
    }

    fn pdf_attachment() -> Attachment {
        Attachment {
            kind: "file".into(),
            mime: Some("application/pdf".into()),
            name: Some("manual.pdf".into()),
            data_url: Some(format!("data:application/pdf;base64,{}", "B".repeat(64))),
        }
    }

    #[tokio::test]
    async fn context_load_strips_images_keeps_pdfs() {
        let pool = fresh_pool().await;
        let tid = seed_thread(&pool, HOST_PEER, "t").await;
        append(&pool, &tid, "user", "me", "look at this", &[img_attachment("pic.png")])
            .await
            .unwrap();
        append(&pool, &tid, "user", "me", "and this doc", &[pdf_attachment()])
            .await
            .unwrap();
        append(&pool, &tid, "user", "me", "plain", &[]).await.unwrap();
        // Image whose NAME contains the pdf mime string — must still strip
        // (the LIKE must key on the mime field, not any substring).
        append(
            &pool,
            &tid,
            "user",
            "me",
            "tricky",
            &[img_attachment("see application/pdf notes.png")],
        )
        .await
        .unwrap();

        let msgs = load_for_context(&pool, HOST_PEER, &tid, 50).await.unwrap();
        assert_eq!(msgs.len(), 4);
        // Image row: placeholder, no payload — but still an attachment so
        // the "[attached image: …]" continuity hint survives.
        assert_eq!(msgs[0].attachments.len(), 1);
        assert!(msgs[0].attachments[0].data_url.is_none(), "image payload stripped");
        assert_eq!(msgs[0].attachments[0].kind, "image");
        // PDF row: passes through whole (text re-extraction needs it).
        assert_eq!(msgs[1].attachments.len(), 1);
        assert!(
            msgs[1].attachments[0].data_url.as_deref().unwrap_or("").starts_with("data:application/pdf"),
            "pdf payload preserved"
        );
        // Plain row untouched.
        assert!(msgs[2].attachments.is_empty());
        // Tricky filename: still stripped.
        assert!(msgs[3].attachments[0].data_url.is_none(), "pdf-ish filename must not defeat the strip");

        // The UI path is unaffected — full payloads still come back.
        let full = load(&pool, HOST_PEER, &tid, 50).await.unwrap();
        assert!(full[0].attachments[0].data_url.is_some(), "UI listing keeps payloads");
    }
}

/// The (question, answer) pair for a fact-check: the assistant message
/// `id` plus the closest preceding user message in the same thread.
/// Peer-scoped through the thread's owner — a WS peer can only check
/// messages in its own threads (same isolation rule as user_facts).
pub async fn question_answer_for_fact_check(
    pool: &SqlitePool,
    peer_id: &str,
    message_id: &str,
) -> Result<Option<(String, String)>> {
    let row: Option<(String, String, String, String)> = sqlx::query_as(
        "SELECT m.thread_id, m.role, m.content, m.created_at
         FROM messages m JOIN threads t ON t.id = m.thread_id
         WHERE m.id = ? AND t.peer_id = ?",
    )
    .bind(message_id)
    .bind(peer_id)
    .fetch_optional(pool)
    .await?;
    let Some((thread_id, role, answer, created_at)) = row else {
        return Ok(None);
    };
    if role != "assistant" {
        return Ok(None);
    }
    let question: Option<(String,)> = sqlx::query_as(
        "SELECT content FROM messages
         WHERE thread_id = ? AND role = 'user' AND created_at <= ? AND id != ?
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&thread_id)
    .bind(&created_at)
    .bind(message_id)
    .fetch_optional(pool)
    .await?;
    Ok(Some((
        question.map(|q| q.0).unwrap_or_default(),
        answer,
    )))
}
