//! Client-side WebSocket dialer.
//!
//! `supervise` is the long-running task spawned per client session — it
//! owns the reconnect loop with exponential backoff (2s → 4s → … capped
//! at 30s) so a host that comes online after the client started, or that
//! restarts mid-session, gets picked back up without user intervention.
//! Manual "Reconnect now" wakes it through the `NetState.client_wake`
//! Notify — both while it sleeps between attempts AND while an attempt is
//! still dialing or waiting for the host, which it then abandons (see
//! `attempt_or_wake`).

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::config::Mode;
use crate::SharedState;

use super::protocol::Envelope;

/// How long dialing the host may take. Without a limit, a dial into
/// silence (host restarting, Wi-Fi hiccup, a VPN dropping LAN packets)
/// waited out the operating system's own connect timeout — about 75 s on
/// macOS, two minutes on Linux — while the "Reconnect now" button could do
/// nothing (2026-09-25: a client that had just updated stayed stuck until
/// it was restarted).
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the host has to answer our Hello with a Welcome. A host that
/// accepts the socket and then says nothing must not hold the attempt.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(15);

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Dial the host's WebSocket, giving up after `limit`.
pub(crate) async fn dial(url: &str, limit: Duration) -> std::result::Result<WsStream, String> {
    match tokio::time::timeout(limit, tokio_tungstenite::connect_async(url)).await {
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("no answer within {} s", limit.as_secs())),
    }
}

/// Run one connection attempt unless a manual reconnect interrupts it;
/// `None` = interrupted. The supervisor used to listen for the button only
/// while sleeping between attempts, and the button used `notify_waiters`,
/// which is not remembered — a press during an attempt was simply lost.
/// The button now uses `notify_one`, whose press is kept until this picks
/// it up.
pub(crate) async fn attempt_or_wake<T>(
    attempt: impl Future<Output = T>,
    wake: &tokio::sync::Notify,
) -> Option<T> {
    tokio::select! {
        r = attempt => Some(r),
        _ = wake.notified() => None,
    }
}

pub async fn auto_connect(state: SharedState, app: AppHandle) -> Result<()> {
    let (url, token) = {
        let cfg = state.config.read();
        match (cfg.client.host_url.clone(), cfg.client.host_token.clone()) {
            (Some(u), Some(t)) => (u, t),
            _ => anyhow::bail!("no host configured"),
        }
    };
    supervise(state, app, url, token).await;
    Ok(())
}

/// Wraps `connect` in a reconnect loop. Exits when the user switches
/// away from Client mode, clears their credentials, or the task is
/// `.abort()`ed (e.g. by `disconnect_client` or by `connect_client`
/// installing a replacement).
pub async fn supervise(state: SharedState, app: AppHandle, url: String, token: String) {
    let wake = state.net.lock().await.client_wake.clone();
    let mut backoff = Duration::from_secs(2);
    let max_backoff = Duration::from_secs(30);

    loop {
        if !still_a_client(&state) {
            tracing::info!("client supervise: no credentials / not Client mode, exiting");
            return;
        }

        // Read credentials fresh each iteration so a connect_client call
        // that swaps host_url/host_token mid-loop (then wakes us) picks
        // up the new values rather than the snapshot supervise was
        // spawned with.
        let (cur_url, cur_token) = {
            let cfg = state.config.read();
            match (cfg.client.host_url.clone(), cfg.client.host_token.clone()) {
                (Some(u), Some(t)) => (u, t),
                _ => {
                    tracing::info!("client supervise: credentials cleared");
                    return;
                }
            }
        };
        // First iteration uses the explicit args (matches old behavior);
        // subsequent iterations always reload — keeps tests + manual
        // unit calls deterministic.
        let (try_url, try_token) = if backoff == Duration::from_secs(2)
            && cur_url == url
            && cur_token == token
        {
            (url.clone(), token.clone())
        } else {
            (cur_url, cur_token)
        };

        let Some(result) =
            attempt_or_wake(connect(state.clone(), app.clone(), try_url, try_token), &wake).await
        else {
            // "Reconnect now" while the attempt was still dialing or
            // waiting for the host: drop it and dial again at once.
            tracing::info!("client supervise: reconnect pressed mid-attempt, dialing again");
            end_session(&state).await;
            let _ = app.emit("kinai://client-status", serde_json::json!({"connected": false}));
            backoff = Duration::from_secs(2);
            continue;
        };
        match result {
            Ok(()) => {
                tracing::info!("client supervise: WS closed cleanly, will retry");
                backoff = Duration::from_secs(2);
            }
            Err(e) => {
                tracing::warn!("client supervise: WS failed: {e:?}");
            }
        }

        if !still_a_client(&state) {
            return;
        }

        // Sleep until the backoff elapses OR a manual reconnect fires.
        tracing::info!("client supervise: waiting {:?} before retry", backoff);
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {
                backoff = (backoff * 2).min(max_backoff);
            }
            _ = wake.notified() => {
                tracing::info!("client supervise: woken for immediate retry");
                backoff = Duration::from_secs(2);
            }
        }
    }
}

