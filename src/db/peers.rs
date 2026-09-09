//! The `peers` table — one row per family device identity (the invite
//! short code that is also `threads.peer_id`, `user_facts.peer_id` and
//! `telegram_links.peer_id`). Written when a device says Hello. Today it
//! carries the member's IANA time zone so reminders fire at their local
//! time; the host's own identity (`HOST_PEER`) never connects over the
//! socket and therefore never has a row.
//!
//! Privacy: nothing here is logged; `tz` is validated by the caller before
//! it is stored, because it later ends up inside a prompt.

use anyhow::Result;
use chrono::Utc;
use sqlx::{Row, SqlitePool};

/// Record a connect. `tz` is `None` when the device sent nothing usable;
/// COALESCE keeps a previously learned zone in that case, so an older
/// client reconnecting does not erase what a newer one taught us.
pub async fn upsert_on_connect(
    pool: &SqlitePool,
    id: &str,
    display_name: &str,
    tz: Option<&str>,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO peers (id, display_name, invite_id, first_seen, last_seen, tz)
         VALUES (?1, ?2, ?1, ?3, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET
           display_name = excluded.display_name,
           last_seen    = excluded.last_seen,
           tz           = COALESCE(excluded.tz, peers.tz)",
    )
    .bind(id)
    .bind(display_name)
    .bind(&now)
    .bind(tz)
    .execute(pool)
    .await?;
    Ok(())
}

/// The zone a device last reported, if any.
pub async fn tz(pool: &SqlitePool, peer_id: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT tz FROM peers WHERE id = ?1")
        .bind(peer_id)
        .fetch_optional(pool)
        .await?;
    Ok(row
        .and_then(|r| r.try_get::<Option<String>, _>("tz").ok().flatten())
        .filter(|s| !s.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        crate::db::migrate::run(&pool).await.expect("apply migrations");
        pool
    }

    #[tokio::test]
    async fn a_reconnect_without_a_zone_keeps_the_learned_one() {
        let pool = fresh_pool().await;
        upsert_on_connect(&pool, "ABC123", "Laptop", Some("Europe/Berlin")).await.unwrap();
        assert_eq!(tz(&pool, "ABC123").await.unwrap().as_deref(), Some("Europe/Berlin"));
        // An older client says Hello with no zone: nothing is erased.
        upsert_on_connect(&pool, "ABC123", "Laptop", None).await.unwrap();
        assert_eq!(tz(&pool, "ABC123").await.unwrap().as_deref(), Some("Europe/Berlin"));
        // A newer one moves house: the zone follows.
        upsert_on_connect(&pool, "ABC123", "Laptop", Some("America/New_York")).await.unwrap();
        assert_eq!(tz(&pool, "ABC123").await.unwrap().as_deref(), Some("America/New_York"));
        assert!(tz(&pool, "nobody").await.unwrap().is_none());
    }
}
