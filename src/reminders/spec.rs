//! The one place a reminder's text and due-time are checked.
//!
//! Until 0.2.124 these guards lived inline in the `set_reminder` arm of
//! `tools::registry`, which meant the chat path was the *only* validated
//! way into the table. `db::reminders::create` checks two things — text
//! non-empty and text within `MAX_TEXT_CHARS` — so anything calling it
//! directly could store a reminder due in the past, or hand chrono an
//! offset big enough to panic. `tests/reminders_live.rs` writes a row due
//! five seconds ago through exactly that gap, and it succeeds.
//!
//! So the rule is: every caller that creates a reminder goes through
//! [`plan`], and [`Planned`] is the only thing that should reach
//! `db.create_reminder`.
//!
//! One check set, two renderings. A refusal is not an error — it is a
//! sentence a person should read, or a code a program should branch on:
//!
//!   * [`Rejected::prose`] — what the model says back to the family. The
//!     tool loop treats an `Err` as a failed lookup and tells the family
//!     their web search broke, so these must stay `Ok(text)` refusals.
//!     The strings are byte-identical to the ones that shipped inline.
//!   * [`Rejected::code`] — a stable machine token. Callers that are
//!     programs should not have to parse prose.

use chrono::{DateTime, Duration, Utc};

/// Longest reminder a member can store. Owned by the store, mirrored here
/// so a caller can check the limit without reaching into `db`.
pub const MAX_TEXT_CHARS: usize = crate::db::reminders::MAX_TEXT_CHARS;
/// The furthest ahead `in_minutes` may reach, in minutes.
///
/// This is not only a product rule. `now + Duration::minutes(m)` PANICS on
/// overflow, which would kill the member's whole turn instead of answering
/// it, so the bound is checked BEFORE the arithmetic — never after.
pub const MAX_MINUTES: i64 = 366 * 24 * 60;
/// The furthest ahead any due time may land.
pub const MAX_AHEAD_DAYS: i64 = 366;

/// How the caller expressed "when". Modelled as an enum so "you gave me
/// neither" is unrepresentable here and stays a surface-specific message.
#[derive(Debug, Clone, Copy)]
pub enum When<'a> {
    /// Minutes from `now`.
    InMinutes(i64),
    /// `YYYY-MM-DDTHH:MM` in the member's own zone.
    DueLocal(&'a str),
}

/// A refusal a person should read. Never an infrastructure failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejected {
    Empty,
    TooLong { len: usize, max: usize },
    TooSoon,
    TooFar,
    Unparseable(String),
    InPast { due_local: String, now_local: String, tz: String },
}

/// A reminder that has passed every check. The only thing that should be
/// handed to `db.create_reminder`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub text: String,
    pub due_at: DateTime<Utc>,
    pub tz_name: String,
}

/// The text rule on its own, returning the trimmed text.
///
/// [`plan`] calls this first, so most callers never need it. The tool does,
/// because it has to decide "you gave me no time at all" — a question
/// [`When`] makes unrepresentable here — and 0.2.123 answered the
/// over-long complaint BEFORE the missing-time one. A member who dictates
/// a long note and forgets the time should be told about the length, not
/// sent round again for a time that is about to be refused anyway.
pub fn check_text(text: &str) -> Result<&str, Rejected> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Rejected::Empty);
    }
    let len = text.chars().count();
    if len > MAX_TEXT_CHARS {
        return Err(Rejected::TooLong { len, max: MAX_TEXT_CHARS });
    }
    Ok(text)
}

/// Validate a reminder request.
///
/// `now` is a parameter rather than `Utc::now()` so the bounds are
/// testable without a clock — they were unreachable from a unit test while
/// they lived inline behind `execute("set_reminder", …)`.
///
/// Check order matches what shipped inline and is load-bearing: text
/// first, then the offset bound (before any chrono arithmetic), then the
/// past check, then the ceiling.
pub fn plan(
    text: &str,
    when: When<'_>,
    tz: Option<chrono_tz::Tz>,
    tz_name: &str,
    now: DateTime<Utc>,
) -> Result<Planned, Rejected> {
    let text = check_text(text)?;

    let due = match when {
        When::InMinutes(m) => {
            if m < 1 {
                return Err(Rejected::TooSoon);
            }
            if m > MAX_MINUTES {
                return Err(Rejected::TooFar);
            }
            // `m` is bounded above, so `Duration::minutes` cannot panic.
            // `checked_add_signed` keeps the guard honest if someone later
            // widens MAX_MINUTES without re-reading the comment on it.
            now.checked_add_signed(Duration::minutes(m)).ok_or(Rejected::TooFar)?
        }
        When::DueLocal(local) => match crate::tools::datetime::local_to_utc(local, tz) {
            Ok(d) => d,
            Err(e) => return Err(Rejected::Unparseable(e.to_string())),
        },
    };

    if due <= now {
        return Err(Rejected::InPast {
            due_local: crate::db::reminders::local_display(due, tz_name),
            now_local: crate::db::reminders::local_display(now, tz_name),
            tz: tz_name.to_string(),
        });
    }
    let ceiling = now
        .checked_add_signed(Duration::days(MAX_AHEAD_DAYS))
        .ok_or(Rejected::TooFar)?;
    if due > ceiling {
        return Err(Rejected::TooFar);
    }

    Ok(Planned { text: text.to_string(), due_at: due, tz_name: tz_name.to_string() })
}

