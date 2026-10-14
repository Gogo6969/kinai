//! Which model servers the host should NOT try right now, and why.
//!
//! Two runtime facts, neither written to disk:
//!
//! * **Planned maintenance.** Someone (a script, via the maintenance API in
//!   `network::maintenance_api`) tells the host "this server is being worked
//!   on until T". Until then every slot that points at it is treated as
//!   down WITHOUT a probe and without a failed request: the family never
//!   waits on a server we already know is gone. The window ends by itself
//!   at T, so a forgotten "end" call cannot leave the house on the backup
//!   model forever.
//! * **Recent failures.** A real request or probe just failed against a
//!   server. Believe it for a few seconds, for every slot on that server.
//!
//! Everything is keyed by the server's ADDRESS (`host:port`), never by slot:
//! `fast` and `balanced` are two model names on one machine, and when that
//! machine is down both are. Keying by slot made the failover try the same
//! dead server twice and announce two hops.
//!
//! Not persisted on purpose. If KinAI restarts mid-maintenance the next
//! probe finds the server down anyway, and a window that survived a restart
//! is exactly the kind of state that outlives its reason.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Serialize;

/// Longest maintenance window one call may open. A test that needs longer
/// can renew; a forgotten window cannot hide a live server for a day.
pub const MAX_MINUTES: i64 = 240;
/// Window used when the caller gives no duration.
pub const DEFAULT_MINUTES: i64 = 30;
/// How long a failed request/probe is believed before the server is probed
/// again. Short, so recovery is noticed within seconds.
pub const FAILURE_MEMORY: Duration = Duration::from_secs(5);

/// `host:port` of a base URL, lowercase, with the scheme's default port
/// filled in. Empty when the URL has no host. Scheme, path, credentials and
/// trailing slashes never matter: `http://M5:8095/v1` and `m5:8095` are the
/// same server.
pub fn authority(base_url: &str) -> String {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };
    match reqwest::Url::parse(&with_scheme) {
        Ok(u) => match (u.host_str(), u.port_or_known_default()) {
            (Some(h), Some(p)) => format!("{}:{p}", h.to_ascii_lowercase()),
            (Some(h), None) => h.to_ascii_lowercase(),
            _ => String::new(),
        },
        Err(_) => String::new(),
    }
}

/// One open maintenance window.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Maintenance {
    pub authority: String,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    /// Free text from the caller, for the status call and the log. It is
    /// never shown to family members and never reaches a prompt.
    pub reason: String,
}

#[derive(Default)]
pub struct Availability {
    maintenance: HashMap<String, Maintenance>,
    failed_at: HashMap<String, Instant>,
}

impl Availability {
    /// Open (or renew) a window for `authority`. `minutes` is clamped to
    /// 1..=MAX_MINUTES. Returns the window as stored.
    pub fn begin(
        &mut self,
        authority: &str,
        now: DateTime<Utc>,
        minutes: i64,
        reason: &str,
    ) -> Maintenance {
        let minutes = minutes.clamp(1, MAX_MINUTES);
        let reason: String = reason
            .chars()
            .filter(|c| !c.is_control())
            .take(120)
            .collect::<String>()
            .trim()
            .to_string();
        let window = Maintenance {
            authority: authority.to_string(),
            since: now,
            until: now + ChronoDuration::minutes(minutes),
            reason,
        };
        self.maintenance.insert(authority.to_string(), window.clone());
        window
    }

    /// Close the window for one server (`Some`) or every server (`None`).
    /// Returns how many windows were open and are now closed — expired
    /// windows do not count, they were already over.
    pub fn end(&mut self, authority: Option<&str>, now: DateTime<Utc>) -> usize {
        let open = |m: &Maintenance| m.until > now;
        match authority {
            Some(a) => match self.maintenance.remove(a) {
                Some(m) if open(&m) => 1,
                _ => 0,
            },
            None => {
                let n = self.maintenance.values().filter(|m| open(m)).count();
                self.maintenance.clear();
                n
            }
        }
    }

    /// The open window for `authority`, if any. An expired window is
    /// simply absent: nothing has to run at T for it to end.
    pub fn maintenance(&self, authority: &str, now: DateTime<Utc>) -> Option<Maintenance> {
        self.maintenance
            .get(authority)
            .filter(|m| m.until > now)
            .cloned()
    }

    /// Every open window, soonest-ending first.
    pub fn active(&self, now: DateTime<Utc>) -> Vec<Maintenance> {
        let mut v: Vec<Maintenance> = self
            .maintenance
            .values()
            .filter(|m| m.until > now)
            .cloned()
            .collect();
        v.sort_by(|a, b| a.until.cmp(&b.until).then_with(|| a.authority.cmp(&b.authority)));
        v
    }

    /// A request or probe against `authority` just failed.
    pub fn note_failed(&mut self, authority: &str, at: Instant) {
        self.failed_at.insert(authority.to_string(), at);
    }

    /// A probe against `authority` just succeeded: forget old failures.
    pub fn note_ok(&mut self, authority: &str) {
        self.failed_at.remove(authority);
    }

