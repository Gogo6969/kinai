//! Reminder scheduler — the host-side loop that turns a stored reminder
//! into a popup on the member's devices and a message in their Telegram
//! chat when its time comes.
//!
//! Spawned once in every mode (like the Telegram supervisor) and gated on
//! `Mode::Host` per tick, so a machine that becomes the host through the
//! setup wizard gets a scheduler without a relaunch. Every 30 s it leases
//! the due rows atomically (`scheduled → firing`), marks each `fired`, and
//! delivers the fired row to (a) the host window when the owner is the
//! host user, (b) every live WebSocket session of a client member, (c) the
//! member's Telegram chat when paired. The row is the source of truth: a
//! device that is offline reads it on its next launch and shows the popup
//! then. Rows left in `firing` by a crash are re-queued after two minutes.
//!
//! Privacy: log lines carry ids and counts, never the reminder text.

use std::time::Duration;

use tauri::{AppHandle, Emitter, Runtime};

use crate::config::Mode;
use crate::db::Reminder;
use crate::network::protocol::Envelope;
use crate::SharedState;

/// The Tauri event the host window listens for; the client re-emits the
/// same name when `Envelope::Reminder` arrives, so the frontend has one path.
pub const EVENT: &str = "kinai://reminder";

const STARTUP_DELAY: Duration = Duration::from_secs(15);
const TICK: Duration = Duration::from_secs(30);
/// A lease older than this belongs to a tick that never finished.
const STUCK_AFTER_MINUTES: i64 = 2;

pub fn spawn<R: Runtime>(state: SharedState, app: AppHandle<R>) {
    // `tauri::async_runtime::spawn`, not `tokio::spawn`: setup runs on the
    // main thread before Tauri's runtime is entered (see lib.rs).
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(STARTUP_DELAY).await;
        loop {
            let is_host = matches!(state.config.read().mode, Mode::Host);
            if is_host {
                if let Err(e) = tick(&state, &app).await {
                    tracing::warn!("reminders: tick failed: {e:#}");
                }
            }
            tokio::time::sleep(TICK).await;
        }
    });
}

async fn tick<R: Runtime>(state: &SharedState, app: &AppHandle<R>) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    let released = state
        .db
        .release_stuck_reminders(now - chrono::Duration::minutes(STUCK_AFTER_MINUTES))
        .await?;
    if released > 0 {
        tracing::warn!(count = released, "reminders: re-queued rows stuck in delivery");
    }
    let due = state.db.lease_due_reminders(now).await?;
    if due.is_empty() {
        return Ok(());
    }
    tracing::info!(count = due.len(), "reminders: due");
    for leased in due {
        // Mark first, then deliver: if the process dies in between, the row
        // is `fired` and the member's device surfaces it on its next load —
        // better than a re-delivery loop.
        if !state
            .db
            .mark_reminder_fired(&leased.peer_id, &leased.id)
            .await
            .unwrap_or(false)
        {
            tracing::warn!(id = %leased.id, "reminders: could not mark fired");
            continue;
        }
        let fired = match state.db.get_reminder(&leased.peer_id, &leased.id).await {
            Ok(Some(r)) => r,
            _ => leased,
        };
        deliver(state, app, &fired).await;
    }
    Ok(())
}

/// Best effort on every channel; failures are logged, never retried here.
async fn deliver<R: Runtime>(state: &SharedState, app: &AppHandle<R>, r: &Reminder) {
    if r.peer_id == crate::db::HOST_PEER {
        if let Err(e) = app.emit(EVENT, r) {
            tracing::warn!(id = %r.id, "reminders: host emit failed: {e:?}");
        }
    } else {
        // Every live session of that member — one member can be connected
        // twice mid-reconnect; the UI dedupes by id. Hold the lock only to
        // send (UnboundedSender::send never awaits).
        let sent = {
            let net = state.net.lock().await;
            net.peers
                .values()
                .filter(|info| info.invite_id == r.peer_id)
                .map(|info| {
                    info.tx.send(Envelope::Reminder {
                        reminder: r.clone(),
                    })
                })
                .filter(|res| res.is_ok())
                .count()
        };
        tracing::info!(id = %r.id, sessions = sent, "reminders: pushed to client sessions");
    }
    deliver_telegram(state, r).await;
}

async fn deliver_telegram(state: &SharedState, r: &Reminder) {
    let token = state.config.read().telegram.bot_token.clone();
    if token.trim().is_empty() {
        return;
    }
    let chat = match crate::db::telegram::chat_for_peer(&state.db.pool, &r.peer_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return, // not paired — nothing to do
        Err(e) => {
            tracing::warn!(id = %r.id, "reminders: telegram lookup failed: {e:#}");
            return;
        }
    };
    let Ok(chat_id) = chat.parse::<i64>() else {
        tracing::warn!(id = %r.id, "reminders: telegram chat id is not numeric");
        return;
    };
    let text = format!(
        "⏰ Reminder: {}\n\nAcknowledge or snooze it in KinAI's Calendar.",
        r.text
    );
    let api = crate::telegram::BotApi::new(token);
    match api.send_message(chat_id, &text).await {
        Ok(_) => tracing::info!(id = %r.id, "reminders: sent to telegram"),
        Err(e) => tracing::warn!(id = %r.id, "reminders: telegram send failed: {e:?}"),
    }
}