impl Rejected {
    /// What the model says back. Byte-identical to the strings that
    /// shipped inline in `tools::registry` — the tests at the bottom of
    /// that module assert on these substrings and must not need changing.
    pub fn prose(&self) -> String {
        match self {
            Rejected::Empty => "A reminder needs something to say — nothing was set.".into(),
            Rejected::TooLong { len, max } => format!(
                "That reminder text is too long ({len} characters; the limit is {max}). Shorten it \
                 to the essentials and try again — nothing was set."
            ),
            Rejected::TooSoon => {
                "A reminder needs to be at least one minute away — nothing was set.".into()
            }
            Rejected::TooFar => "That's more than a year away — nothing was set. Pick a date within \
                                 the next year."
                .into(),
            Rejected::Unparseable(e) => format!(
                "I couldn't read that time ({e}). Give it as YYYY-MM-DDTHH:MM in the \
                 user's timezone, or say how many minutes from now. Nothing was set."
            ),
            Rejected::InPast { due_local, now_local, tz } => format!(
                "{} ({}) is already in the past — it's {} now. Did you mean tomorrow, or \
                 another day? Nothing was set.",
                pretty_local(due_local),
                tz,
                pretty_local(now_local),
            ),
        }
    }

    /// A stable token for a caller that is a program, not a person.
    /// These are an API surface: changing one is a breaking change.
    pub fn code(&self) -> &'static str {
        match self {
            Rejected::Empty => "empty",
            Rejected::TooLong { .. } => "text_too_long",
            Rejected::TooSoon => "too_soon",
            Rejected::TooFar => "too_far",
            Rejected::Unparseable(_) => "bad_due_local",
            Rejected::InPast { .. } => "in_past",
        }
    }
}

