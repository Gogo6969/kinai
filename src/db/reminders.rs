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
    /// "", "daily", "weekdays", "weekly" or "monthly" — see
    /// `reminders::spec::Repeat`.
    pub repeat: String,
    /// The wall clock of the occurrence currently outstanding, or of the
    /// last one delivered. `""` on one-offs and on every row written
    /// before recurrence shipped.
    ///
    /// The series steps from HERE, never from `due_local` (which a snooze
    /// can move anywhere) and never from `fired_at` (which is when
    /// delivery happened: seconds late on a good tick, days late after an
    /// outage). On the morning the clocks skip 02:30 the reminder fires at
    /// 03:00 — `due_local` says 03:00, this still says 02:30, and that is
    /// the only reason the series is back at 02:30 the next day.
    pub occurrence_local: String,
    pub status: String,
    pub fired_at: Option<String>,
    pub source_msg_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

const COLS: &str = "id, peer_id, thread_id, text, due_at, tz, due_local, repeat, \
                    occurrence_local, status, fired_at, source_msg_id, created_at, updated_at";

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
    repeat: &str,
    occurrence_local: &str,
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
                                occurrence_local, status, fired_at, source_msg_id,
                                created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?10, ?11, 'scheduled', NULL, ?8, ?9, ?9)",
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
    .bind(repeat)
    .bind(occurrence_local)
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
        repeat: repeat.into(),
        occurrence_local: occurrence_local.into(),
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
         WHERE datetime(due_at) <= datetime(?2)
           AND (status = 'scheduled' OR (status = 'fired' AND repeat != ''))
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

/// Delivery happened for a REPEATING reminder: record the occurrence that
/// went out and arm the row for the next one, in a single guarded UPDATE.
///
/// The advance happens on DELIVERY, not on acknowledgement. Waiting for an
/// ack would be the silent stop this whole feature must not have: a
/// Telegram-only member has no ack button at all (the message is plain
/// text telling them to open the Calendar), and `lease_due` would never
/// see the row again anyway once it rested in `fired`.
///
/// `occurrence_local` is the wall clock `next_local` produced, passed in
/// verbatim. It is NOT `local_display(due_at, tz)` and NOT derived from
/// `fired_at` — see the field's own documentation for why that is wrong
/// exactly once a year and then permanently.
pub async fn mark_fired_advanced(
    pool: &SqlitePool,
    peer_id: &str,
    id: &str,
    occurrence_local: &str,
    next_due_at: DateTime<Utc>,
    next_due_local: &str,
) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let r = sqlx::query(
        "UPDATE reminders
         SET status = 'fired', fired_at = ?1, updated_at = ?1,
             occurrence_local = ?4, due_at = ?5, due_local = ?6
         WHERE id = ?2 AND peer_id = ?3 AND status = 'firing'",
    )
    .bind(&now)
    .bind(id)
    .bind(peer_id)
    .bind(occurrence_local)
    .bind(next_due_at.to_rfc3339())
    .bind(next_due_local)
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
    let mut new_due = now + Duration::minutes(minutes);
    let repeat = crate::reminders::spec::Repeat::parse(&current.repeat).unwrap_or_default();

    // Snoozing must never move the series. It writes due_at and nothing
    // else that matters: `occurrence_local` is untouched here and in every
    // other member-facing verb, so the next occurrence is always
    // next_local(occurrence_local) no matter how many times the member
    // hits the button. That is a structural guarantee rather than an
    // arithmetic one — there is no accumulating offset to get wrong.
    if repeat.repeats() && !current.occurrence_local.is_empty() {
        // A snooze past the next occurrence would skip a day. Fold it back.
        if let Some(anchor) = parse_wall(&current.occurrence_local) {
            let tz = current.tz.parse::<chrono_tz::Tz>().ok();
            if let Some((_, next_at, _)) =
                crate::reminders::spec::advance(anchor, repeat, tz, now)
            {
                if new_due >= next_at {
                    new_due = next_at;
                }
            }
        }
    }

    let due_local = local_display(new_due, &current.tz);
    // A repeating row stays `fired` while its occurrence is outstanding —
    // the member asked to be poked again about THIS one, not to have it
    // marked handled.
    let (next_status, keep_fired) =
        if repeat.repeats() { ("fired", true) } else { ("scheduled", false) };
    let row = sqlx::query(&format!(
        "UPDATE reminders
         SET due_at = ?1, due_local = ?2, status = ?6,
             fired_at = CASE WHEN ?7 THEN fired_at ELSE NULL END,
             updated_at = ?3
         WHERE id = ?4 AND peer_id = ?5 AND status IN ('scheduled', 'firing', 'fired')
         RETURNING {COLS}"
    ))
    .bind(new_due.to_rfc3339())
    .bind(&due_local)
    .bind(now.to_rfc3339())
    .bind(id)
    .bind(peer_id)
    .bind(next_status)
    .bind(keep_fired)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_reminder))
}

