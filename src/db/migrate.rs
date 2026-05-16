//! Hand-rolled migrations — kept inline so a fresh user just runs `pnpm tauri dev`.

use anyhow::Result;
use sqlx::SqlitePool;

const STATEMENTS: &[&str] = &[
    r#"
    CREATE TABLE IF NOT EXISTS threads (
        id          TEXT PRIMARY KEY,
        title       TEXT NOT NULL,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL
    )
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS messages (
        id              TEXT PRIMARY KEY,
        thread_id       TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
        role            TEXT NOT NULL,
        sender          TEXT NOT NULL,
        content         TEXT NOT NULL,
        attachments     TEXT NOT NULL DEFAULT '[]',
        created_at      TEXT NOT NULL,
        summarized_into TEXT
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS messages_thread_created
        ON messages(thread_id, created_at)
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS memory_notes (
        id          TEXT PRIMARY KEY,
        thread_id   TEXT NOT NULL,
        summary     TEXT NOT NULL,
        keywords    TEXT NOT NULL,
        created_at  TEXT NOT NULL
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS memory_thread ON memory_notes(thread_id)
    "#,
    r#"
    CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
        keywords,
        summary,
        content='memory_notes',
        content_rowid='rowid'
    )
    "#,
    r#"
    CREATE TRIGGER IF NOT EXISTS memory_notes_ai AFTER INSERT ON memory_notes BEGIN
        INSERT INTO memory_fts(rowid, keywords, summary)
        VALUES (new.rowid, new.keywords, new.summary);
    END
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS invites (
        id          TEXT PRIMARY KEY,
        short_code  TEXT NOT NULL UNIQUE,
        jwt         TEXT NOT NULL,
        host_url    TEXT NOT NULL,
        label       TEXT NOT NULL DEFAULT '',
        created_at  TEXT NOT NULL,
        expires_at  TEXT NOT NULL,
        revoked     INTEGER NOT NULL DEFAULT 0
    )
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS peers (
        id           TEXT PRIMARY KEY,
        display_name TEXT NOT NULL,
        invite_id    TEXT,
        first_seen   TEXT NOT NULL,
        last_seen    TEXT NOT NULL,
        revoked      INTEGER NOT NULL DEFAULT 0
    )
    "#,
    // Per-turn LLM perf metrics: time-to-first-token, total wall time,
    // output token count, tokens-per-second. Stored as JSON.
    r#"
    ALTER TABLE messages ADD COLUMN metrics TEXT
    "#,
    // -- Per-peer context isolation -----------------------------------------
    // Every thread and every long-term memory note is owned by exactly one
    // peer. The host's own chats live under the sentinel peer_id "host"
    // (set via DEFAULT). Client peers use their invite's short_code as
    // peer_id — same value the JWT carries in `claims.sub`. Without this
    // scoping, list_threads on the host machine would expose every family
    // member's titles to whoever opened the host UI, and memory_notes
    // search would pull other peers' summaries into one peer's prompt.
    r#"
    ALTER TABLE threads ADD COLUMN peer_id TEXT NOT NULL DEFAULT 'host'
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS threads_peer_updated
        ON threads(peer_id, updated_at)
    "#,
    r#"
    ALTER TABLE memory_notes ADD COLUMN peer_id TEXT NOT NULL DEFAULT 'host'
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS memory_peer_thread
        ON memory_notes(peer_id, thread_id)
    "#,
];

pub async fn run(pool: &SqlitePool) -> Result<()> {
    for stmt in STATEMENTS {
        if let Err(e) = sqlx::query(stmt).execute(pool).await {
            let msg = e.to_string().to_lowercase();
            // Idempotency: ALTER TABLE / CREATE TABLE that already happened
            // on a previous launch shouldn't be a fatal error.
            if msg.contains("duplicate column") || msg.contains("already exists") {
                continue;
            }
            return Err(e.into());
        }
    }
    Ok(())
}
