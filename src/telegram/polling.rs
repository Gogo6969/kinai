//! getUpdates long-poll loop. Runs forever (until aborted by the
//! supervisor in mod.rs) and feeds each incoming update into
//! `router::handle_update`.

use std::time::Duration;

use tauri::{AppHandle, Runtime};

use crate::SharedState;

use super::api::BotApi;
use super::redact_token;
use super::router;

/// Seconds we ask Telegram to hold each long-poll request open before
/// returning with an empty result.
const LONG_POLL_TIMEOUT_SECS: u32 = 30;

/// A message normally reaches us within a second or two of being sent.
/// From here on it counts as late and gets a warning in the log.
const LATE_AFTER_SECS: i64 = 10;
/// Telegram stamps messages in whole seconds and the two clocks are never
/// exactly equal. An earlier poll only counts as having MISSED a message
/// when it came back at least this long after the message was sent.
const CLOCK_SLACK_SECS: i64 = 3;

/// How a message reached us, judged by its own timestamp.
///
/// The question this answers came up for real: a question typed into
/// Telegram got no "typing…" dots and an answer only much later. No dots
/// means KinAI did not have the message yet — but the log could not show
/// WHY, because it never recorded when a message had been sent. Now every
/// message says how old it was on arrival, and a late one says which
/// story it is.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Arrival {
    /// Here within seconds of being sent.
    Prompt { age: i64 },
    /// Late, although KinAI was asking: an earlier poll came back
    /// `missed_by` seconds AFTER this message was sent, and did not
    /// contain it. The wait was on Telegram's side.
    MissedByEarlierPoll { age: i64, missed_by: i64 },
    /// Late, although KinAI was asking: the very poll that brought it had
    /// been open for `waited` seconds while the message already existed.
    /// A long poll returns at once when something is there — so it was
    /// not there. The wait was on Telegram's side.
    SlowInOpenPoll { age: i64, waited: i64 },
    /// Late because KinAI was not asking when it was sent — it had only
    /// just started, or its polls were failing — and it came with the
    /// first poll that could bring it. The wait was on our side.
    NotPolling { age: i64 },
}

/// All in Unix seconds: `sent` by Telegram's clock; `asked` and `now`
/// when the poll that brought the message went out and came back;
/// `previous_poll` when the last successful poll before it came back
/// (None for the first one).
pub(crate) fn classify_arrival(
    sent: i64,
    asked: i64,
    now: i64,
    previous_poll: Option<i64>,
) -> Option<Arrival> {
    if sent <= 0 {
        return None; // no timestamp on it — nothing to say
    }
    let age = now - sent;
    if age < LATE_AFTER_SECS {
        return Some(Arrival::Prompt { age });
    }
    if let Some(prev) = previous_poll {
        if prev - sent >= CLOCK_SLACK_SECS {
            return Some(Arrival::MissedByEarlierPoll {
                age,
                missed_by: prev - sent,
            });
        }
    }
    // How long this poll was open while the message already existed.
    let waited = now - sent.max(asked);
    if waited >= LATE_AFTER_SECS {
        return Some(Arrival::SlowInOpenPoll { age, waited });
    }
    Some(Arrival::NotPolling { age })
}

