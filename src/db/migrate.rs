//! Hand-rolled migrations — kept inline so a fresh user just runs `pnpm tauri dev`.

use anyhow::Result;
use sqlx::{Row, SqlitePool};

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

    // Telegram pairings — one paired Telegram per family peer. The
    // bot routes incoming messages from `chat_id` to the matching
    // peer's KinAI thread; outbound assistant replies route back to
    // the same chat_id.
    r#"
    CREATE TABLE IF NOT EXISTS telegram_links (
        peer_id      TEXT PRIMARY KEY,
        chat_id      TEXT NOT NULL,
        username     TEXT,
        first_name   TEXT,
        paired_at    TEXT NOT NULL
    )
    "#,
    r#"
    CREATE UNIQUE INDEX IF NOT EXISTS telegram_links_chat
        ON telegram_links(chat_id)
    "#,
    // Short-lived pairing tokens. A client requests one, displays it
    // as a QR/deep-link, user scans → Telegram sends /start <token>
    // → bot looks up the token here, finds the peer, INSERTs into
    // telegram_links above, then DELETEs the row. TTL ~10 min.
    r#"
    CREATE TABLE IF NOT EXISTS telegram_pending_pairs (
        token        TEXT PRIMARY KEY,
        peer_id      TEXT NOT NULL,
        created_at   TEXT NOT NULL
    )
    "#,

    // Per-thread sticky LLM-slot selection. When the user types
    // `/fast` or `/deep` in a thread, we remember that choice here so
    // subsequent plain messages (no slash prefix) keep routing to the
    // same slot — they don't snap back to the global default every
    // turn. NULL = "use the global default", "fast" or "deep" = lock
    // this thread to that slot until the user switches again.
    r#"
    ALTER TABLE threads ADD COLUMN active_slot TEXT
    "#,

    // Per-chat voice-reply opt-in (the Telegram /voice toggle). Lives on
    // telegram_links because chat ↔ peer is 1:1, so "per chat" == "per
    // peer" — and the pref naturally resets when a user unpairs.
    r#"
    ALTER TABLE telegram_links ADD COLUMN voice_replies INTEGER NOT NULL DEFAULT 0
    "#,

    // Persistent per-user facts — the long-term memory layer. Distinct
    // from `memory_notes` (which are extractive thread summaries) in
    // two ways:
    //   1. Scope is peer-wide, not thread-wide — a fact stated in one
    //      conversation surfaces in every conversation that peer has,
    //      because "I live in Berlin" is true regardless of which
    //      chat it came up in.
    //   2. Population is deliberate, not automatic — populated by the
    //      `remember` tool (the LLM calls it when the user states a
    //      persistent fact) and by passive background extraction.
    //
    // (peer_id, key) is unique so calling remember twice on the same
    // key overwrites instead of duplicating — "city: Berlin" → "city:
    // Munich" is a single row, not a history of moves. Update timestamp
    // lets the user see when a fact was last touched in the Settings
    // → Memory page.
    //
    // `source` is one of "tool" (LLM called remember()), "extractor"
    // (passive background pass), or "manual" (user added it via the
    // Settings UI). Drives a small chip in the UI so the user can tell
    // facts they explicitly stated apart from ones the model inferred.
    r#"
    CREATE TABLE IF NOT EXISTS user_facts (
        id            TEXT PRIMARY KEY,
        peer_id       TEXT NOT NULL,
        key           TEXT NOT NULL,
        value         TEXT NOT NULL,
        source        TEXT NOT NULL DEFAULT 'tool',
        source_msg_id TEXT,
        created_at    TEXT NOT NULL,
        updated_at    TEXT NOT NULL,
        UNIQUE(peer_id, key)
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_user_facts_peer ON user_facts(peer_id)
    "#,

    // Full-text index over message bodies, for cross-thread search.
    // External-content FTS5 (same pattern as memory_fts): the text lives
    // in `messages`, this stores only the inverted index keyed by
    // messages.rowid. We index only `content`; thread/role/created_at are
    // fetched via the rowid join. Unlike memory_notes (never deleted/
    // edited), messages ARE deleted (delete_thread CASCADE) and edited
    // (update_content), so we need DELETE + UPDATE triggers too, else the
    // index returns stale rowids.
    r#"
    CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
        content,
        content='messages',
        content_rowid='rowid'
    )
    "#,
    r#"
    CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
        INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
    END
    "#,
    r#"
    CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
        INSERT INTO messages_fts(messages_fts, rowid, content)
        VALUES ('delete', old.rowid, old.content);
    END
    "#,
    r#"
    CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
        INSERT INTO messages_fts(messages_fts, rowid, content)
        VALUES ('delete', old.rowid, old.content);
        INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
    END
    "#,
    // One-time backfill for DBs that predate the FTS index (the triggers
    // only fire on rows written after they exist). Guarded so a second
    // launch is a no-op: only runs when the index is empty but messages
    // exist. Must come AFTER the CREATE VIRTUAL TABLE above.
    r#"
    INSERT INTO messages_fts(rowid, content)
        SELECT rowid, content FROM messages
        WHERE (SELECT COUNT(*) FROM messages_fts) = 0
          AND EXISTS (SELECT 1 FROM messages)
    "#,

    // Per-peer "active" Telegram thread. Telegram chats default to one
    // deterministic thread per peer; the `/newchat` command rotates to a
    // fresh thread and records its id here so subsequent messages from
    // that chat land in the new thread instead of the old one. No row =
    // use the deterministic default (backward compatible with chats
    // paired before this column existed).
    r#"
    CREATE TABLE IF NOT EXISTS telegram_active_thread (
        peer_id    TEXT PRIMARY KEY,
        thread_id  TEXT NOT NULL,
        updated_at TEXT NOT NULL
    )
    "#,
    // Records WHEN an invite was revoked, so a background sweep can
    // hard-delete invites that have been dead (revoked or expired) for a
    // grace period. Expired invites purge off their existing `expires_at`;
    // revoked ones need their own timestamp because revoke() can fire any
    // time after issue. NULL for rows revoked before this column existed —
    // cleanup_stale falls back to created_at for those.
    r#"
    ALTER TABLE invites ADD COLUMN revoked_at TEXT
    "#,
    // Answers a family member flagged as wrong/nonsensical with the
    // Report button. The row carries a SNAPSHOT of the question and the
    // answer the reporter chose to share — the host never reads a peer's
    // thread to fill this in, which is what keeps the "your chats are
    // invisible to the host" promise intact: reporting is the user
    // deliberately handing over one exchange.
    r#"
    CREATE TABLE IF NOT EXISTS reports (
        id           TEXT PRIMARY KEY,
        peer_id      TEXT NOT NULL,
        reporter     TEXT NOT NULL,
        message_id   TEXT NOT NULL,
        question     TEXT NOT NULL,
        answer       TEXT NOT NULL,
        model        TEXT NOT NULL DEFAULT '',
        slot         TEXT NOT NULL DEFAULT '',
        created_at   TEXT NOT NULL,
        reviewed_at  TEXT
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS reports_open
        ON reports(reviewed_at, created_at)
    "#,
    // One row per (reporter, message): the dedup guarantee belongs to the
    // database, not to a SELECT-then-INSERT that two concurrent sessions
    // sharing an invite can both win.
    r#"
    CREATE UNIQUE INDEX IF NOT EXISTS reports_peer_msg
        ON reports(peer_id, message_id)
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
    backfill_device_named_thread_titles(pool).await?;
    Ok(())
}