/// "2026-09-10T09:00" → "Wed 10 Sep, 09:00" for confirmations and lists.
pub fn pretty_local(due_local: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(due_local, "%Y-%m-%dT%H:%M")
        .map(|d| d.format("%a %-d %b, %H:%M").to_string())
        .unwrap_or_else(|_| due_local.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TZ: &str = "Europe/Berlin";

    fn zone() -> Option<chrono_tz::Tz> {
        Some("Europe/Berlin".parse().unwrap())
    }

    /// A fixed clock: 2026-09-10 12:00 UTC, which is 14:00 in Berlin.
    /// Built rather than parsed — an RFC3339 literal with seconds and a Z
    /// reads as a pasted log line to `scripts/privacy-guard.sh`.
    fn now() -> DateTime<Utc> {
        utc(2026, 9, 10, 12, 0)
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).single().expect("a real instant")
    }

    #[test]
    fn a_plain_relative_reminder_is_planned() {
        let p = plan("water the plants", When::InMinutes(30), zone(), TZ, now()).unwrap();
        assert_eq!(p.text, "water the plants");
        assert_eq!(p.due_at, utc(2026, 9, 10, 12, 30));
        assert_eq!(p.tz_name, TZ);
    }

    #[test]
    fn text_is_trimmed_and_emptiness_is_refused() {
        let p = plan("  spaced  ", When::InMinutes(5), zone(), TZ, now()).unwrap();
        assert_eq!(p.text, "spaced");
        for blank in ["", "   ", "\n\t "] {
            assert_eq!(plan(blank, When::InMinutes(5), zone(), TZ, now()), Err(Rejected::Empty));
        }
    }

    #[test]
    fn check_text_is_the_same_rule_plan_applies() {
        // The tool asks this on its own, before it knows the "when", so
        // the two must never drift apart.
        let over = "z".repeat(MAX_TEXT_CHARS + 1);
        assert_eq!(check_text(&over).unwrap_err(), Rejected::TooLong {
            len: MAX_TEXT_CHARS + 1,
            max: MAX_TEXT_CHARS
        });
        assert_eq!(
            plan(&over, When::InMinutes(30), zone(), TZ, now()).unwrap_err(),
            check_text(&over).unwrap_err()
        );
        assert_eq!(check_text("  trimmed  ").unwrap(), "trimmed");
        assert_eq!(check_text("   ").unwrap_err(), Rejected::Empty);
    }

    #[test]
    fn the_text_cap_is_counted_in_characters_not_bytes() {
        // Exactly at the cap is fine; one past it is a refusal. Using a
        // multi-byte character proves the count is chars, not bytes — a
        // byte count would refuse this at a third of the real limit.
        let at_cap = "é".repeat(MAX_TEXT_CHARS);
        assert!(plan(&at_cap, When::InMinutes(5), zone(), TZ, now()).is_ok());

        let over = "é".repeat(MAX_TEXT_CHARS + 1);
        let err = plan(&over, When::InMinutes(5), zone(), TZ, now()).unwrap_err();
        assert_eq!(err, Rejected::TooLong { len: MAX_TEXT_CHARS + 1, max: MAX_TEXT_CHARS });
        assert_eq!(err.code(), "text_too_long");
        assert!(err.prose().contains("too long"), "{}", err.prose());
    }

    #[test]
    fn the_offset_bound_is_checked_before_the_arithmetic() {
        // `now + Duration::minutes(m)` panics on overflow. If the bound
        // were checked after the add, these would abort the whole turn.
        // The test is the assertion that we return at all.
        for m in [MAX_MINUTES + 1, 525_600_000_000, 160_000_000_000_000, i64::MAX] {
            let err = plan("far off", When::InMinutes(m), zone(), TZ, now()).unwrap_err();
            assert_eq!(err, Rejected::TooFar, "in_minutes={m}");
            assert!(err.prose().contains("more than a year"), "in_minutes={m}");
        }
    }

    #[test]
    fn the_offset_floor_is_one_minute() {
        for m in [0i64, -1, i64::MIN] {
            assert_eq!(
                plan("too soon", When::InMinutes(m), zone(), TZ, now()).unwrap_err(),
                Rejected::TooSoon,
                "in_minutes={m}"
            );
        }
        assert!(plan("just enough", When::InMinutes(1), zone(), TZ, now()).is_ok());
    }

    #[test]
    fn the_boundaries_of_the_offset_bound_are_inclusive() {
        assert!(plan("exactly a year", When::InMinutes(MAX_MINUTES), zone(), TZ, now()).is_ok());
        assert_eq!(
            plan("a minute past", When::InMinutes(MAX_MINUTES + 1), zone(), TZ, now()).unwrap_err(),
            Rejected::TooFar
        );
    }

    #[test]
    fn an_absolute_time_is_read_in_the_members_zone() {
        // 14:00 Berlin in September is 12:00Z (CEST, UTC+2).
        let p = plan("call back", When::DueLocal("2026-09-10T14:30"), zone(), TZ, now()).unwrap();
        assert_eq!(p.due_at, utc(2026, 9, 10, 12, 30));
    }

    #[test]
    fn a_time_in_the_past_is_refused_and_says_both_clocks() {
        let err = plan("too late", When::DueLocal("2001-01-01T09:00"), zone(), TZ, now())
            .unwrap_err();
        assert_eq!(err.code(), "in_past");
        let prose = err.prose();
        assert!(prose.contains("in the past"), "{prose}");
        assert!(prose.contains("Nothing was set"), "{prose}");
        // Both the due time and "now" are rendered, in the member's zone.
        assert!(prose.contains("Mon 1 Jan, 09:00"), "{prose}");
        assert!(prose.contains("Thu 10 Sep, 14:00"), "{prose}");
        assert!(prose.contains(TZ), "{prose}");
    }

    #[test]
    fn now_itself_is_in_the_past() {
        // `due <= now`, not `<`. A reminder for this exact instant has
        // already missed.
        let err = plan("right now", When::DueLocal("2026-09-10T14:00"), zone(), TZ, now())
            .unwrap_err();
        assert_eq!(err.code(), "in_past");
    }

    #[test]
    fn an_absolute_time_beyond_the_ceiling_is_refused() {
        let err = plan("far future", When::DueLocal("2030-01-01T09:00"), zone(), TZ, now())
            .unwrap_err();
        assert_eq!(err, Rejected::TooFar);
    }

    #[test]
    fn an_unreadable_time_is_a_refusal_carrying_the_reason() {
        for bad in ["nine-ish", "", "2026-13-45T99:99", "10/09/2026 14:00"] {
            let err = plan("garbled", When::DueLocal(bad), zone(), TZ, now()).unwrap_err();
            assert_eq!(err.code(), "bad_due_local", "input {bad:?}");
            let prose = err.prose();
            assert!(prose.contains("couldn't read that time"), "{prose}");
            assert!(prose.contains("Nothing was set"), "{prose}");
        }
    }

    #[test]
    fn a_skipped_dst_hour_is_handled_not_panicked() {
        // Europe/Berlin springs forward 2027-03-28 02:00 → 03:00, so
        // 02:30 does not exist. It must resolve or refuse, never panic.
        // (The 2027 gap, not 2026's — the 2026 one is behind `now` and
        // would be refused as in-past before the zone logic is reached.)
        let out = plan("dst gap", When::DueLocal("2027-03-28T02:30"), zone(), TZ, now());
        match out {
            Ok(p) => assert!(p.due_at > now(), "resolved forward"),
            Err(e) => assert_eq!(e.code(), "bad_due_local"),
        }
        // And the ambiguous hour when the clocks go back (2026-10-25).
        let out = plan("dst fold", When::DueLocal("2026-10-25T02:30"), zone(), TZ, now());
        match out {
            Ok(p) => assert!(p.due_at > now(), "resolved to one of the two"),
            Err(e) => assert_eq!(e.code(), "bad_due_local"),
        }
    }

    #[test]
    fn an_unknown_zone_still_plans() {
        // `tz: None` is what a member with no zone on file gets; the
        // caller passes the host's zone name for display.
        let p = plan("no zone", When::InMinutes(10), None, "UTC", now()).unwrap();
        assert_eq!(p.due_at, utc(2026, 9, 10, 12, 10));
        assert_eq!(p.tz_name, "UTC");
    }

    #[test]
    fn the_machine_codes_are_stable() {
        // These are an API contract for a non-LLM caller. Changing one is
        // a breaking change, so pin them.
        assert_eq!(Rejected::Empty.code(), "empty");
        assert_eq!(Rejected::TooLong { len: 1, max: 0 }.code(), "text_too_long");
        assert_eq!(Rejected::TooSoon.code(), "too_soon");
        assert_eq!(Rejected::TooFar.code(), "too_far");
        assert_eq!(Rejected::Unparseable("x".into()).code(), "bad_due_local");
        assert_eq!(
            Rejected::InPast { due_local: "a".into(), now_local: "b".into(), tz: "c".into() }
                .code(),
            "in_past"
        );
    }

    #[test]
    fn every_refusal_says_nothing_was_set() {
        // The family must never be left wondering whether it half-worked.
        let all = [
            Rejected::Empty,
            Rejected::TooLong { len: 999, max: 600 },
            Rejected::TooSoon,
            Rejected::TooFar,
            Rejected::Unparseable("bad".into()),
            Rejected::InPast {
                due_local: "2026-01-01T09:00".into(),
                now_local: "2026-06-01T09:00".into(),
                tz: "UTC".into(),
            },
        ];
        for r in all {
            let p = r.prose();
            assert!(
                p.contains("nothing was set") || p.contains("Nothing was set"),
                "{:?} → {p}",
                r.code()
            );
        }
    }

    #[test]
    fn the_sentences_are_pinned() {
        // These shipped inline in `tools::registry` through 0.2.123 and
        // are transcribed here character for character. They are what the
        // family reads, and the refusal tests in that module assert on
        // substrings of them — so a reword is a deliberate act, not a
        // tidy-up. Reflow the source freely; changing a word is a change.
        assert_eq!(
            Rejected::TooSoon.prose(),
            "A reminder needs to be at least one minute away — nothing was set."
        );
        assert_eq!(
            Rejected::TooFar.prose(),
            "That's more than a year away — nothing was set. Pick a date within the next year."
        );
        assert_eq!(
            Rejected::TooLong { len: 601, max: 600 }.prose(),
            "That reminder text is too long (601 characters; the limit is 600). Shorten it to the essentials and try again — nothing was set."
        );
        assert_eq!(
            Rejected::Unparseable("bad input".into()).prose(),
            "I couldn't read that time (bad input). Give it as YYYY-MM-DDTHH:MM in the user's timezone, or say how many minutes from now. Nothing was set."
        );
        assert_eq!(
            Rejected::InPast {
                due_local: "2001-01-01T09:00".into(),
                now_local: "2026-09-10T14:00".into(),
                tz: "Europe/Berlin".into(),
            }
            .prose(),
            "Mon 1 Jan, 09:00 (Europe/Berlin) is already in the past — it's Thu 10 Sep, 14:00 now. Did you mean tomorrow, or another day? Nothing was set."
        );
    }

    #[test]
    fn pretty_local_falls_back_to_the_raw_string() {
        assert_eq!(pretty_local("2026-09-10T09:00"), "Thu 10 Sep, 09:00");
        assert_eq!(pretty_local("not a time"), "not a time");
    }
}
