//! Reminders — per-member timed nudges. A family member sets one by asking
//! KinAI ("remind me tomorrow at 9 to …"); the host's scheduler delivers it
//! when due to the member's own devices and Telegram; the member
//! acknowledges or snoozes it from the popup or the Calendar.
//!
//! Privacy: every read and write except `lease_due` / `release_stuck`
//! filters by `peer_id` — the same invariant as user_facts, so a forged id
//! from another member is a no-op. Those two are the ONE deliberate
//! cross-peer path: the scheduler runs once for the household and routes
//! each leased row by its own `peer_id`. Nothing in this module logs `text`.

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

pub const STATUS_SCHEDULED: &str = "scheduled";
pub const STATUS_FIRING: &str = "firing";
pub const STATUS_FIRED: &str = "fired";
pub const STATUS_DONE: &str = "done";
pub const STATUS_CANCELLED: &str = "cancelled";

/// Longest reminder text accepted, in characters. Roughly a hundred
/// words: enough for "read X, pull out the three points on Y, then send
/// Z a summary" with a link on the end. The popup collapses anything
/// past a few lines behind a "Show more", so a long one costs the reader
/// nothing until they want it.
pub const MAX_TEXT_CHARS: usize = 600;
/// Shortest id prefix `find_by_prefix` accepts.
pub const MIN_PREFIX: usize = 6;

/// The wire shape too: it rides in `Envelope::Reminders` and in the
/// `kinai://reminder` event, mirrored field-for-field by the TypeScript
/// `Reminder` interface. Keep field names and nullability in step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reminder {
    pub id: String,
    pub peer_id: String,
    pub thread_id: Option<String>,
    pub text: String,
    /// UTC RFC3339 — the next moment it fires.
    pub due_at: String,
    /// IANA zone the member used ("Europe/Berlin"); "" when unknown.
    pub tz: String,
    /// "YYYY-MM-DDTHH:MM" in `tz`. What every surface DISPLAYS; recomputed
    /// on snooze so it never drifts from `due_at`.
    pub due_local: String,
    pub repeat: String,
    pub status: String,
    pub fired_at: Option<String>,
    pub source_msg_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

const COLS: &str = "id, peer_id, thread_id, text, due_at, tz, due_local, repeat, status, \
                    fired_at, source_msg_id, created_at, updated_at";

/// Render a UTC instant as the member's wall-clock. Falls back to the
/// host's own zone when `tz` is empty or unknown, which is also what the
/// host user gets.
pub fn local_display(at: DateTime<Utc>, tz: &str) -> String {
    const FMT: &str = "%Y-%m-%dT%H:%M";
    match tz.trim().parse::<chrono_tz::Tz>() {
        Ok(zone) => at.with_timezone(&zone).format(FMT).to_string(),
        Err(_) => at.with_timezone(&chrono::Local).format(FMT).to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &SqlitePool,
    peer_id: &str,
    thread_id: Option<&str>,
    text: &str,
    due_at: DateTime<Utc>,
    tz: &str,
    source_msg_id: Option<&str>,
) -> Result<Reminder> {
    let text = text.trim();
    if text.is_empty() {
        bail!("reminder text is empty");
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        bail!("reminder text is too long (max {MAX_TEXT_CHARS} characters)");
    }
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let due = due_at.to_rfc3339();
    let due_local = local_display(due_at, tz);
    sqlx::query(
        "INSERT INTO reminders (id, peer_id, thread_id, text, due_at, tz, due_local, repeat,
                                status, fired_at, source_msg_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '', 'scheduled', NULL, ?8, ?9, ?9)",
    )
    .bind(&id)
    .bind(peer_id)
    .bind(thread_id)
    .bind(text)
    .bind(&due)
    .bind(tz)
    .bind(&due_local)
    .bind(source_msg_id)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(Reminder {
        id,
        peer_id: peer_id.into(),
        thread_id: thread_id.map(|s| s.into()),
        text: text.into(),
        due_at: due,
        tz: tz.into(),
        due_local,
        repeat: String::new(),
        status: STATUS_SCHEDULED.into(),
        fired_at: None,
        source_msg_id: source_msg_id.map(|s| s.into()),
        created_at: now.clone(),
        updated_at: now,
    })
}