/// Is this delivery a fresh occurrence, or a snooze coming back round?
///
/// Compared as INSTANTS, not as wall-clock strings. The obvious version —
/// `due_local == next_local(anchor)` — is wrong on precisely the morning
/// this whole feature is careful about: a 02:30 daily meets the skipped
/// hour, `local_to_utc_forgiving` lands it at 03:00, and the strings then
/// disagree even though the row is exactly on its grid. It would be
/// classified a re-poke, the anchor would stop advancing, and the series
/// would re-deliver the same occurrence every day from then on.
///
/// Deliberately NOT "is `fired_at` set": a crash between lease and mark
/// leaves a stale `fired_at`, and that rule would re-poke every thirty
/// seconds forever.
pub(crate) fn is_new_occurrence(
    r: &Reminder,
    repeat: crate::reminders::spec::Repeat,
    anchor: Option<chrono::NaiveDateTime>,
    tz: Option<chrono_tz::Tz>,
) -> bool {
    if !repeat.repeats() {
        return true;
    }
    let Some(a) = anchor else { return true };
    let Some(next) = crate::reminders::spec::next_local(a, repeat) else {
        return true;
    };
    let expected = crate::reminders::spec::local_to_utc_forgiving(next, tz);
    match chrono::DateTime::parse_from_rfc3339(&r.due_at) {
        Ok(due) => due.with_timezone(&Utc) == expected,
        Err(_) => true,
    }
}

/// "YYYY-MM-DDTHH:MM" -> naive wall clock.
fn parse_wall(s: &str) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%dT%H:%M").ok()
}