fn still_a_client(state: &SharedState) -> bool {
    let cfg = state.config.read();
    matches!(cfg.mode, Mode::Client)
        && cfg.client.host_url.is_some()
        && cfg.client.host_token.is_some()
}

/// Open the WebSocket to the host, install a writer task driven by an
/// outbound channel stored on `state.net.client_tx`, and pump inbound
/// envelopes into Tauri events for the UI to consume.
pub async fn connect(
    state: SharedState,
    app: AppHandle,
    url: String,
    token: String,
) -> Result<()> {
    let display_name = state.config.read().client.display_name.clone();
    let url_for_ws = url.trim_start_matches("kinai://").to_string();

    let ws = match dial(&url_for_ws, DIAL_TIMEOUT).await {
        Ok(v) => v,
        Err(e) => {
            // A LAN address that times out is very often a VPN with its
            // kill switch on — the packets are dropped, so the error is a
            // bare timeout with no clue why. Ask the OS and, if a tunnel
            // is involved, tell the user what to actually change.
            let mut msg = format!("Couldn't reach the KinAI host at {url}: {e}");
            if let Some(hint) = super::vpn_hint::lan_vpn_hint(&url_for_ws).await {
                msg.push_str(&hint);
            }
            tracing::warn!("{msg}");
            {
                let mut stats = state.stats.write();
                stats.client_connected = false;
                stats.client_error = Some(msg.clone());
            }
            let _ = app.emit(
                "kinai://client-status",
                serde_json::json!({"connected": false, "error": msg.clone()}),
            );
            let _ = app.emit("kinai://error", &msg);
            return Err(anyhow::anyhow!(msg));
        }
    };
    let (mut sink, mut source) = ws.split();

    // Outbound channel + writer task. We store the tx half on the shared
    // NetState so command handlers (`send_message` in Client mode, etc.)
    // can hand envelopes to the live socket without holding the lock for
    // the duration of the network call.
    let (tx, mut rx) = mpsc::unbounded_channel::<Envelope>();
    {
        let mut net = state.net.lock().await;
        net.client_tx = Some(tx.clone());
    }

    // Hello first — every other frame waits behind this.
    let hello = Envelope::Hello {
        token,
        display_name,
        client_version: env!("CARGO_PKG_VERSION").into(),
        // So the host can schedule "9am" as THIS machine's 9am.
        tz: iana_time_zone::get_timezone().unwrap_or_default(),
    };
    if let Err(e) = sink
        .send(WsMessage::Text(serde_json::to_string(&hello)?.into()))
        .await
    {
        let msg = format!("KinAI host accepted the connection but rejected hello: {e}");
        {
            let mut stats = state.stats.write();
            stats.client_connected = false;
            stats.client_error = Some(msg.clone());
        }
        let _ = app.emit(
            "kinai://client-status",
            serde_json::json!({"connected": false, "error": msg.clone()}),
        );
        let _ = app.emit("kinai://error", &msg);
        state.net.lock().await.client_tx = None;
        return Err(anyhow::anyhow!(msg));
    }

    {
        let mut stats = state.stats.write();
        stats.client_connected = true;
        stats.client_error = None;
    }
    let _ = app.emit(
        "kinai://client-status",
        serde_json::json!({"connected": true, "url": url}),
    );

    // Re-check for updates on every successful (re)connect. The periodic
    // 4-hour poll is the steady-state fallback; this immediate check
    // closes the "host shipped a new version, client just (re)connected"
    // window so updates feel instantaneous instead of taking up to 4h
    // to be discovered. Spawned so the WS read loop isn't blocked.
    let app_for_update = app.clone();
    tauri::async_runtime::spawn(async move {
        // Small delay so the WS Hello + Welcome round-trip can finish
        // first — keeps the user-visible "Connected" indicator from
        // racing with an update banner appearing on the same tick.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        crate::updater::check_once(&app_for_update).await;
    });

    let writer = tokio::spawn(async move {
        while let Some(env) = rx.recv().await {
            let Ok(text) = serde_json::to_string(&env) else { continue };
            if sink.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    // The host must answer the Hello within WELCOME_TIMEOUT; after that the
    // session reads without a deadline.
    let welcome_by = tokio::time::Instant::now() + WELCOME_TIMEOUT;
    let mut welcomed = false;
    let mut no_welcome = false;
    loop {
        let next = if welcomed {
            source.next().await
        } else {
            match tokio::time::timeout_at(welcome_by, source.next()).await {
                Ok(n) => n,
                Err(_) => {
                    no_welcome = true;
                    break;
                }
            }
        };
        let Some(frame) = next else { break };
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("ws read error: {e}");
                break;
            }
        };
        if frame.is_close() {
            break;
        }
        let text = match frame.to_text() {
            Ok(t) => t.to_string(),
            Err(_) => continue,
        };
        let env: Envelope = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("bad envelope from host: {e}");
                continue;
            }
        };
        match env {
            Envelope::Token { client_msg_id, delta } => {
                let _ = app.emit(
                    "kinai://token",
                    serde_json::json!({"client_msg_id": client_msg_id, "delta": delta}),
                );
            }
            Envelope::Reasoning { client_msg_id, delta } => {
                let _ = app.emit(
                    "kinai://reasoning",
                    serde_json::json!({"client_msg_id": client_msg_id, "delta": delta}),
                );
            }
            Envelope::Tool { client_msg_id, event } => {
                let _ = app.emit(
                    "kinai://tool",
                    serde_json::json!({"client_msg_id": client_msg_id, "event": event}),
                );
            }
            Envelope::Message { message } => {
                let _ = app.emit("kinai://message", &message);
            }
            Envelope::AssistantDone {
                client_msg_id,
                message,
                metrics,
            } => {
                let _ = app.emit(
                    "kinai://assistant-done",
                    serde_json::json!({
                        "client_msg_id": client_msg_id,
                        "message": message,
                        "metrics": metrics,
                    }),
                );
            }
            Envelope::Welcome {
                family_name,
                host_version,
                host_model,
                host_search_engine,
                host_vision,
                host_telegram_bot,
                host_slots,
                host_fact_check,
                host_reports,
                host_thread_ops,
                host_reminders,
                host_message_delete,
            } => {
                welcomed = true;
                {
                    let mut stats = state.stats.write();
                    stats.host_info = Some(crate::HostInfo {
                        family_name: family_name.clone(),
                        host_version: host_version.clone(),
                        host_model: host_model.clone(),
                        host_search_engine: host_search_engine.clone(),
                        host_vision: host_vision.clone(),
                        host_telegram_bot: host_telegram_bot.clone(),
                        host_slots: host_slots.clone(),
                        host_fact_check,
                        host_reports,
                        host_thread_ops,
                        host_reminders,
                        host_message_delete,
                    });
                }
                let _ = app.emit(
                    "kinai://welcome",
                    serde_json::json!({
                        "family_name": family_name,
                        "host_version": host_version,
                        "host_model": host_model,
                        "host_search_engine": host_search_engine,
                        "host_vision": host_vision,
                        "host_telegram_bot": host_telegram_bot,
                        "host_slots": host_slots,
                        "host_fact_check": host_fact_check,
                        "host_reports": host_reports,
                        "host_thread_ops": host_thread_ops,
                        "host_reminders": host_reminders,
                        "host_message_delete": host_message_delete,
                    }),
                );
            }
            Envelope::Threads { threads } => {
                let mut net = state.net.lock().await;
                if let Some(tx) = net.threads_pending.take() {
                    let _ = tx.send(threads.clone());
                }
                drop(net);
                let _ = app.emit("kinai://threads", &threads);
            }
            Envelope::ThreadMessages { thread_id, messages } => {
                let mut net = state.net.lock().await;
                if let Some(tx) = net.thread_messages_pending.take() {
                    let _ = tx.send(messages.clone());
                }
                drop(net);
                let _ = app.emit(
                    "kinai://thread-loaded",
                    serde_json::json!({"thread_id": thread_id, "messages": messages}),
                );
            }
            // Host's reply to any of List/Save/Delete/Clear UserFacts.
            // Pipe the fresh list back through whichever oneshot is
            // parked, so the awaiting Tauri command resolves with the
            // new facts. The host always sends the full list, never
            // partial diffs, so the UI is always in sync with one
            // round-trip per action.
            //
            // IMPORTANT: do NOT emit `kinai://user-facts-updated` here.
            // The Settings → Memory page subscribes to that event and
            // re-calls `load()` when it fires — so emitting on every
            // WS response races the explicit `await load()` that the
            // mutation handler (delete / save / clear) already runs
            // right after the IPC resolves. The race manifested as:
            //
            //   * the second load()'s sender displaced the first in
            //     `user_facts_pending`, leaving the first to time out
            //     after 10s ("timed out waiting for host's user_facts
            //     response")
            //   * the page flickered through loading→facts→loading→
            //     facts as the two competing load() calls fought
            //
            // The event is still legitimately used on the HOST side
            // where the background extractor writes a fact and the
            // Settings page wants to refresh without polling. That
            // emit lives in commands.rs after the extractor save and
            // is unaffected by this change.
            Envelope::UserFacts { facts } => {
                let mut net = state.net.lock().await;
                if let Some(tx) = net.user_facts_pending.take() {
                    let _ = tx.send(facts);
                }
                drop(net);
            }
            Envelope::Reminders { items } => {
                let mut net = state.net.lock().await;
                if let Some(tx) = net.reminders_pending.take() {
                    let _ = tx.send(items);
                }
            }
            Envelope::ReminderActionAck {
                id,
                ok,
                message,
                reminder,
            } => {
                let mut net = state.net.lock().await;
                if let Some(tx) = net.reminder_action_pending.remove(&id) {
                    let _ = tx.send((ok, message, reminder));
                }
            }
            Envelope::Reminder { reminder } => {
                // Same event name the host window uses, so the frontend has
                // one code path for "a reminder just came due".
                let _ = app.emit(crate::reminders::EVENT, &reminder);
            }
            Envelope::Error { message } => {
                // Surface authoritative host-side errors (e.g. "invite revoked",
                // "rate limit exceeded") on the sidebar status pill in addition
                // to the global error toast.
                {
                    let mut stats = state.stats.write();
                    stats.client_error = Some(message.clone());
                }
                let _ = app.emit("kinai://error", &message);
            }
            Envelope::PromptDebug {
                assistant_msg_id,
                prompt,
            } => {
                let _ = app.emit(
                    "kinai://prompt-debug",
                    serde_json::json!({
                        "assistant_msg_id": assistant_msg_id,
                        "prompt": prompt,
                    }),
                );
            }
            // Telegram pairing round-trip responses. Each one resolves
            // a pending oneshot stashed by the matching Tauri command —
            // see `src/commands.rs` `request_telegram_pair` etc. We
            // also emit a Tauri event so the Settings UI can refresh
            // proactively (e.g. the status poll loop).
            Envelope::TelegramPair {
                url,
                expires_in_secs,
                bot_username,
            } => {
                let mut net = state.net.lock().await;
                if let Some(tx) = net.telegram_pair_pending.take() {
                    let _ = tx.send(super::TelegramPairWire {
                        url: url.clone(),
                        expires_in_secs,
                        bot_username: bot_username.clone(),
                    });
                }
                drop(net);
                let _ = app.emit(
                    "kinai://telegram-pair",
                    serde_json::json!({
                        "url": url,
                        "expires_in_secs": expires_in_secs,
                        "bot_username": bot_username,
                    }),
                );
            }
            Envelope::ThreadOpAck { thread_id, ok, message } => {
                let mut net = state.net.lock().await;
                if let Some(tx) = net.thread_op_pending.remove(&thread_id) {
                    let _ = tx.send((ok, message));
                }
            }
            Envelope::ReportAck { message_id, ok, message } => {
                let mut net = state.net.lock().await;
                if let Some(tx) = net.report_pending.remove(&message_id) {
                    let _ = tx.send((ok, message));
                }
            }
            Envelope::FactCheckResult { message_id, ok, report } => {
                let mut net = state.net.lock().await;
                if let Some(tx) = net.fact_check_pending.remove(&message_id) {
                    let _ = tx.send((ok, report));
                }
            }
            Envelope::SlotHealth { slots } => {
                // Keep the stored advertisement current so later
                // runtime_stats hydrations see fresh liveness too.
                {
                    let mut stats = state.stats.write();
                    if let Some(info) = stats.host_info.as_mut() {
                        info.host_slots = slots.clone();
                    }
                }
                let mut net = state.net.lock().await;
                if let Some(tx) = net.slot_health_pending.take() {
                    let _ = tx.send(slots.clone());
                }
            }
            Envelope::TelegramStatus {
                bot_configured,
                bot_username,
                paired,
                username,
                first_name,
                paired_at,
            } => {
                let mut net = state.net.lock().await;
                if let Some(tx) = net.telegram_status_pending.take() {
                    let _ = tx.send(super::TelegramStatusWire {
                        bot_configured,
                        bot_username: bot_username.clone(),
                        paired,
                        username: username.clone(),
                        first_name: first_name.clone(),
                        paired_at: paired_at.clone(),
                    });
                }
                drop(net);
                let _ = app.emit(
                    "kinai://telegram-status",
                    serde_json::json!({
                        "bot_configured": bot_configured,
                        "bot_username": bot_username,
                        "paired": paired,
                        "username": username,
                        "first_name": first_name,
                        "paired_at": paired_at,
                    }),
                );
            }
            Envelope::TelegramUnpairDone => {
                let mut net = state.net.lock().await;
                if let Some(tx) = net.telegram_unpair_pending.take() {
                    let _ = tx.send(());
                }
                drop(net);
                let _ = app.emit("kinai://telegram-unpair-done", serde_json::json!({}));
            }
            _ => {}
        }
    }

    writer.abort();
    end_session(&state).await;
    if no_welcome {
        let msg = format!(
            "The KinAI host at {url} accepted the connection but didn't answer within {} s.",
            WELCOME_TIMEOUT.as_secs()
        );
        tracing::warn!("{msg}");
        state.stats.write().client_error = Some(msg.clone());
        let _ = app.emit(
            "kinai://client-status",
            serde_json::json!({"connected": false, "error": msg.clone()}),
        );
        return Err(anyhow::anyhow!(msg));
    }
    let _ = app.emit("kinai://client-status", serde_json::json!({"connected": false}));
    Ok(())
}

