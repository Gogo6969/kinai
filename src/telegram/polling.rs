//! getUpdates long-poll loop. Runs forever (until aborted by the
//! supervisor in mod.rs) and feeds each incoming update into
//! `router::handle_update`.

use std::time::Duration;

use tauri::{AppHandle, Runtime};

use crate::SharedState;

use super::api::BotApi;
use super::router;

/// Seconds we ask Telegram to hold each long-poll request open before
/// returning with an empty result.
const LONG_POLL_TIMEOUT_SECS: u32 = 30;

pub async fn run<R: Runtime>(api: BotApi, state: SharedState, app: AppHandle<R>) {
    let mut offset: i64 = 0;
    // After consecutive failures, back off so we don't hammer Telegram
    // when there's an outage (or invalid token). Capped at 60s.
    let mut backoff = Duration::from_secs(1);
    loop {
        match api.get_updates(offset, LONG_POLL_TIMEOUT_SECS).await {
            Ok(updates) => {
                backoff = Duration::from_secs(1);
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
                            tracing::warn!("telegram router: {e:?}");
                        }
                    });
                }
            }
            Err(e) => {
                tracing::warn!("telegram getUpdates failed: {e:?} — backing off {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}