/// The member says "got it". Allowed from any live state — acknowledging
/// something early is a fine way to be done with it.
pub async fn acknowledge(pool: &SqlitePool, peer_id: &str, id: &str) -> Result<bool> {
    let Some(current) = get(pool, peer_id, id).await? else {
        return Ok(false);
    };
    let repeat = crate::reminders::spec::Repeat::parse(&current.repeat).unwrap_or_default();

    // Done on a repeating reminder means "this one is handled", never
    // "stop reminding me" — that is Stop, on its own control. So the row
    // goes back to `scheduled` on its own grid, re-derived from the anchor
    // rather than left wherever a snooze put due_at.
    //
    // Idempotent by construction: a second Done, from a second device,
    // recomputes the same values and changes nothing. That is why this
    // needs no occurrence token on the wire.
    if repeat.repeats() {
        if let Some(anchor) = parse_wall(&current.occurrence_local) {
            let tz = current.tz.parse::<chrono_tz::Tz>().ok();
            if let Some((_, next_at, _)) =
                crate::reminders::spec::advance(anchor, repeat, tz, Utc::now())
            {
                // `occurrence_local` is deliberately NOT written here. The
                // scheduler already stepped it when it delivered this
                // occurrence, and stepping it again would mean a second
                // Done — from a second device, or a double tap — quietly
                // ate a day. Leaving it alone is what makes this
                // idempotent, and is why no occurrence token is needed on
                // the wire.
                let r = sqlx::query(
                    "UPDATE reminders
                     SET status = 'scheduled', fired_at = NULL, updated_at = ?1,
                         due_at = ?4, due_local = ?5
                     WHERE id = ?2 AND peer_id = ?3
                       AND status IN ('scheduled', 'firing', 'fired')",
                )
                .bind(Utc::now().to_rfc3339())
                .bind(id)
                .bind(peer_id)
                .bind(next_at.to_rfc3339())
                .bind(local_display(next_at, &current.tz))
                .execute(pool)
                .await?;
                return Ok(r.rows_affected() > 0);
            }
        }
        // The calendar ran out, or the row predates the anchor column.
        // Fall through and finish it rather than leave it stuck.
    }

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
        // Ending a series, as opposed to finishing one occurrence. Kept
        // as a separate verb because Done on a repeating reminder
        // deliberately does NOT stop it, and a member who wants it gone
        // needs a word for that which is not "delete" (a hard row
        // removal) and not "ack".
        "stop" => {
            if !cancel(pool, peer_id, id).await? {
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
        occurrence_local: r.get("occurrence_local"),
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

    const BERLIN: &str = "Europe/Berlin";

    fn wall(s: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").unwrap()
    }

    /// Seed a repeating row directly, the way `create` would.
    async fn seed_daily(pool: &SqlitePool, peer: &str, first_local: &str) -> Reminder {
        let tz: chrono_tz::Tz = BERLIN.parse().unwrap();
        let due = crate::reminders::spec::local_to_utc_forgiving(wall(first_local), Some(tz));
        create(pool, peer, None, "take the pills", due, BERLIN, None, "daily", first_local)
            .await
            .expect("seed")
    }

    /// Walk one delivery the way the scheduler does: lease, work out the
    /// occurrence, advance.
    async fn fire_once(pool: &SqlitePool, peer: &str, now: DateTime<Utc>) -> Reminder {
        let leased = lease_due(pool, now).await.expect("lease");
        assert_eq!(leased.len(), 1, "exactly one row leased at {now}");
        let r = &leased[0];
        let repeat = crate::reminders::spec::Repeat::parse(&r.repeat).unwrap();
        let tz = r.tz.parse::<chrono_tz::Tz>().ok();
        let anchor = parse_wall(&r.occurrence_local);
        let due_wall = parse_wall(&r.due_local).unwrap();
        let is_new = is_new_occurrence(r, repeat, anchor, tz);
        let occurrence = match (anchor, is_new) {
            (None, _) => due_wall,
            (Some(a), true) => crate::reminders::spec::next_local(a, repeat).unwrap_or(a),
            (Some(a), false) => a,
        };
        let (_, next_at, _) =
            crate::reminders::spec::advance(occurrence, repeat, tz, now).expect("advance");
        mark_fired_advanced(
            pool,
            peer,
            &r.id,
            &occurrence.format("%Y-%m-%dT%H:%M").to_string(),
            next_at,
            &local_display(next_at, &r.tz),
        )
        .await
        .expect("mark");
        get(pool, peer, &r.id).await.unwrap().unwrap()
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).single().unwrap()
    }

    /// A daily reminder nobody ever acknowledges must keep coming. This is
    /// the failure the whole design is built to avoid: advance-on-ack
    /// would leave it parked in `fired`, where `lease_due` never looks
    /// again, and it would silently stop forever.
    #[tokio::test]
    async fn a_daily_reminder_keeps_firing_without_a_single_ack() {
        let pool = fresh_pool().await;
        seed_daily(&pool, "ALICE", "2026-11-10T09:00").await;

        // 09:00 Berlin in November is 08:00 UTC.
        let day1 = fire_once(&pool, "ALICE", utc(2026, 11, 10, 8, 0)).await;
        assert_eq!(day1.occurrence_local, "2026-11-10T09:00");
        assert_eq!(day1.due_local, "2026-11-11T09:00", "armed for tomorrow already");
        assert_eq!(day1.status, "fired", "and still outstanding for the member");

        let day2 = fire_once(&pool, "ALICE", utc(2026, 11, 11, 8, 0)).await;
        assert_eq!(day2.occurrence_local, "2026-11-11T09:00", "the grid stepped");
        assert_eq!(day2.due_local, "2026-11-12T09:00");

        let day3 = fire_once(&pool, "ALICE", utc(2026, 11, 12, 8, 0)).await;
        assert_eq!(day3.occurrence_local, "2026-11-12T09:00");
        assert_eq!(day3.due_local, "2026-11-13T09:00");
    }

    /// Snoozing moves this poke and nothing else. The guarantee is
    /// structural: no member-facing verb writes `occurrence_local`.
    #[tokio::test]
    async fn snoozing_a_daily_reminder_never_drifts_the_series() {
        let pool = fresh_pool().await;
        let seeded = seed_daily(&pool, "ALICE", "2026-11-10T09:00").await;
        let fired = fire_once(&pool, "ALICE", utc(2026, 11, 10, 8, 0)).await;
        assert_eq!(fired.occurrence_local, "2026-11-10T09:00");

        for _ in 0..5 {
            let s = snooze(&pool, "ALICE", &seeded.id, 10).await.unwrap().unwrap();
            assert_eq!(
                s.occurrence_local, "2026-11-10T09:00",
                "snooze must never touch the anchor"
            );
            assert_eq!(s.status, "fired", "still outstanding, not quietly handled");
            assert!(s.fired_at.is_some(), "and still flagged as delivered");
        }

        // Tomorrow is still 09:00, not 09:50.
        let after = get(&pool, "ALICE", &seeded.id).await.unwrap().unwrap();
        let next = crate::reminders::spec::next_local(
            parse_wall(&after.occurrence_local).unwrap(),
            crate::reminders::spec::Repeat::Daily,
        )
        .unwrap();
        assert_eq!(next.format("%Y-%m-%dT%H:%M").to_string(), "2026-11-11T09:00");
    }

    /// Done finishes the occurrence and re-arms on the grid — it does not
    /// end the series, and it repairs a snoozed due_at.
    #[tokio::test]
    async fn done_handles_one_occurrence_and_stop_ends_the_series() {
        let pool = fresh_pool().await;
        let seeded = seed_daily(&pool, "ALICE", "2026-11-10T09:00").await;
        fire_once(&pool, "ALICE", utc(2026, 11, 10, 8, 0)).await;
        snooze(&pool, "ALICE", &seeded.id, 30).await.unwrap();

        assert!(acknowledge(&pool, "ALICE", &seeded.id).await.unwrap());
        let after = get(&pool, "ALICE", &seeded.id).await.unwrap().unwrap();
        assert_eq!(after.status, "scheduled", "a repeating row is never 'done'");
        assert!(after.fired_at.is_none(), "this occurrence is handled");
        assert_eq!(after.due_local, "2026-11-11T09:00", "back on the grid, not 09:30");

        // A second Done from another device changes nothing.
        acknowledge(&pool, "ALICE", &seeded.id).await.unwrap();
        let again = get(&pool, "ALICE", &seeded.id).await.unwrap().unwrap();
        assert_eq!(again.due_local, "2026-11-11T09:00", "idempotent");

        // Stop is the way out, and it is a different verb from Done.
        apply_action(&pool, "ALICE", &seeded.id, "stop", 0).await.unwrap();
        let stopped = get(&pool, "ALICE", &seeded.id).await.unwrap().unwrap();
        assert_eq!(stopped.status, "cancelled");
        assert!(
            lease_due(&pool, utc(2027, 1, 1, 0, 0)).await.unwrap().is_empty(),
            "a stopped series is never leased again"
        );
    }

    /// THE trap. A 02:30 daily meets the morning the clocks skip 02:00 to
    /// 03:00. It fires at 03:00 that day — and the anchor must still say
    /// 02:30, or the series moves to 03:00 permanently and nobody ever
    /// connects the shift to a clock change six months earlier.
    #[tokio::test]
    async fn the_skipped_hour_does_not_move_the_series_forever() {
        let pool = fresh_pool().await;
        seed_daily(&pool, "ALICE", "2027-03-27T02:30").await;

        // The day before the change: an ordinary 02:30.
        let d1 = fire_once(&pool, "ALICE", utc(2027, 3, 27, 1, 30)).await;
        assert_eq!(d1.occurrence_local, "2027-03-27T02:30");
        assert_eq!(d1.due_local, "2027-03-28T03:00", "02:30 does not exist that morning");

        // The gap morning: delivered at 03:00, anchored at 02:30.
        let d2 = fire_once(&pool, "ALICE", utc(2027, 3, 28, 1, 0)).await;
        assert_eq!(
            d2.occurrence_local, "2027-03-28T02:30",
            "the anchor is the wall clock next_local produced, NOT the instant that fired"
        );
        assert_eq!(d2.due_local, "2027-03-29T02:30", "and the next one is back to 02:30");

        // And it stays there.
        let d3 = fire_once(&pool, "ALICE", utc(2027, 3, 29, 0, 30)).await;
        assert_eq!(d3.occurrence_local, "2027-03-29T02:30");
        assert_eq!(d3.due_local, "2027-03-30T02:30");
    }

    /// The host was off for three days. One notification, not three —
    /// there is one row, and the advance walks past everything missed.
    #[tokio::test]
    async fn an_outage_collapses_to_a_single_delivery() {
        let pool = fresh_pool().await;
        seed_daily(&pool, "ALICE", "2026-11-10T09:00").await;

        // Nothing ran until the 13th.
        let back = fire_once(&pool, "ALICE", utc(2026, 11, 13, 10, 0)).await;
        assert_eq!(back.occurrence_local, "2026-11-10T09:00", "the one it was due for");
        assert_eq!(back.due_local, "2026-11-14T09:00", "and it lands in the future");

        // Critically: nothing is left due, so the next tick is quiet
        // rather than firing again thirty seconds later.
        assert!(
            lease_due(&pool, utc(2026, 11, 13, 10, 1)).await.unwrap().is_empty(),
            "a repeating row must never come to rest in the past"
        );
    }

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
        create(pool, peer, Some("thread-1"), text, due, "Europe/Berlin", None, "", "")
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
        assert!(create(&pool, "ALICE", None, "   ", due, "Europe/Berlin", None, "", "").await.is_err());
        let long = "x".repeat(MAX_TEXT_CHARS + 1);
        assert!(create(&pool, "ALICE", None, &long, due, "Europe/Berlin", None, "", "").await.is_err());
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