/// One-off repair for threads the host titled after the sending device.
///
/// Until 0.2.111 a client's thread row was born inside `run_chat_turn`
/// with `sender` as its title (see `network::server`), because
/// `commands::create_thread` has no `Mode::Client` branch and the host
/// therefore never heard of the thread until its first message. As
/// `upsert_thread` is INSERT OR IGNORE, that name was permanent: on the
/// household DB 57 of 61 non-Telegram client threads read as a device or
/// sender name instead of their topic.
///
/// The fingerprint below is deliberately narrow — it must never rewrite
/// a title a human chose:
///   * title equals the sender of the thread's FIRST message, and that
///     message is a `user` turn (an auto-stamp, by construction);
///   * the thread has not been touched since its last message
///     (`updated_at` == max(messages.created_at)). A manual rename bumps
///     `updated_at` past that, which is what excluded the three
///     hand-renamed threads in the household DB;
///   * "Telegram" rows are skipped outright — the bridge titles those on
///     purpose (`telegram::router`).
///
/// Writes `title` ONLY. `list_threads` orders by `updated_at DESC`, so
/// touching that column would re-sort every family member's sidebar into
/// migration order. Naturally idempotent: once rewritten the title no
/// longer equals the sender, so a second run matches nothing.
async fn backfill_device_named_thread_titles(pool: &SqlitePool) -> Result<()> {
    let rows = sqlx::query(
        r#"
        SELECT t.id AS id, m.content AS content
        FROM threads t
        JOIN messages m ON m.thread_id = t.id
        WHERE t.title <> 'Telegram'
          AND m.created_at = (SELECT MIN(created_at) FROM messages WHERE thread_id = t.id)
          AND m.role = 'user'
          AND t.title = m.sender
          AND t.updated_at = (SELECT MAX(created_at) FROM messages WHERE thread_id = t.id)
        "#,
    )
    .fetch_all(pool)
    .await?;

    let mut fixed = 0usize;
    for row in rows {
        let id: String = row.get("id");
        let content: String = row.get("content");
        let Some(title) = super::messages::derive_thread_title(&content) else {
            continue; // empty first message — keep the device name
        };
        sqlx::query("UPDATE threads SET title = ?1 WHERE id = ?2")
            .bind(&title)
            .bind(&id)
            .execute(pool)
            .await?;
        fixed += 1;
    }
    if fixed > 0 {
        tracing::info!("migrate: retitled {fixed} device-named thread(s) from their first message");
    }
    Ok(())
}