/// One log line per message. Numbers only — never who wrote or what.
fn log_arrival(update_id: i64, arrival: &Arrival) {
    match arrival {
        Arrival::Prompt { age } => {
            // Two clocks: ours can be a second behind Telegram's. "-1 s"
            // in a log only raises questions.
            let age = (*age).max(0);
            tracing::info!("telegram: update {update_id} arrived {age} s after it was sent");
        }
        Arrival::MissedByEarlierPoll { age, missed_by } => {
            tracing::warn!(
                "telegram: update {update_id} arrived LATE, {age} s after it was sent — Telegram held it back: \
                 an earlier poll came back {missed_by} s after it was sent and did not contain it"
            );
        }
        Arrival::SlowInOpenPoll { age, waited } => {
            tracing::warn!(
                "telegram: update {update_id} arrived LATE, {age} s after it was sent — Telegram held it back: \
                 KinAI's poll was open and waiting for {waited} s of that"
            );
        }
        Arrival::NotPolling { age } => {
            tracing::warn!(
                "telegram: update {update_id} arrived LATE, {age} s after it was sent — KinAI was not asking \
                 Telegram at that time (just started, or its polls were failing)"
            );
        }
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub async fn run<R: Runtime>(api: BotApi, state: SharedState, app: AppHandle<R>) {
    let mut offset: i64 = 0;
    // After consecutive failures, back off so we don't hammer Telegram
    // when there's an outage (or invalid token). Capped at 60s.
    let mut backoff = Duration::from_secs(1);
    // When the last successful poll came back — what a late message is
    // judged against (see `classify_arrival`).
    let mut previous_poll: Option<i64> = None;
    loop {
        let asked = now_unix();
        match api.get_updates(offset, LONG_POLL_TIMEOUT_SECS).await {
            Ok(updates) => {
                backoff = Duration::from_secs(1);
                let arrived = now_unix();
                for update in &updates {
                    let sent = update.message.as_ref().map(|m| m.date).unwrap_or(0);
                    if let Some(arrival) = classify_arrival(sent, asked, arrived, previous_poll) {
                        log_arrival(update.update_id, &arrival);
                    }
                }
                previous_poll = Some(arrived);
                for update in updates {
                    // Advance offset to update_id + 1 so the next poll
                    // doesn't redeliver this one.
                    offset = offset.max(update.update_id + 1);
                    // Handle each update in its OWN task instead of awaiting
                    // it inline. A single slow update — notably `/pic` /
                    // `/picHQ`, which block on ComfyUI for 5-30s (up to a
                    // 6-minute poll) — would otherwise freeze the whole poll
                    // loop: no further getUpdates runs, so every other
                    // family member's messages pile up unanswered and the
                    // bot looks dead. Spawning keeps polling responsive and
                    // also isolates a panic to the one update (it can't tear
                    // down the loop).
                    let api = api.clone();
                    let state = state.clone();
                    let app = app.clone();
                    tokio::spawn(async move {
                        if let Err(e) = router::handle_update(&api, &state, &app, &update).await {
                            tracing::warn!("telegram router: {}", redact_token(&format!("{e:?}")));
                        }
                    });
                }
            }
            Err(e) => {
                // Redacted twice over: `api::scrub` already strips the token
                // from every Bot API error, and this is the loop that logs
                // one on every failed poll — four of the five tokens that
                // reached ~/.kinai/logs/ came out of here, not the startup
                // getMe. Cheap insurance on the highest-frequency path, and
                // `handle_update` above can surface an error from any
                // subsystem, not only Telegram.
                tracing::warn!(
                    "telegram getUpdates failed: {} — backing off {backoff:?}",
                    redact_token(&format!("{e:?}"))
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

#[cfg(test)]
mod arrival_tests {
    use super::*;

    const T: i64 = 2_000_000_000;

    #[test]
    fn a_message_that_arrives_within_seconds_is_just_noted() {
        assert_eq!(
            classify_arrival(T - 1, T - 20, T, Some(T - 20)),
            Some(Arrival::Prompt { age: 1 })
        );
        // Telegram's clock a second ahead of ours is not news either.
        assert_eq!(
            classify_arrival(T + 1, T - 20, T, Some(T - 20)),
            Some(Arrival::Prompt { age: -1 })
        );
    }

    /// The case from the family: KinAI was polling the whole time, and a
    /// message still turned up minutes after it was sent. A poll that came
    /// back in between without it is the proof that Telegram had not
    /// handed it over.
    #[test]
    fn late_although_an_earlier_poll_came_back_without_it_is_telegrams_delay() {
        let sent = T - 140;
        let earlier_poll = T - 30; // came back 110 s after the message was sent
        assert_eq!(
            classify_arrival(sent, T - 30, T, Some(earlier_poll)),
            Some(Arrival::MissedByEarlierPoll { age: 140, missed_by: 110 })
        );
    }

    /// The same delay, shorter: the message was sent INTO a poll that was
    /// already open, and that poll still took a quarter of a minute to
    /// come back with it. No earlier poll to point at — but an open long
    /// poll returns the moment something is there.
    #[test]
    fn late_inside_a_poll_that_was_already_waiting_is_telegrams_delay() {
        let asked = T - 25;
        let sent = T - 14;
        assert_eq!(
            classify_arrival(sent, asked, T, Some(asked)),
            Some(Arrival::SlowInOpenPoll { age: 14, waited: 14 })
        );
    }

    /// The opposite story must not be blamed on Telegram: messages sent
    /// while KinAI was down, or while its polls were failing, are old on
    /// arrival because nobody was there to take them — and they come with
    /// the first poll that asks.
    #[test]
    fn late_because_we_were_not_asking_is_our_delay() {
        // First poll after a start, answered at once.
        assert_eq!(
            classify_arrival(T - 500, T, T, None),
            Some(Arrival::NotPolling { age: 500 })
        );
        // The last good poll was BEFORE the message was sent; the polls
        // in between failed; this one went out a second ago.
        assert_eq!(
            classify_arrival(T - 60, T - 1, T, Some(T - 200)),
            Some(Arrival::NotPolling { age: 60 })
        );
    }

    /// Whole-second timestamps and two clocks: a poll that came back in
    /// the same second or two as the message was sent proves nothing.
    #[test]
    fn a_poll_that_returned_as_the_message_was_sent_did_not_miss_it() {
        assert_eq!(
            classify_arrival(T - 12, T - 2, T, Some(T - 11)),
            Some(Arrival::NotPolling { age: 12 })
        );
        assert_eq!(
            classify_arrival(T - 12, T - 2, T, Some(T - 12 + CLOCK_SLACK_SECS)),
            Some(Arrival::MissedByEarlierPoll { age: 12, missed_by: CLOCK_SLACK_SECS })
        );
    }

    #[test]
    fn an_update_without_a_timestamp_says_nothing() {
        assert_eq!(classify_arrival(0, T - 5, T, Some(T - 5)), None);
    }

    /// Telegram really sends the field, under this name.
    #[test]
    fn the_sent_time_is_read_from_the_message() {
        let update: crate::telegram::api::TelegramUpdate = serde_json::from_value(serde_json::json!({
            "update_id": 7,
            "message": {
                "message_id": 3,
                "date": 2_000_000_123i64,
                "chat": {"id": 1, "type": "private"},
                "text": "is the bakery open on sunday"
            }
        }))
        .unwrap();
        assert_eq!(update.message.unwrap().date, 2_000_000_123);
    }
}