/// A member's reminders, soonest first. Cancelled rows are history and
/// stay out unless asked for.
pub async fn list_for_peer(
    pool: &SqlitePool,
    peer_id: &str,
    include_cancelled: bool,
) -> Result<Vec<Reminder>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM reminders
         WHERE peer_id = ?1 AND (?2 OR status != 'cancelled')
         ORDER BY datetime(due_at) ASC, created_at ASC"
    ))
    .bind(peer_id)
    .bind(include_cancelled)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_reminder).collect())
}

/// What the MODEL is shown when a member asks "what reminders do I have?".
/// Only live rows — a finished reminder is history for the Calendar, not
/// something to read back — soonest first, and bounded: after months of
/// daily use an unbounded oldest-first list would push the upcoming ones
/// past the tool-result cap and out of the model's view entirely.
pub async fn list_live_for_peer(
    pool: &SqlitePool,
    peer_id: &str,
    limit: i64,
) -> Result<Vec<Reminder>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM reminders
         WHERE peer_id = ?1 AND status IN ('scheduled', 'firing', 'fired')
         ORDER BY datetime(due_at) ASC, created_at ASC
         LIMIT ?2"
    ))
    .bind(peer_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_reminder).collect())
}

/// How many live reminders a member has, so a truncated list can say so.
pub async fn count_live_for_peer(pool: &SqlitePool, peer_id: &str) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM reminders
         WHERE peer_id = ?1 AND status IN ('scheduled', 'firing', 'fired')",
    )
    .bind(peer_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

pub async fn get(pool: &SqlitePool, peer_id: &str, id: &str) -> Result<Option<Reminder>> {
    let row = sqlx::query(&format!(
        "SELECT {COLS} FROM reminders WHERE id = ?1 AND peer_id = ?2"
    ))
    .bind(id)
    .bind(peer_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_reminder))
}

/// Live (not done, not cancelled) reminders whose id starts with `prefix`.
/// The model works with short ids, so the tool resolves them here.
pub async fn find_by_prefix(
    pool: &SqlitePool,
    peer_id: &str,
    prefix: &str,
) -> Result<Vec<Reminder>> {
    // Strip LIKE wildcards BEFORE measuring: a model that was told "at
    // least 6 characters" but has no id to hand can emit "______", which
    // would otherwise pass the length check, strip to nothing, and match
    // every live reminder — cancelling an arbitrary one.
    let cleaned: String = prefix
        .trim()
        .chars()
        .filter(|c| !matches!(c, '%' | '_'))
        .collect();
    if cleaned.chars().count() < MIN_PREFIX {
        bail!("give at least {MIN_PREFIX} characters of the reminder id");
    }
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM reminders
         WHERE peer_id = ?1 AND id LIKE ?2 AND status IN ('scheduled', 'firing', 'fired')
         ORDER BY datetime(due_at) ASC"
    ))
    .bind(peer_id)
    .bind(format!("{cleaned}%"))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_reminder).collect())
}

/// Does this id (or prefix) name a reminder that is already finished?
/// Lets `cancel_reminder` say "that one is already done" instead of
/// "no such reminder", which reads as a bug to the member.
pub async fn finished_by_prefix(
    pool: &SqlitePool,
    peer_id: &str,
    prefix: &str,
) -> Result<Vec<Reminder>> {
    let cleaned: String = prefix
        .trim()
        .chars()
        .filter(|c| !matches!(c, '%' | '_'))
        .collect();
    if cleaned.chars().count() < MIN_PREFIX {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM reminders
         WHERE peer_id = ?1 AND id LIKE ?2 AND status IN ('done', 'cancelled')
         ORDER BY datetime(due_at) ASC"
    ))
    .bind(peer_id)
    .bind(format!("{cleaned}%"))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_reminder).collect())
}

