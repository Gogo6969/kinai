//! Client-side WebSocket dialer.
//!
//! `supervise` is the long-running task spawned per client session — it
//! owns the reconnect loop with exponential backoff (2s → 4s → … capped
//! at 30s) so a host that comes online after the client started, or that
//! restarts mid-session, gets picked back up without user intervention.
//! Manual "Reconnect now" actions wake the sleeper through the
//! `NetState.client_wake` Notify.

use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::config::Mode;
use crate::SharedState;

use super::protocol::Envelope;

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

        let result = connect(state.clone(), app.clone(), try_url, try_token).await;
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

    let (ws, _) = match tokio_tungstenite::connect_async(&url_for_ws).await {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("Couldn't reach the KinAI host at {url}: {e}");
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

    while let Some(frame) = source.next().await {
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
            } => {
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
    state.net.lock().await.client_tx = None;
    {
        let mut stats = state.stats.write();
        stats.client_connected = false;
        // Leave `client_error` as whatever the host last sent (or None) so
        // the sidebar can surface "invite revoked" / "rate limit" / etc.
        // after a graceful close.
    }
    let _ = app.emit("kinai://client-status", serde_json::json!({"connected": false}));
    Ok(())
}

pub async fn disconnect(state: SharedState) -> Result<()> {
    let mut net = state.net.lock().await;
    if let Some(task) = net.client.take() {
        task.abort();
    }
    net.client_tx = None;
    Ok(())
}
