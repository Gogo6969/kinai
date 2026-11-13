//! Is somebody using this device right now?
//!
//! One question, asked by the self-installing update (`auto_update.rs`):
//! may KinAI restart itself this minute without taking the app away from
//! a person? Two things say no —
//!
//!   * somebody touched it lately: a key, a click, a question sent, a
//!     piece of an answer arriving;
//!   * a question is out and its answer has not finished. A slow model
//!     can think for minutes without sending a word, so "nothing arrived
//!     lately" is not enough to call the device idle.
//!
//! Everything here is a pair of timestamps. Nothing about WHAT was typed
//! or asked is kept.

use std::sync::atomic::{AtomicI64, Ordering};

/// How long the device has to sit untouched before it counts as free.
pub const QUIET_SECS: i64 = 10 * 60;

/// How long an unanswered question keeps the device "in use". An answer
/// that never comes (the host went away mid-question) must not hold an
/// update back for ever, so the claim lapses.
pub const ANSWER_GRACE_SECS: i64 = 30 * 60;

/// Unix time of the last sign of use. `0` = not started yet.
static LAST_USE: AtomicI64 = AtomicI64::new(0);
/// Unix time the oldest still-unanswered question was sent. `0` = none.
static AWAITING_SINCE: AtomicI64 = AtomicI64::new(0);

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Somebody (or an arriving answer) just used the app.
pub fn touch() {
    LAST_USE.store(now_unix(), Ordering::Relaxed);
}

/// A question went out to the host.
pub fn question_sent() {
    let now = now_unix();
    LAST_USE.store(now, Ordering::Relaxed);
    // Keep the OLDEST open question: a second one sent while the first
    // is still running must not make the first look younger.
    let _ = AWAITING_SINCE.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
}

/// An answer finished (or the host said it failed).
pub fn answer_finished() {
    LAST_USE.store(now_unix(), Ordering::Relaxed);
    AWAITING_SINCE.store(0, Ordering::Relaxed);
}

/// Why the device is not free right now — `None` when it is.
pub fn busy_reason() -> Option<&'static str> {
    busy_reason_at(
        now_unix(),
        LAST_USE.load(Ordering::Relaxed),
        AWAITING_SINCE.load(Ordering::Relaxed),
    )
}

/// How long the device has been untouched, in whole minutes (for the log).
pub fn quiet_minutes() -> i64 {
    let last = LAST_USE.load(Ordering::Relaxed);
    if last == 0 {
        return 0;
    }
    (now_unix() - last).max(0) / 60
}

/// The decision itself, with the clock passed in so it can be tested.
pub(crate) fn busy_reason_at(now: i64, last_use: i64, awaiting_since: i64) -> Option<&'static str> {
    if last_use == 0 {
        // Never touched since launch means the launch has not been
        // recorded yet — treat that as "just started", not as "idle".
        return Some("the app has only just started");
    }
    if awaiting_since != 0 && now - awaiting_since < ANSWER_GRACE_SECS {
        return Some("an answer is still on its way");
    }
    if now - last_use < QUIET_SECS {
        return Some("the device was used a moment ago");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 2_000_000_000;

    #[test]
    fn a_device_nobody_touched_for_ten_minutes_is_free() {
        assert_eq!(busy_reason_at(T, T - QUIET_SECS, 0), None);
        assert_eq!(busy_reason_at(T, T - 3 * 60 * 60, 0), None);
    }

    #[test]
    fn a_recent_key_or_click_keeps_it_in_use() {
        assert!(busy_reason_at(T, T - 5, 0).is_some());
        assert!(busy_reason_at(T, T - (QUIET_SECS - 1), 0).is_some());
    }

    /// A slow model can be silent for many minutes in the middle of an
    /// answer. Restarting then would throw the question away in front of
    /// the person waiting for it.
    #[test]
    fn a_question_without_its_answer_keeps_it_in_use_even_when_nothing_arrives() {
        let asked = T - 20 * 60;
        assert_eq!(
            busy_reason_at(T, asked, asked),
            Some("an answer is still on its way"),
            "twenty silent minutes into an answer is still an answer"
        );
    }

    /// …but a question whose host vanished must not block updates for ever.
    #[test]
    fn an_answer_that_never_comes_stops_counting_after_half_an_hour() {
        let asked = T - ANSWER_GRACE_SECS;
        assert_eq!(busy_reason_at(T, asked, asked), None);
    }

    /// The first minutes after a launch are somebody opening the app, or a
    /// restart we caused ourselves — neither is a moment to restart again.
    #[test]
    fn a_fresh_launch_is_not_idle() {
        assert!(busy_reason_at(T, 0, 0).is_some(), "launch not recorded yet");
        assert!(busy_reason_at(T, T, 0).is_some(), "launched this second");
    }
}