/// CROSS-PEER BY DESIGN. Atomically flips every due `scheduled` row to
/// `firing` and returns them; a second call with the same `now` returns
/// nothing, so two ticks can never deliver the same reminder twice. Both
/// sides go through datetime(): the stored strings carry a 'T' and an
/// offset, and a raw text compare against a space-separated clock would
/// miss every same-day row.
pub async fn lease_due(pool: &SqlitePool, now: DateTime<Utc>) -> Result<Vec<Reminder>> {
    let stamp = Utc::now().to_rfc3339();
    let rows = sqlx::query(&format!(
        "UPDATE reminders SET status = 'firing', updated_at = ?1
         WHERE status = 'scheduled' AND datetime(due_at) <= datetime(?2)
         RETURNING {COLS}"
    ))
    .bind(&stamp)
    .bind(now.to_rfc3339())
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_reminder).collect())
}

/// CROSS-PEER BY DESIGN. A row left in `firing` (the host died between
/// lease and delivery) goes back to `scheduled` once its lease is older
/// than `older_than`, so the next tick delivers it. Returns how many.
pub async fn release_stuck(pool: &SqlitePool, older_than: DateTime<Utc>) -> Result<u64> {
    let r = sqlx::query(
        "UPDATE reminders SET status = 'scheduled', updated_at = ?1
         WHERE status = 'firing' AND datetime(updated_at) <= datetime(?2)",
    )
    .bind(Utc::now().to_rfc3339())
    .bind(older_than.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// Delivery happened (or was attempted — the row is the source of truth
/// and a device that was offline reads it on its next launch).
pub async fn mark_fired(pool: &SqlitePool, peer_id: &str, id: &str) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let r = sqlx::query(
        "UPDATE reminders SET status = 'fired', fired_at = ?1, updated_at = ?1
         WHERE id = ?2 AND peer_id = ?3 AND status = 'firing'",
    )
    .bind(&now)
    .bind(id)
    .bind(peer_id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Push a reminder `minutes` into the future and back to `scheduled`,
/// re-rendering `due_local` in the member's zone. `None` when the id is not
/// this member's or the row is already done/cancelled.
pub async fn snooze(
    pool: &SqlitePool,
    peer_id: &str,
    id: &str,
    minutes: i64,
) -> Result<Option<Reminder>> {
    if !(1..=7 * 24 * 60).contains(&minutes) {
        bail!("snooze must be between 1 minute and 7 days");
    }
    let Some(current) = get(pool, peer_id, id).await? else {
        return Ok(None);
    };
    if !matches!(
        current.status.as_str(),
        STATUS_SCHEDULED | STATUS_FIRING | STATUS_FIRED
    ) {
        return Ok(None);
    }
    let now = Utc::now();
    let new_due = now + Duration::minutes(minutes);
    let due_local = local_display(new_due, &current.tz);
    let row = sqlx::query(&format!(
        "UPDATE reminders
         SET due_at = ?1, due_local = ?2, status = 'scheduled', fired_at = NULL, updated_at = ?3
         WHERE id = ?4 AND peer_id = ?5 AND status IN ('scheduled', 'firing', 'fired')
         RETURNING {COLS}"
    ))
    .bind(new_due.to_rfc3339())
    .bind(&due_local)
    .bind(now.to_rfc3339())
    .bind(id)
    .bind(peer_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_reminder))
}

/// The member says "got it". Allowed from any live state — acknowledging
/// something early is a fine way to be done with it.
pub async fn acknowledge(pool: &SqlitePool, peer_id: &str, id: &str) -> Result<bool> {
    let r = sqlx::query(
        "UPDATE reminders SET status = 'done', updated_at = ?1
         WHERE id = ?2 AND peer_id = ?3 AND status IN ('scheduled', 'firing', 'fired')",
    )
    .bind(Utc::now().to_rfc3339())
    .bind(id)
    .bind(peer_id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Cancelled via the tool; kept as history, never listed.
pub async fn cancel(pool: &SqlitePool, peer_id: &str, id: &str) -> Result<bool> {
    let r = sqlx::query(
        "UPDATE reminders SET status = 'cancelled', updated_at = ?1
         WHERE id = ?2 AND peer_id = ?3 AND status IN ('scheduled', 'firing', 'fired')",
    )
    .bind(Utc::now().to_rfc3339())
    .bind(id)
    .bind(peer_id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Hard delete from the Calendar. Idempotent, peer-scoped.
pub async fn delete(pool: &SqlitePool, peer_id: &str, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM reminders WHERE id = ?1 AND peer_id = ?2")
        .bind(id)
        .bind(peer_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The one entry point the popup, the Calendar and the client protocol all
/// go through. `Ok(None)` after a delete; `Err` for an unknown id, a
/// finished row, or a bad action — the caller turns that into an honest
/// message for the member.
pub async fn apply_action(
    pool: &SqlitePool,
    peer_id: &str,
    id: &str,
    action: &str,
    snooze_minutes: u32,
) -> Result<Option<Reminder>> {
    match action {
        "ack" => {
            if !acknowledge(pool, peer_id, id).await? {
                return Err(anyhow!("that reminder is not yours to act on, or it is already done"));
            }
            get(pool, peer_id, id).await
        }
        "snooze" => {
            let minutes = if snooze_minutes == 0 { 10 } else { snooze_minutes as i64 };
            snooze(pool, peer_id, id, minutes)
                .await?
                .map(Some)
                .ok_or_else(|| anyhow!("that reminder is not yours to act on, or it is already done"))
        }
        "delete" => {
            delete(pool, peer_id, id).await?;
            Ok(None)
        }
        other => Err(anyhow!("unknown reminder action: {other}")),
    }
}

fn row_to_reminder(r: sqlx::sqlite::SqliteRow) -> Reminder {
    // Nullable columns decode as Option<String>: sqlx turns a NULL TEXT
    // into "" when asked for a plain String, so `try_get(..).ok()` would
    // report Some("") for an unset fired_at.
    let opt = |col: &str| -> Option<String> {
        r.try_get::<Option<String>, _>(col)
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
    };
    Reminder {
        id: r.get("id"),
        peer_id: r.get("peer_id"),
        thread_id: opt("thread_id"),
        text: r.get("text"),
        due_at: r.get("due_at"),
        tz: r.get("tz"),
        due_local: r.get("due_local"),
        repeat: r.get("repeat"),
        status: r.get("status"),
        fired_at: opt("fired_at"),
        source_msg_id: opt("source_msg_id"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use sqlx::sqlite::SqlitePoolOptions;

    /// In-memory DB with the REAL migrations applied, so the DDL in
    /// migrate.rs is exercised. One connection: each connection to
    /// `sqlite::memory:` is its own database.
    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        crate::db::migrate::run(&pool).await.expect("apply migrations");
        pool
    }

    fn at(offset: Duration) -> DateTime<Utc> {
        Utc::now() + offset
    }

    async fn seed(pool: &SqlitePool, peer: &str, text: &str, due: DateTime<Utc>) -> Reminder {
        create(pool, peer, Some("thread-1"), text, due, "Europe/Berlin", None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn peer_scope_is_strict() {
        let pool = fresh_pool().await;
        seed(&pool, "ALICE", "water the plants", at(Duration::hours(1))).await;
        seed(&pool, "ALICE", "call the dentist", at(Duration::hours(2))).await;
        seed(&pool, "BOB", "take out the bins", at(Duration::hours(1))).await;
        let alice = list_for_peer(&pool, "ALICE", false).await.unwrap();
        assert_eq!(alice.len(), 2);
        assert!(alice.iter().all(|r| r.peer_id == "ALICE"));
        assert!(alice.iter().all(|r| !r.text.contains("bins")));
    }

    #[tokio::test]
    async fn create_renders_the_members_wall_clock() {
        let pool = fresh_pool().await;
        // 07:00 UTC on a summer day is 09:00 in Berlin.
        let due = Utc.with_ymd_and_hms(2030, 7, 1, 7, 0, 0).unwrap();
        let r = seed(&pool, "ALICE", "stand-up", due).await;
        assert_eq!(r.due_local, "2030-07-01T09:00");
        assert!(r.due_at.ends_with("+00:00"), "stored as UTC: {}", r.due_at);
        // Validation.
        assert!(create(&pool, "ALICE", None, "   ", due, "Europe/Berlin", None).await.is_err());
        let long = "x".repeat(MAX_TEXT_CHARS + 1);
        assert!(create(&pool, "ALICE", None, &long, due, "Europe/Berlin", None).await.is_err());
    }

    #[tokio::test]
    async fn lease_due_flips_only_due_rows_and_is_idempotent() {
        let pool = fresh_pool().await;
        seed(&pool, "ALICE", "overdue by an hour", at(Duration::hours(-1))).await;
        seed(&pool, "BOB", "overdue by a minute", at(Duration::minutes(-1))).await;
        let later = seed(&pool, "ALICE", "not yet", at(Duration::hours(1))).await;

        let leased = lease_due(&pool, Utc::now()).await.unwrap();
        assert_eq!(leased.len(), 2, "both due rows, across peers on purpose");
        assert!(leased.iter().all(|r| r.status == STATUS_FIRING));
        let peers: Vec<&str> = leased.iter().map(|r| r.peer_id.as_str()).collect();
        assert!(peers.contains(&"ALICE") && peers.contains(&"BOB"));

        assert!(lease_due(&pool, Utc::now()).await.unwrap().is_empty(), "second lease is empty");
        let still = get(&pool, "ALICE", &later.id).await.unwrap().unwrap();
        assert_eq!(still.status, STATUS_SCHEDULED);
    }

    #[tokio::test]
    async fn same_day_row_leases_when_due() {
        // The datetime() regression: a due time earlier TODAY must lease.
        // A raw string compare against a space-separated clock misses it.
        let pool = fresh_pool().await;
        seed(&pool, "ALICE", "earlier today", at(Duration::minutes(-30))).await;
        assert_eq!(lease_due(&pool, Utc::now()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stuck_firing_rows_are_released_after_the_grace_period() {
        let pool = fresh_pool().await;
        seed(&pool, "ALICE", "stuck", at(Duration::minutes(-5))).await;
        let leased = lease_due(&pool, Utc::now()).await.unwrap();
        assert_eq!(leased.len(), 1);
        // Younger than the grace period: untouched.
        assert_eq!(release_stuck(&pool, Utc::now() - Duration::minutes(2)).await.unwrap(), 0);
        // Grace period elapsed (older_than in the future relative to the lease): released.
        assert_eq!(release_stuck(&pool, Utc::now() + Duration::seconds(1)).await.unwrap(), 1);
        assert_eq!(lease_due(&pool, Utc::now()).await.unwrap().len(), 1, "delivered again");
    }

    #[tokio::test]
    async fn actions_are_peer_scoped() {
        let pool = fresh_pool().await;
        let r = seed(&pool, "ALICE", "mine", at(Duration::minutes(-1))).await;
        lease_due(&pool, Utc::now()).await.unwrap();
        assert!(mark_fired(&pool, "ALICE", &r.id).await.unwrap());

        assert!(snooze(&pool, "BOB", &r.id, 10).await.unwrap().is_none());
        assert!(!acknowledge(&pool, "BOB", &r.id).await.unwrap());
        assert!(!cancel(&pool, "BOB", &r.id).await.unwrap());
        delete(&pool, "BOB", &r.id).await.unwrap();
        assert!(get(&pool, "ALICE", &r.id).await.unwrap().is_some(), "still there");
        assert!(apply_action(&pool, "BOB", &r.id, "ack", 0).await.is_err());

        let snoozed = apply_action(&pool, "ALICE", &r.id, "snooze", 15).await.unwrap().unwrap();
        assert_eq!(snoozed.status, STATUS_SCHEDULED);
        assert!(snoozed.fired_at.is_none());
        assert_ne!(snoozed.due_local, r.due_local, "due_local re-rendered after snooze");
        let done = apply_action(&pool, "ALICE", &r.id, "ack", 0).await.unwrap().unwrap();
        assert_eq!(done.status, STATUS_DONE);
        assert!(apply_action(&pool, "ALICE", &r.id, "delete", 0).await.unwrap().is_none());
        assert!(get(&pool, "ALICE", &r.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn snooze_returns_a_row_to_the_lease_queue() {
        let pool = fresh_pool().await;
        let r = seed(&pool, "ALICE", "again later", at(Duration::minutes(-1))).await;
        lease_due(&pool, Utc::now()).await.unwrap();
        mark_fired(&pool, "ALICE", &r.id).await.unwrap();
        let s = snooze(&pool, "ALICE", &r.id, 5).await.unwrap().unwrap();
        assert_eq!(s.status, STATUS_SCHEDULED);
        assert!(lease_due(&pool, Utc::now()).await.unwrap().is_empty(), "not due yet");
        assert_eq!(lease_due(&pool, Utc::now() + Duration::minutes(10)).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancelled_rows_stay_out_of_lists_and_leases() {
        let pool = fresh_pool().await;
        let r = seed(&pool, "ALICE", "never mind", at(Duration::minutes(-1))).await;
        assert!(cancel(&pool, "ALICE", &r.id).await.unwrap());
        assert!(list_for_peer(&pool, "ALICE", false).await.unwrap().is_empty());
        assert_eq!(list_for_peer(&pool, "ALICE", true).await.unwrap().len(), 1);
        assert!(lease_due(&pool, Utc::now()).await.unwrap().is_empty());
        assert!(!cancel(&pool, "ALICE", &r.id).await.unwrap(), "cancel is not repeatable");
    }

    #[tokio::test]
    async fn a_wildcard_prefix_cannot_match_everything() {
        // "______" is six characters but zero id — before the fix it passed
        // the length check, stripped to nothing, and LIKE '%' matched every
        // live reminder, so cancel_reminder could kill an arbitrary one.
        let pool = fresh_pool().await;
        seed(&pool, "ALICE", "one", at(Duration::hours(1))).await;
        seed(&pool, "ALICE", "two", at(Duration::hours(2))).await;
        for probe in ["______", "%%%%%%", "__%__%", "%", ""] {
            assert!(
                find_by_prefix(&pool, "ALICE", probe).await.is_err(),
                "{probe:?} must be refused, not treated as a wildcard"
            );
        }
    }

    #[tokio::test]
    async fn the_models_list_is_live_only_and_bounded() {
        let pool = fresh_pool().await;
        // Finished rows are history: the Calendar keeps them, the model's
        // list must not, or they crowd out what is actually upcoming.
        let done = seed(&pool, "ALICE", "already handled", at(Duration::hours(-3))).await;
        assert!(acknowledge(&pool, "ALICE", &done.id).await.unwrap());
        let cancelled = seed(&pool, "ALICE", "called off", at(Duration::hours(-2))).await;
        assert!(cancel(&pool, "ALICE", &cancelled.id).await.unwrap());
        for i in 0..5 {
            seed(&pool, "ALICE", &format!("upcoming {i}"), at(Duration::hours(i + 1))).await;
        }
        let live = list_live_for_peer(&pool, "ALICE", 25).await.unwrap();
        assert_eq!(live.len(), 5, "only the live rows");
        assert!(live.iter().all(|r| r.status == STATUS_SCHEDULED));
        assert_eq!(count_live_for_peer(&pool, "ALICE").await.unwrap(), 5);
        // Soonest first, and the cap keeps the SOONEST ones, not the oldest.
        assert_eq!(live[0].text, "upcoming 0");
        let capped = list_live_for_peer(&pool, "ALICE", 2).await.unwrap();
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[1].text, "upcoming 1");
        // The Calendar still sees everything except cancelled.
        assert_eq!(list_for_peer(&pool, "ALICE", false).await.unwrap().len(), 6);
    }

    #[tokio::test]
    async fn a_finished_reminder_is_findable_so_cancel_can_say_so() {
        let pool = fresh_pool().await;
        let r = seed(&pool, "ALICE", "done thing", at(Duration::hours(1))).await;
        assert!(finished_by_prefix(&pool, "ALICE", &r.id[..8]).await.unwrap().is_empty());
        assert!(acknowledge(&pool, "ALICE", &r.id).await.unwrap());
        let found = finished_by_prefix(&pool, "ALICE", &r.id[..8]).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].status, STATUS_DONE);
        // Still peer-scoped, and still refuses a wildcard.
        assert!(finished_by_prefix(&pool, "BOB", &r.id[..8]).await.unwrap().is_empty());
        assert!(finished_by_prefix(&pool, "ALICE", "______").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn prefix_lookup_needs_six_chars_and_is_scoped() {
        let pool = fresh_pool().await;
        let r = seed(&pool, "ALICE", "find me", at(Duration::hours(1))).await;
        assert!(find_by_prefix(&pool, "ALICE", &r.id[..4]).await.is_err());
        let hits = find_by_prefix(&pool, "ALICE", &r.id[..8]).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(find_by_prefix(&pool, "BOB", &r.id[..8]).await.unwrap().is_empty());
    }
}