    /// Did something fail against `authority` within `window` of `now`?
    pub fn failed_within(&self, authority: &str, window: Duration, now: Instant) -> bool {
        self.failed_at
            .get(authority)
            .map(|at| now.saturating_duration_since(*at) < window)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2031-03-05T20:00:00Z").unwrap().with_timezone(&Utc)
    }

    #[test]
    fn one_server_has_one_authority_however_it_is_written() {
        let want = "192.0.2.10:8095";
        for url in [
            "http://192.0.2.10:8095",
            "http://192.0.2.10:8095/",
            "http://192.0.2.10:8095/v1/chat/completions",
            "HTTP://192.0.2.10:8095",
            "192.0.2.10:8095",
            "  http://user:pw@192.0.2.10:8095/x?y=1#z  ",
        ] {
            assert_eq!(authority(url), want, "{url}");
        }
        // Default ports are filled in so two spellings of one server agree.
        assert_eq!(authority("https://api.example.com"), "api.example.com:443");
        assert_eq!(authority("http://Example.COM/"), "example.com:80");
        // Different port, different server.
        assert_ne!(authority("http://m5:8095"), authority("http://m5:8091"));
        assert_eq!(authority(""), "");
        assert_eq!(authority("   "), "");
    }

    #[test]
    fn a_window_is_open_until_its_end_and_closes_by_itself() {
        let mut a = Availability::default();
        let w = a.begin("m5:8095", t0(), 30, "engine test");
        assert_eq!(w.until, t0() + ChronoDuration::minutes(30));
        assert!(a.maintenance("m5:8095", t0()).is_some());
        assert!(a.maintenance("m5:8095", t0() + ChronoDuration::minutes(29)).is_some());
        // Exactly at the end it is over — nothing had to run to close it.
        assert!(a.maintenance("m5:8095", t0() + ChronoDuration::minutes(30)).is_none());
        assert!(a.active(t0() + ChronoDuration::minutes(31)).is_empty());
        // Another server is unaffected.
        assert!(a.maintenance("other:8087", t0()).is_none());
    }

    #[test]
    fn the_length_is_clamped_so_a_window_cannot_be_forgotten_forever() {
        let mut a = Availability::default();
        assert_eq!(a.begin("a:1", t0(), 0, "").until, t0() + ChronoDuration::minutes(1));
        assert_eq!(a.begin("a:1", t0(), -5, "").until, t0() + ChronoDuration::minutes(1));
        assert_eq!(
            a.begin("a:1", t0(), 100_000, "").until,
            t0() + ChronoDuration::minutes(MAX_MINUTES)
        );
    }

    #[test]
    fn renewing_replaces_the_window() {
        let mut a = Availability::default();
        a.begin("a:1", t0(), 10, "first");
        let later = t0() + ChronoDuration::minutes(5);
        let w = a.begin("a:1", later, 10, "second");
        assert_eq!(a.active(later).len(), 1);
        assert_eq!(a.maintenance("a:1", later).unwrap().until, w.until);
        assert_eq!(w.reason, "second");
    }

    #[test]
    fn ending_closes_one_or_all_and_counts_only_open_windows() {
        let mut a = Availability::default();
        a.begin("a:1", t0(), 10, "");
        a.begin("b:2", t0(), 10, "");
        assert_eq!(a.end(Some("a:1"), t0()), 1);
        assert_eq!(a.end(Some("a:1"), t0()), 0, "already closed");
        assert!(a.maintenance("a:1", t0()).is_none());
        assert!(a.maintenance("b:2", t0()).is_some());
        assert_eq!(a.end(None, t0()), 1);
        assert!(a.active(t0()).is_empty());
        // A window that already expired is not "ended" by the call.
        a.begin("c:3", t0(), 1, "");
        assert_eq!(a.end(None, t0() + ChronoDuration::minutes(5)), 0);
    }

    #[test]
    fn the_reason_is_one_short_clean_line() {
        let mut a = Availability::default();
        let w = a.begin("a:1", t0(), 5, "  engine\ntest\u{7}  ");
        assert_eq!(w.reason, "enginetest");
        let long = "x".repeat(500);
        assert_eq!(a.begin("a:1", t0(), 5, &long).reason.chars().count(), 120);
    }

    #[test]
    fn a_failure_is_believed_briefly_for_the_whole_server() {
        let mut a = Availability::default();
        let now = Instant::now();
        assert!(!a.failed_within("m5:8095", FAILURE_MEMORY, now));
        a.note_failed("m5:8095", now);
        assert!(a.failed_within("m5:8095", FAILURE_MEMORY, now));
        assert!(a.failed_within("m5:8095", FAILURE_MEMORY, now + Duration::from_secs(4)));
        assert!(!a.failed_within("m5:8095", FAILURE_MEMORY, now + Duration::from_secs(6)));
        assert!(!a.failed_within("other:1", FAILURE_MEMORY, now));
        a.note_failed("m5:8095", now);
        a.note_ok("m5:8095");
        assert!(!a.failed_within("m5:8095", FAILURE_MEMORY, now));
    }

    #[test]
    fn active_lists_soonest_ending_first() {
        let mut a = Availability::default();
        a.begin("late:1", t0(), 60, "");
        a.begin("soon:1", t0(), 5, "");
        let v = a.active(t0());
        assert_eq!(v[0].authority, "soon:1");
        assert_eq!(v[1].authority, "late:1");
    }
}
