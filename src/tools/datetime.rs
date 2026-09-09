//! Current date / time on the host machine.

use chrono::Local;

/// Date-only form for the SYSTEM prompt. Deliberately no clock: llama.cpp
/// caches the prompt by comparing tokens from position 0, and a
/// minute-resolution timestamp in the first ~40 tokens invalidated the
/// entire cache every minute — measured on the Olares fast slot as a full
/// ~2.3k-token reprocess per turn (830 ms) versus 41 tokens (165 ms) with
/// a stable prefix. The precise clock now rides with the newest user
/// message, which is uncached anyway.
pub fn today_pretty() -> String {
    let now = Local::now();
    format!(
        "{} (timezone {})",
        now.format("%A, %B %-d, %Y"),
        now.format("%Z")
    )
}

pub fn now_pretty() -> String {
    let now = Local::now();
    format!(
        "{} (timezone {})",
        now.format("%A, %B %-d, %Y %-I:%M %p"),
        now.format("%Z")
    )
}

/// The current year on the host — used to anchor tool descriptions in real
/// time (models anchor "recent" to their training years otherwise).
pub fn current_year() -> i32 {
    use chrono::Datelike;
    chrono::Local::now().year()
}

// ---- Per-member time zones (reminders, and the clock the model sees) ----

/// The zone a family member lives in. A device reports its IANA zone in the
/// Hello frame (kept in `peers.tz`); failing that, a saved `timezone` fact;
/// failing that, `None` = the host's own zone. The host's own identity never
/// connects over the socket, so it always falls through to the facts.
pub async fn resolve_peer_tz(
    db: &crate::db::Db,
    peer_id: &str,
    facts: &[crate::db::UserFact],
) -> Option<chrono_tz::Tz> {
    if peer_id != crate::db::HOST_PEER {
        if let Ok(Some(z)) = db.peer_tz(peer_id).await {
            if let Ok(tz) = z.parse::<chrono_tz::Tz>() {
                return Some(tz);
            }
        }
    }
    facts
        .iter()
        .find(|f| f.key.trim().eq_ignore_ascii_case("timezone"))
        .and_then(|f| f.value.trim().parse::<chrono_tz::Tz>().ok())
}

/// The host machine's own IANA zone name ("Europe/Berlin"), or "" when the
/// OS lookup fails. Stored on reminders the host user sets.
pub fn host_tz_name() -> String {
    iana_time_zone::get_timezone().unwrap_or_default()
}

/// `now_pretty`, but in a member's zone when one is known. Same shape as
/// the host version so prompts read identically for every member.
pub fn now_pretty_in(tz: Option<chrono_tz::Tz>) -> String {
    match tz {
        Some(zone) => {
            let now = chrono::Utc::now().with_timezone(&zone);
            format!(
                "{} (timezone {})",
                now.format("%A, %B %-d, %Y %-I:%M %p"),
                zone.name()
            )
        }
        None => now_pretty(),
    }
}

/// A wall-clock the member typed ("2026-09-10T09:00") in their zone, as a
/// UTC instant. Handles the two DST edge cases explicitly: an ambiguous
/// hour takes the earlier instant, a non-existent one is an error the tool
/// relays. `None` zone = the host's own.
pub fn local_to_utc(
    local: &str,
    tz: Option<chrono_tz::Tz>,
) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
    use chrono::{LocalResult, NaiveDateTime, TimeZone, Utc};
    let raw = local.trim();
    let naive = NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M"))
        .map_err(|_| anyhow::anyhow!("due_local must look like YYYY-MM-DDTHH:MM, got {raw:?}"))?;
    let resolve = |r: LocalResult<chrono::DateTime<chrono::Utc>>, zone: &str| match r {
        LocalResult::Single(dt) => Ok(dt),
        LocalResult::Ambiguous(earlier, _) => Ok(earlier),
        LocalResult::None => Err(anyhow::anyhow!(
            "{raw} does not exist in {zone} (the clock skips it for daylight saving)"
        )),
    };
    match tz {
        Some(zone) => resolve(
            zone.from_local_datetime(&naive).map(|dt| dt.with_timezone(&Utc)),
            zone.name(),
        ),
        None => resolve(
            chrono::Local
                .from_local_datetime(&naive)
                .map(|dt| dt.with_timezone(&Utc)),
            "the host's timezone",
        ),
    }
}

#[cfg(test)]
mod tz_tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    #[test]
    fn berlin_summer_wall_clock_becomes_utc_minus_two() {
        let tz: chrono_tz::Tz = "Europe/Berlin".parse().unwrap();
        let utc = local_to_utc("2030-07-01T09:00", Some(tz)).unwrap();
        assert_eq!((utc.month(), utc.day(), utc.hour(), utc.minute()), (7, 1, 7, 0));
    }

    #[test]
    fn seconds_and_a_space_are_tolerated_but_garbage_is_not() {
        let tz: chrono_tz::Tz = "UTC".parse().unwrap();
        assert!(local_to_utc("2030-01-01T09:00:30", Some(tz)).is_ok());
        assert!(local_to_utc("2030-01-01 09:00", Some(tz)).is_ok());
        assert!(local_to_utc("tomorrow at nine", Some(tz)).is_err());
    }

    #[test]
    fn a_skipped_dst_hour_is_an_error_and_an_ambiguous_one_takes_the_earlier() {
        let tz: chrono_tz::Tz = "Europe/Berlin".parse().unwrap();
        // 2030-03-31 02:30 does not exist in Berlin (clocks jump 02:00→03:00).
        assert!(local_to_utc("2030-03-31T02:30", Some(tz)).is_err());
        // 2030-10-27 02:30 happens twice; the earlier is CEST = 00:30 UTC.
        let utc = local_to_utc("2030-10-27T02:30", Some(tz)).unwrap();
        assert_eq!((utc.hour(), utc.minute()), (0, 30));
    }

    #[test]
    fn now_in_a_zone_names_the_zone() {
        let tz: chrono_tz::Tz = "Asia/Tokyo".parse().unwrap();
        assert!(now_pretty_in(Some(tz)).ends_with("(timezone Asia/Tokyo)"));
    }
}