/// Tear down what a session (or an abandoned attempt) left behind: the
/// outbound channel — dropping it also ends the writer task and closes the
/// socket — every command still waiting on the host, and the connected
/// flag.
async fn end_session(state: &SharedState) {
    {
        let mut net = state.net.lock().await;
        net.client_tx = None;
        // Resolve any in-flight fact-check waiters with an honest error —
        // otherwise their commands sit blind until the 120s timeout.
        for (_, tx) in net.fact_check_pending.drain() {
            let _ = tx.send((
                false,
                "Lost the connection to your KinAI host during the fact check.".to_string(),
            ));
        }
        for (_, tx) in net.thread_op_pending.drain() {
            let _ = tx.send((
                false,
                "Lost the connection to your KinAI host — the change wasn't saved.".to_string(),
            ));
        }
        for (_, tx) in net.reminder_action_pending.drain() {
            let _ = tx.send((
                false,
                "Lost the connection to your KinAI host — the reminder wasn't updated.".to_string(),
                None,
            ));
        }
        net.reminders_pending = None;
        for (_, tx) in net.report_pending.drain() {
            let _ = tx.send((
                false,
                "Lost the connection to your KinAI host — the report wasn't sent.".to_string(),
            ));
        }
    }
    {
        let mut stats = state.stats.write();
        stats.client_connected = false;
        // Leave `client_error` as whatever the host last sent (or None) so
        // the sidebar can surface "invite revoked" / "rate limit" / etc.
        // after a graceful close.
    }
}

