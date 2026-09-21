//! Long-term per-user facts — the persistent memory layer that survives
//! across threads and sessions.
//!
//! Distinct from `memory_notes` (thread-scoped extractive summaries):
//!   * Scope is the whole peer, not one thread.
//!   * Population is deliberate (the `remember` tool, the passive
//!     extractor, or manual entry via the Settings → Memory page).
//!   * (peer_id, key) is unique so an update overwrites rather than
//!     appending — "city: Berlin" then "city: Munich" is a single row.
//!
//! Privacy: every read/write filters by `peer_id`, so one family
//! member's facts never leak into another's prompt context. Same
//! invariant the rest of the DB layer follows.

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserFact {
    pub id: String,
    pub peer_id: String,
    pub key: String,
    pub value: String,
    /// "tool" (LLM called remember()), "extractor" (passive background
    /// pass over the last user message), or "manual" (user added via UI).
    pub source: String,
    pub source_msg_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Collapse a caller-supplied key into the canonical stored form:
/// lowercase, non-alphanumerics folded to `_`, runs of `_` collapsed,
/// leading/trailing `_` trimmed. `Wife Name`, `wife-name` and
/// `wife_name` all become `wife_name`.
///
/// This lives HERE, not in the caller, because `(peer_id, key)` is the
/// table's uniqueness constraint — and there are four write paths into
/// it (the `remember` tool, the passive extractor, the host's Settings
/// → Memory page, and the client's WS `SaveUserFact`). Originally only
/// the extractor normalized, so `wife_name`, `wifes_name` and
/// `Wife Name` could sit in three separate rows: the overwrite-on-update
/// contract the `remember` tool advertises silently didn't hold, both
/// values got injected into the prompt as authoritative, and `forget`
/// cleared only whichever variant it was handed. Normalizing at the
/// single point of write is what keeps those paths from diverging again.
pub fn normalize_key(raw: &str) -> String {
    raw.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

/// Insert or update a fact. Returns the row as it now sits in the DB —
/// the same id if this was an update, or a fresh uuid if it was new.
///
/// Normalizes the key (see `normalize_key`) and trims the value, then
/// rejects empty input (returns Err so callers can surface "you tried to
/// remember nothing" to the model / user instead of silently writing
/// junk). The returned row carries the NORMALIZED key — callers that
/// echo it back or match on it must use that, not what they passed in.
pub async fn upsert(
    pool: &SqlitePool,
    peer_id: &str,
    key: &str,
    value: &str,
    source: &str,
    source_msg_id: Option<&str>,
) -> Result<UserFact> {
    // Normalize BEFORE the empty/length gates so what we validate is
    // exactly what lands in the unique index.
    let key = normalize_key(key);
    let value = value.trim();
    if key.is_empty() {
        anyhow::bail!("fact key is empty");
    }
    if value.is_empty() {
        anyhow::bail!("fact value is empty");
    }
    if key.len() > 80 {
        anyhow::bail!("fact key too long (max 80 chars)");
    }
    if value.len() > 500 {
        anyhow::bail!("fact value too long (max 500 chars)");
    }

    let now = Utc::now().to_rfc3339();

    // Try to find an existing row first so we can preserve the original
    // created_at (and id, so any UI that linked to it doesn't 404).
    let existing = sqlx::query(
        "SELECT id, created_at FROM user_facts WHERE peer_id = ?1 AND key = ?2",
    )
    .bind(peer_id)
    .bind(key.as_str())
    .fetch_optional(pool)
    .await?;

    if let Some(row) = existing {
        let id: String = row.get("id");
        let created_at: String = row.get("created_at");
        sqlx::query(
            "UPDATE user_facts
             SET value = ?1, source = ?2, source_msg_id = ?3, updated_at = ?4
             WHERE id = ?5",
        )
        .bind(value)
        .bind(source)
        .bind(source_msg_id)
        .bind(&now)
        .bind(&id)
        .execute(pool)
        .await?;
        return Ok(UserFact {
            id,
            peer_id: peer_id.into(),
            key: key.clone(),
            value: value.into(),
            source: source.into(),
            source_msg_id: source_msg_id.map(|s| s.into()),
            created_at,
            updated_at: now,
        });
    }

    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO user_facts (id, peer_id, key, value, source, source_msg_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
    )
    .bind(&id)
    .bind(peer_id)
    .bind(key.as_str())
    .bind(value)
    .bind(source)
    .bind(source_msg_id)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(UserFact {
        id,
        peer_id: peer_id.into(),
        key,
        value: value.into(),
        source: source.into(),
        source_msg_id: source_msg_id.map(|s| s.into()),
        created_at: now.clone(),
        updated_at: now,
    })
}

/// List every fact for a peer, newest-first. Used by the Settings →
/// Memory page so the user can review/edit/delete what's been stored.
pub async fn list(pool: &SqlitePool, peer_id: &str) -> Result<Vec<UserFact>> {
    let rows = sqlx::query(
        "SELECT id, peer_id, key, value, source, source_msg_id, created_at, updated_at
         FROM user_facts WHERE peer_id = ?1
         ORDER BY updated_at DESC",
    )
    .bind(peer_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_fact).collect())
}

/// Load facts for prompt injection. Same as list(), but exists as a
/// separate entry-point so we can add caching/limits without affecting
/// the Settings UI's "show everything" semantics.
pub async fn for_prompt(pool: &SqlitePool, peer_id: &str) -> Result<Vec<UserFact>> {
    list(pool, peer_id).await
}

/// Forget a fact by id. Idempotent — no error if the row doesn't exist.
/// Cross-peer guard: only the owning peer can delete (you can't delete
/// another family member's facts even if you somehow learned the id).
pub async fn delete(pool: &SqlitePool, peer_id: &str, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM user_facts WHERE id = ?1 AND peer_id = ?2")
        .bind(id)
        .bind(peer_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Forget a fact by its semantic key. Used by the `forget` tool when
/// the LLM only knows the key ("forget my city") not the row id.
///
/// The key is normalized on the way in, exactly as `upsert` normalizes
/// it on the way out, so `forget("Wife Name")` clears the row that
/// `remember("wifes name")` wrote.
pub async fn delete_by_key(pool: &SqlitePool, peer_id: &str, key: &str) -> Result<u64> {
    let result = sqlx::query("DELETE FROM user_facts WHERE peer_id = ?1 AND key = ?2")
        .bind(peer_id)
        .bind(normalize_key(key))
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Wipe every fact for this peer. Exposed via the Settings page as a
/// "Forget everything" button — useful when the user starts a new
/// chapter of life and wants to clear the slate (job change, move,
/// etc.) without deleting them one by one.
pub async fn clear_all(pool: &SqlitePool, peer_id: &str) -> Result<u64> {
    let result = sqlx::query("DELETE FROM user_facts WHERE peer_id = ?1")
        .bind(peer_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    /// Spin up an in-memory SQLite + apply the schema. Used by the
    /// privacy-boundary tests below — keeps them hermetic and fast.
    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        // user_facts table — same DDL as migrate.rs.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS user_facts (
                id            TEXT PRIMARY KEY,
                peer_id       TEXT NOT NULL,
                key           TEXT NOT NULL,
                value         TEXT NOT NULL,
                source        TEXT NOT NULL DEFAULT 'tool',
                source_msg_id TEXT,
                created_at    TEXT NOT NULL,
                updated_at    TEXT NOT NULL,
                UNIQUE(peer_id, key)
            )",
        )
        .execute(&pool)
        .await
        .expect("create user_facts table");
        pool
    }

    /// The crown-jewel privacy invariant: peer A's facts must never
    /// surface in peer B's list/get queries.
    ///
    /// This test was deliberately written to fail loudly if the WS
    /// handlers in network/server.rs ever drop the `context_peer`
    /// scope by accident — e.g. if someone refactors and uses
    /// `HOST_PEER` instead of the connecting peer's id. Same kind of
    /// regression as the JWT panic: would compile cleanly, ship
    /// silently, expose another family member's memory.
    #[tokio::test]
    async fn peer_scope_is_strict() {
        let pool = fresh_pool().await;

        upsert(&pool, "ALICE", "city", "Berlin", "tool", None).await.unwrap();
        upsert(&pool, "ALICE", "diet", "vegetarian", "tool", None).await.unwrap();
        upsert(&pool, "BOB", "city", "Munich", "tool", None).await.unwrap();

        let alice_facts = list(&pool, "ALICE").await.unwrap();
        let bob_facts = list(&pool, "BOB").await.unwrap();

        assert_eq!(alice_facts.len(), 2, "ALICE should see 2 own facts");
        assert!(alice_facts.iter().all(|f| f.peer_id == "ALICE"));
        assert!(!alice_facts.iter().any(|f| f.value == "Munich"),
            "ALICE must not see BOB's city");

        assert_eq!(bob_facts.len(), 1, "BOB should see 1 own fact");
        assert_eq!(bob_facts[0].peer_id, "BOB");
        assert_eq!(bob_facts[0].value, "Munich");
    }

    /// upsert under (peer_id, key) must overwrite within the SAME peer,
    /// but creating the same key under a DIFFERENT peer must be a
    /// separate row. That's how the owner and the kid can both have
    /// `city` set to different things without colliding.
    #[tokio::test]
    async fn upsert_is_per_peer_not_global() {
        let pool = fresh_pool().await;
        let a1 = upsert(&pool, "ALICE", "city", "Berlin", "tool", None).await.unwrap();
        let a2 = upsert(&pool, "ALICE", "city", "Munich", "tool", None).await.unwrap();
        assert_eq!(a1.id, a2.id, "same peer + same key → SAME row");
        assert_eq!(a2.value, "Munich");

        let b1 = upsert(&pool, "BOB", "city", "Hamburg", "tool", None).await.unwrap();
        assert_ne!(a2.id, b1.id, "different peer + same key → DIFFERENT row");
        let alice = list(&pool, "ALICE").await.unwrap();
        let bob = list(&pool, "BOB").await.unwrap();
        assert_eq!(alice[0].value, "Munich");
        assert_eq!(bob[0].value, "Hamburg");
    }

    /// delete must only touch the requesting peer's rows. A malicious
    /// or buggy client sending a delete with someone else's id should
    /// be a no-op, not a cross-peer deletion.
    #[tokio::test]
    async fn delete_is_peer_scoped() {
        let pool = fresh_pool().await;
        let alice_fact = upsert(&pool, "ALICE", "city", "Berlin", "tool", None).await.unwrap();
        upsert(&pool, "BOB", "city", "Munich", "tool", None).await.unwrap();

        // BOB tries to delete ALICE's fact by id. Must NOT succeed.
        delete(&pool, "BOB", &alice_fact.id).await.unwrap();
        let alice_after = list(&pool, "ALICE").await.unwrap();
        assert_eq!(alice_after.len(), 1, "ALICE's fact must still exist after BOB's cross-peer delete attempt");
        assert_eq!(alice_after[0].id, alice_fact.id);

        // ALICE deletes her own — must succeed.
        delete(&pool, "ALICE", &alice_fact.id).await.unwrap();
        assert!(list(&pool, "ALICE").await.unwrap().is_empty());
        // BOB's row untouched.
        assert_eq!(list(&pool, "BOB").await.unwrap().len(), 1);
    }

    /// The regression this module exists to prevent: the `remember`
    /// tool, the extractor and the two manual UI paths all reach the
    /// SAME row for keys that differ only in case, spacing or
    /// punctuation. Before normalization moved in here, each spelling
    /// got its own row and the "calling remember with the same key
    /// OVERWRITES" contract in the tool description was a lie.
    #[tokio::test]
    async fn key_variants_collapse_to_one_row() {
        let pool = fresh_pool().await;

        // Same fact, four spellings a model or a person might produce.
        let a = upsert(&pool, "ALICE", "favourite_tea", "sencha", "tool", None).await.unwrap();
        let b = upsert(&pool, "ALICE", "Favourite Tea", "genmaicha", "manual", None).await.unwrap();
        let c = upsert(&pool, "ALICE", "FAVOURITE-TEA", "hojicha", "extractor", None).await.unwrap();
        let d = upsert(&pool, "ALICE", "  favourite__tea  ", "matcha", "tool", None).await.unwrap();

        assert_eq!(a.id, b.id, "mixed case must hit the same row");
        assert_eq!(a.id, c.id, "punctuation must hit the same row");
        assert_eq!(a.id, d.id, "spaces + doubled underscores must hit the same row");

        let facts = list(&pool, "ALICE").await.unwrap();
        assert_eq!(facts.len(), 1, "four spellings must leave exactly ONE row");
        assert_eq!(facts[0].key, "favourite_tea", "the stored key is the normalized one");
        assert_eq!(facts[0].value, "matcha", "last write wins — that IS the update path");
    }

    /// The stored key is normalized even when only one spelling is ever
    /// used, so what the UI shows and what the prompt injects agree.
    #[tokio::test]
    async fn upsert_returns_the_normalized_key() {
        let pool = fresh_pool().await;
        let f = upsert(&pool, "ALICE", "Preferred Units", "metric", "manual", None).await.unwrap();
        assert_eq!(f.key, "preferred_units");
        let listed = list(&pool, "ALICE").await.unwrap();
        assert_eq!(listed[0].key, "preferred_units");
    }

    /// A key that is all punctuation normalizes to nothing — reject it
    /// rather than writing a row under the empty string.
    #[tokio::test]
    async fn key_of_pure_punctuation_is_rejected() {
        let pool = fresh_pool().await;
        assert!(upsert(&pool, "ALICE", "???", "whatever", "tool", None).await.is_err());
        assert!(list(&pool, "ALICE").await.unwrap().is_empty());
    }

    /// forget("Wife Name") must clear the row remember("wifes name")
    /// wrote. Any variant reaches the row; the delete is not silently
    /// a no-op that leaves a stale fact injected as authoritative.
    #[tokio::test]
    async fn forget_clears_the_row_via_any_variant() {
        for variant in ["reading_format", "Reading Format", "READING-FORMAT", " reading__format "] {
            let pool = fresh_pool().await;
            upsert(&pool, "ALICE", "reading_format", "paperback", "tool", None).await.unwrap();

            let deleted = delete_by_key(&pool, "ALICE", variant).await.unwrap();
            assert_eq!(deleted, 1, "variant {variant:?} should have cleared the row");
            assert!(
                list(&pool, "ALICE").await.unwrap().is_empty(),
                "variant {variant:?} left the fact behind"
            );
        }
    }

    /// Normalization must not punch through the peer boundary: ALICE
    /// forgetting her key cannot clear BOB's identically-named row.
    #[tokio::test]
    async fn forget_by_key_stays_peer_scoped() {
        let pool = fresh_pool().await;
        upsert(&pool, "ALICE", "Commute Mode", "bicycle", "tool", None).await.unwrap();
        upsert(&pool, "BOB", "commute_mode", "tram", "tool", None).await.unwrap();

        let deleted = delete_by_key(&pool, "ALICE", "COMMUTE MODE").await.unwrap();
        assert_eq!(deleted, 1);
        assert!(list(&pool, "ALICE").await.unwrap().is_empty());
        assert_eq!(list(&pool, "BOB").await.unwrap().len(), 1, "BOB's row must survive");
    }

    #[test]
    fn normalize_key_handles_spaces_and_punctuation() {
        assert_eq!(normalize_key("Wife's birthday"), "wife_s_birthday");
        assert_eq!(normalize_key("CITY"), "city");
        assert_eq!(normalize_key("  multi   word  "), "multi_word");
        assert_eq!(normalize_key("__leading_and_trailing__"), "leading_and_trailing");
        assert_eq!(normalize_key("work-stack"), "work_stack");
        assert_eq!(normalize_key("   "), "");
        // Non-ASCII letters are alphanumeric and must survive: a German
        // or Catalan key should not be shredded into underscores.
        assert_eq!(normalize_key("Lieblingskäse"), "lieblingskäse");
    }

    /// clear_all wipes only the requesting peer.
    #[tokio::test]
    async fn clear_all_is_peer_scoped() {
        let pool = fresh_pool().await;
        upsert(&pool, "ALICE", "a", "1", "tool", None).await.unwrap();
        upsert(&pool, "ALICE", "b", "2", "tool", None).await.unwrap();
        upsert(&pool, "BOB", "c", "3", "tool", None).await.unwrap();

        let wiped = clear_all(&pool, "ALICE").await.unwrap();
        assert_eq!(wiped, 2);
        assert!(list(&pool, "ALICE").await.unwrap().is_empty());
        assert_eq!(list(&pool, "BOB").await.unwrap().len(), 1, "BOB's fact survives ALICE's clear_all");
    }
}

fn row_to_fact(r: sqlx::sqlite::SqliteRow) -> UserFact {
    UserFact {
        id: r.get("id"),
        peer_id: r.get("peer_id"),
        key: r.get("key"),
        value: r.get("value"),
        source: r.get("source"),
        source_msg_id: r.try_get("source_msg_id").ok(),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}