pub async fn disconnect(state: SharedState) -> Result<()> {
    let mut net = state.net.lock().await;
    if let Some(task) = net.client.take() {
        task.abort();
    }
    net.client_tx = None;
    Ok(())
}

#[cfg(test)]
mod reconnect_tests {
    use super::{attempt_or_wake, dial};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::Notify;

    /// A host that accepts the TCP connection and then says nothing — the
    /// shape of a half-restarted host or a path that drops packets. The
    /// dial must give up at its limit instead of waiting for the OS.
    #[tokio::test]
    async fn a_silent_host_times_out_the_dial() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _hold = tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let started = Instant::now();
        let err = dial(&format!("ws://127.0.0.1:{port}/kin"), Duration::from_millis(300))
            .await
            .expect_err("a silent host must not count as connected");
        assert!(err.contains("no answer within"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    }

    /// "Reconnect now" during an attempt that would otherwise hang forever.
    #[tokio::test]
    async fn reconnect_interrupts_a_hanging_attempt() {
        let wake = Arc::new(Notify::new());
        let w = wake.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            w.notify_one();
        });
        let out = tokio::time::timeout(
            Duration::from_secs(2),
            attempt_or_wake(std::future::pending::<()>(), &wake),
        )
        .await
        .expect("the press must end the attempt");
        assert_eq!(out, None);
    }

    /// A press that lands while nothing is listening yet is not lost.
    #[tokio::test]
    async fn a_press_made_before_anyone_listens_is_kept() {
        let wake = Notify::new();
        wake.notify_one();
        let out = tokio::time::timeout(
            Duration::from_secs(1),
            attempt_or_wake(std::future::pending::<()>(), &wake),
        )
        .await
        .expect("the stored press must be picked up");
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn a_finished_attempt_is_returned() {
        let wake = Notify::new();
        assert_eq!(attempt_or_wake(async { 7 }, &wake).await, Some(7));
    }
}
