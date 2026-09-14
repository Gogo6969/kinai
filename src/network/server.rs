//! Host-side Axum HTTP+WebSocket server.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::context;
use crate::network::ratelimit::RateLimiter;
use crate::tools::loop_pipeline::{PipelineHandlers, ToolEvent};
use crate::tools::registry;

use super::invite;
use super::protocol::Envelope;
use super::PeerInfo;
use crate::SharedState;

/// Guesses per minute, per source IP, allowed against `/v1/invite/redeem`.
///
/// Deliberately its own constant and its own limiter instance rather than
/// the configurable `rate_limit_rpm` one: that is user-settable and
/// `RateLimiter::allow` returns true unconditionally when it is 0, so a
/// household that turned chat throttling off would also have turned off
/// the only thing standing between a 6-character code and a brute force.
/// A person types a code once; ten a minute is generous.
const REDEEM_RPM: u32 = 10;

#[derive(Clone)]
pub(crate) struct AxumState {
    pub(crate) app: SharedState,
    pub(crate) tauri: AppHandle,
    pub(crate) rate: Arc<RateLimiter>,
    /// Keyed on client IP, not on a JWT subject — redeem is the one route
    /// that runs before there is any identity to key on.
    pub(crate) redeem_rate: Arc<RateLimiter>,
}

pub async fn start(state: SharedState, app: AppHandle) -> Result<()> {
    let (bind_addr, port, rpm) = {
        let cfg = state.config.read();
        (cfg.host.bind_addr.clone(), cfg.host.port, cfg.host.rate_limit_rpm)
    };

    crate::discovery::start_advertise(&state, app.clone());

    let axum_state = AxumState {
        app: state.clone(),
        tauri: app.clone(),
        rate: Arc::new(RateLimiter::new(rpm)),
        redeem_rate: Arc::new(RateLimiter::new(REDEEM_RPM)),
    };

    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/info", get(info))
        .route("/v1/invite/redeem", get(redeem_invite))
        .route("/v1/update/manifest", get(super::updates::manifest))
        // New multi-platform routes — Tauri's updater follows the
        // ?target=… URL out of the manifest.
        .route("/v1/update/bundle", get(super::updates::bundle))
        .route("/v1/update/signature", get(super::updates::signature_route))
        // Legacy single-platform routes (v0.2.x clients still hit these).
        .route("/v1/update/bundle.tar.gz", get(super::updates::bundle_legacy))
        .route("/v1/update/bundle.tar.gz.sig", get(super::updates::signature_legacy))
        // Generated images from /pic + /picHQ slash commands.
        // Axum 0.7 path-parameter syntax is `:name` (Axum 0.8+ uses {name}).
        .route("/v1/pic/:filename", get(super::pics::serve_pic))
        // The reminder API: the one way in that is not a chat turn.
        .route(
            "/v1/reminders",
            post(super::api::create).get(super::api::list),
        )
        .route("/v1/reminders/:id/action", post(super::api::action))
        .route("/kin", any(ws_upgrade))
        .with_state(axum_state);

    let listen: SocketAddr = format!("{bind_addr}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!("KinAI host listening on ws://{listen}/kin");

    // Derive the URL we'll *validate JWTs against* from the LAN IP — same
    // canonical form `invite::public_host_url` uses when stamping the
    // JWT's `aud` claim. Using the bind socket here would publish
    // "ws://0.0.0.0:PORT/kin" which no client ever reaches us at, and
    // crucially never matches the JWT's audience either — so every Hello
    // frame would be rejected.
    let public_host_url = invite::public_host_url(&state.config.read());
    {
        let mut stats = state.stats.write();
        stats.host_url = Some(public_host_url.clone());
    }
    let _ = app.emit(
        "kinai://host-status",
        serde_json::json!({"running": true, "addr": listen.to_string()}),
    );

    // `.into_make_service_with_connect_info` is what makes the
    // `ConnectInfo<SocketAddr>` extractor on `redeem_invite` resolve.
    // Without it that handler compiles and then 500s on every redeem,
    // and the family member sees the raw error text.
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

#[derive(Serialize)]
struct HealthResp {
    ok: bool,
    version: &'static str,
}

async fn healthz() -> Json<HealthResp> {
    Json(HealthResp {
        ok: true,
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[derive(Serialize)]
struct InfoResp {
    family_name: String,
    host_version: &'static str,
    model: String,
}

/// Unauthenticated, so it says as little as it can while still letting a
/// device confirm it has found the right household before pairing.
///
/// It used to include a live count of connected devices. That is a
/// presence signal, not a capability one: polled once a second by anything
/// on the subnet it draws an at-home/away timeline for the household, from
/// a route with no authentication and no throttle. Nothing needs it — the
/// count reaches the host's own UI through Tauri, never over HTTP.
///
/// `family_name` now follows the mDNS setting. With advertising on it is
/// already being broadcast subnet-wide, so serving it here costs nothing;
/// with advertising deliberately turned off, continuing to hand it to any
/// caller quietly undid the toggle the household had just set.
async fn info(State(s): State<AxumState>) -> Json<InfoResp> {
    let cfg = s.app.config.read().clone();
    Json(InfoResp {
        family_name: if cfg.host.mdns_enabled {
            cfg.host.family_name
        } else {
            String::new()
        },
        host_version: env!("CARGO_PKG_VERSION"),
        model: cfg.llm.model,
    })
}

#[derive(Debug, Deserialize)]
struct RedeemQuery {
    code: String,
}

#[derive(Serialize)]
struct RedeemResp {
    host_url: String,
    token: String,
    label: String,
}

/// `GET /v1/invite/redeem?code=XXXXXX` — clients on the LAN type the
/// 6-character short code shown on the host's invite UI and we resolve it
/// to the full JWT they need to open the WebSocket. The host URL we return
/// is the one stored on the invite at creation time — clients should
/// connect to that (NOT the IP they used to reach this endpoint, which may
/// differ if the host has multiple interfaces).
/// This route is unauthenticated by necessity — it is how a device with
/// nothing but a code gets its first credential — so it is throttled per
/// source IP and every rejection is logged. Without both, it answered as a
/// clean oracle (404 wrong, 200 plus the JWT right) as fast as the network
/// allowed, and left no trace that anyone had been guessing.
///
/// Note the `Query<RedeemQuery>` extractor runs BEFORE this body, so a
/// request with no `code` at all is rejected by axum and never reaches the
/// limiter. Every request that names a code does.
async fn redeem_invite(
    State(s): State<AxumState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(q): Query<RedeemQuery>,
) -> Result<Json<RedeemResp>, (StatusCode, String)> {
    let ip = addr.ip().to_string();
    if !s.redeem_rate.allow(&ip) {
        tracing::warn!(peer = %ip, "invite redeem throttled");
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts; wait a minute and try again".into(),
        ));
    }
    let code = q.code.trim().to_lowercase();
    if code.len() != 6 {
        return Err((
            StatusCode::BAD_REQUEST,
            "invite code must be exactly 6 characters".into(),
        ));
    }
    match invite::lookup_by_short_code(&s.app.db.pool, &code).await {
        Ok(r) => Ok(Json(RedeemResp {
            host_url: r.host_url,
            token: r.token,
            label: r.label,
        })),
        Err(e) => {
            // The code and the token are both credentials; only ever the
            // source and the reason.
            tracing::warn!(peer = %ip, "invite redeem rejected: {e}");
            Err((StatusCode::NOT_FOUND, e.to_string()))
        }
    }
}

async fn ws_upgrade(
    State(s): State<AxumState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // 4 MiB per frame: comfortably above a big pasted question with an
    // image data URL, far below tungstenite's 64 MiB default (which let
    // one `ReportAnswer` write 64 MiB straight into the host's DB).
    ws.max_message_size(4 * 1024 * 1024)
        .on_upgrade(move |socket| handle_socket(s, socket))
}

async fn handle_socket(s: AxumState, socket: WebSocket) {
    if let Err(e) = run_socket(s, socket).await {
        tracing::info!("ws conn ended: {e:?}");
    }
}

async fn run_socket(s: AxumState, socket: WebSocket) -> anyhow::Result<()> {
    let (mut sink, mut source) = socket.split();

    // First frame must be Hello.
    let hello_frame = source
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("no hello"))??;
    let text = frame_text(&hello_frame)
        .ok_or_else(|| anyhow::anyhow!("hello not text"))?;
    let envelope: Envelope = serde_json::from_str(&text)?;
    let (token, display_name, tz) = match envelope {
        Envelope::Hello { token, display_name, tz, .. } => (token, display_name, tz),
        _ => return Err(anyhow::anyhow!("expected Hello frame")),
    };
    // Untrusted client input that later lands in a prompt: keep it only
    // when it is a real IANA zone name, spelled the canonical way.
    let peer_tz: Option<String> = tz
        .trim()
        .parse::<chrono_tz::Tz>()
        .ok()
        .map(|z| z.name().to_string());

    let host_url = {
        let stats = s.app.stats.read();
        stats
            .host_url
            .clone()
            .unwrap_or_else(|| invite::public_host_url(&s.app.config.read()))
    };

    let claims = match invite::validate_jwt_for_host(&s.app.db.pool, &token, &host_url).await {
        Ok(c) => c,
        Err(e) => {
            let _ = sink
                .send(WsMessage::Text(
                    serde_json::to_string(&Envelope::Error {
                        message: format!("invite rejected: {e}"),
                    })?
                    .into(),
                ))
                .await;
            return Err(e);
        }
    };

    // An automation key may call the HTTP reminder API and nothing else.
    // This check has to be HERE, before anything downstream treats the
    // token as a member: a few lines below, `claims.sub` starts evicting
    // live peers, then goes into `net.peers` and the `peers` table, and
    // by dispatch it is `context_peer` — the identity every thread,
    // message and user-fact query is scoped to. A token carrying
    // `act_as: "host"` that reached that far would read the host's own
    // conversations.
    if !claims.is_family() {
        let _ = sink
            .send(WsMessage::Text(
                serde_json::to_string(&Envelope::Error {
                    message: "this key is for the reminders API, not for chat".into(),
                })?
                .into(),
            ))
            .await;
        tracing::warn!(scope = %claims.scope, "refused a non-family token at the handshake");
        return Err(anyhow::anyhow!("non-family token refused at handshake"));
    }

    // A paused device stays out until the host resumes it. This check is
    // what makes Pause mean anything: clients auto-retry within seconds,
    // so cutting the socket alone would be undone by the paused device
    // itself before the host had let go of the mouse.
    if s.app.db.peer_is_paused(&claims.sub).await.unwrap_or(false) {
        let _ = sink
            .send(WsMessage::Text(
                serde_json::to_string(&Envelope::Error {
                    message: "this device is paused — ask the host to resume it".into(),
                })?
                .into(),
            ))
            .await;
        return Err(anyhow::anyhow!("paused device refused at handshake"));
    }

    let peer_id = uuid::Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::unbounded_channel::<Envelope>();
    // Cancelled by Pause and by Disconnect; the read loop selects on it.
    let session_cancel = CancellationToken::new();

    {
        let mut net = s.app.net.lock().await;
        // Evict any stale connections from the same invite before inserting
        // the new one. Without this step every client reconnect (auto-retry,
        // network blip, host restart, manual "Reconnect now") leaves a
        // ghost entry until the old TCP socket times out — which on macOS
        // can be several minutes. The Manage Family page then shows the
        // same device twice. Keying eviction on `invite_id` (the JWT's
        // `sub` = invite short-code) means "one invite, one live device" —
        // matching our recommended best practice of one invite per device.
        let stale_ids: Vec<String> = net
            .peers
            .iter()
            .filter(|(_, info)| info.invite_id == claims.sub)
            .map(|(id, _)| id.clone())
            .collect();
        for stale in &stale_ids {
            if let Some(info) = net.peers.remove(stale) {
                // Best-effort polite goodbye to the stale WS so its read
                // loop exits cleanly instead of waiting on TCP timeout.
                let _ = info.tx.send(Envelope::Error {
                    message: "replaced by a newer connection from the same invite".into(),
                });
            }
        }
        // Notify any UI listening to peer-left for the evicted entries.
        // Drop the lock before emitting so listeners that call back into
        // shared state don't deadlock.
        drop(net);
        for stale in &stale_ids {
            let _ = s
                .tauri
                .emit("kinai://peer-left", serde_json::json!({"id": stale}));
        }

        let mut net = s.app.net.lock().await;
        net.peers.insert(
            peer_id.clone(),
            PeerInfo {
                display_name: display_name.clone(),
                invite_id: claims.sub.clone(),
                tx: tx.clone(),
                first_seen: chrono::Utc::now(),
                last_seen: chrono::Utc::now(),
                cancel: session_cancel.clone(),
            },
        );
        s.app.stats.write().peers_connected = net.peers.len();
    }
    // Remember the device's zone (keyed by the storage peer id, the same
    // value threads/facts/reminders use) — after the net lock is gone.
    if let Err(e) = s
        .app
        .db
        .upsert_peer_on_connect(&claims.sub, &display_name, peer_tz.as_deref())
        .await
    {
        tracing::warn!("peer upsert failed: {e:#}");
    }
    let _ = s.tauri.emit(
        "kinai://peer-joined",
        serde_json::json!({"id": peer_id, "name": display_name}),
    );

    let (family_name, host_model, host_search_engine, host_vision, host_telegram_bot, host_slots) = {
        let cfg = s.app.config.read();
        let vision_label = if crate::vision::is_vision_capable(&cfg.llm.model) {
            // Chat model can do vision on its own — no dedicated endpoint needed.
            format!("{} (chat model)", cfg.llm.model)
        } else if cfg.vision.enabled && !cfg.vision.primary.base_url.is_empty() {
            let label = if cfg.vision.primary.label.is_empty() {
                cfg.vision.primary.model.clone()
            } else {
                cfg.vision.primary.label.clone()
            };
            if !cfg.vision.failover.base_url.is_empty() {
                format!("{} (with failover)", label)
            } else {
                label
            }
        } else {
            "off".into()
        };
        // bot_username is empty until the host has run a successful
        // getMe — exposing it lets the client peer's Settings card show
        // "Family bot: @foo" + enable its Connect button. If the host
        // hasn't configured a bot yet, the empty string tells clients
        // to display the "ask the family owner to set up Telegram" hint
        // instead of the QR flow.
        (
            cfg.host.family_name.clone(),
            cfg.llm.model.clone(),
            format!("{:?}", cfg.tools.search_engine).to_lowercase(),
            vision_label,
            cfg.telegram.bot_username.clone(),
            // Same guard as the fields above — one atomic config
            // snapshot per Welcome, so a concurrent Settings save can't
            // produce a half-old / half-new advertisement.
            crate::slash::active_slot_wires(&cfg),
        )
    };
    // Stamp connect-time liveness onto the advertised slots (parallel,
    // 1.5s-bounded probes behind a 15s cache — adds ~1.5s to the
    // handshake at most once per TTL, ~0ms on a warm cache).
    let mut host_slots = host_slots;
    let probes = futures_util::future::join_all(
        host_slots
            .iter()
            .map(|w| crate::slash::slot_alive_cached(&s.app, &w.slug)),
    )
    .await;
    for (w, alive) in host_slots.iter_mut().zip(probes) {
        w.alive = Some(alive);
    }
    let host_fact_check = crate::factcheck::is_configured(&s.app.config.read().llm_factcheck);
    let _ = tx.send(Envelope::Welcome {
        family_name,
        host_version: env!("CARGO_PKG_VERSION").into(),
        host_model,
        host_search_engine,
        host_vision,
        host_telegram_bot,
        host_slots,
        host_fact_check,
        host_reports: true,
        host_thread_ops: true,
        host_reminders: true,
    });

    let writer = tokio::spawn(async move {
        while let Some(env) = rx.recv().await {
            if let Ok(text) = serde_json::to_string(&env) {
                if sink.send(WsMessage::Text(text.into())).await.is_err() {
                    break;
                }
            }
        }
    });

    // Pause and Disconnect both cancel this token. Without the select the
    // loop parks on `source.next()` until the client happens to send
    // something, so a removed member kept chatting on an open socket.
    loop {
        let frame = tokio::select! {
            biased;
            _ = session_cancel.cancelled() => {
                tracing::info!(peer = %peer_id, "session ended by the host");
                break;
            }
            next = source.next() => match next {
                Some(f) => f,
                None => break,
            },
        };
        let Ok(frame) = frame else { break };
        if matches!(frame, WsMessage::Close(_)) {
            break;
        }
        let Some(text) = frame_text(&frame) else { continue };
        let env: Envelope = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(Envelope::Error {
                    message: format!("bad envelope: {e}"),
                });
                continue;
            }
        };
        // Keyed on `claims.sub` (the invite, stable across reconnects) —
        // NOT the per-connection UUID. With the session key a peer got a
        // fresh full bucket by reconnecting, which made the limit
        // decorative for anything a client can trigger in a loop.
        if matches!(env, Envelope::SendMessage { .. } | Envelope::FactCheckRequest { .. }
            | Envelope::ReportAnswer { .. } | Envelope::DeleteThread { .. }
            | Envelope::RenameThread { .. } | Envelope::ReminderAction { .. })
            && !s.rate.allow(&claims.sub) {
            // Same for a reminder action: the popup waits for this ack.
            if let Envelope::ReminderAction { id, .. } = &env {
                let _ = tx.send(Envelope::ReminderActionAck {
                    id: id.clone(),
                    ok: false,
                    message: "Too many requests just now — wait a moment and try again.".into(),
                    reminder: None,
                });
            }
            // A rejected REPORT still needs its ack, or the client's
            // round-trip waits out the full timeout and then blames a
            // timeout for what was really a rate limit.
            if let Envelope::ReportAnswer { message_id, .. } = &env {
                let _ = tx.send(Envelope::ReportAck {
                    message_id: message_id.clone(),
                    ok: false,
                    message: "Too many requests just now — wait a moment and try again.".into(),
                });
            }
            if let Envelope::DeleteThread { thread_id } | Envelope::RenameThread { thread_id, .. } =
                &env
            {
                let _ = tx.send(Envelope::ThreadOpAck {
                    thread_id: thread_id.clone(),
                    ok: false,
                    message: "Too many requests just now — wait a moment and try again.".into(),
                });
            }
            let _ = tx.send(Envelope::Error {
                message: "rate limit exceeded; slow down a moment".into(),
            });
            continue;
        }
        // `claims.sub` is the invite's short_code — stable across reconnects
        // and used as the storage-level peer_id. `peer_id` is the per-WS
        // session UUID, only used for rate-limiting + peer-list bookkeeping.
        if let Err(e) =
            dispatch(env, &s, &tx, &peer_id, &claims.sub, &display_name, &claims.label).await
        {
            let _ = tx.send(Envelope::Error {
                message: e.to_string(),
            });
        }
    }

    writer.abort();
    {
        let mut net = s.app.net.lock().await;
        net.peers.remove(&peer_id);
        s.app.stats.write().peers_connected = net.peers.len();
    }
    let _ = s.tauri.emit("kinai://peer-left", serde_json::json!({"id": peer_id}));
    Ok(())
}

fn frame_text(f: &WsMessage) -> Option<String> {
    match f {
        WsMessage::Text(t) => Some(t.to_string()),
        WsMessage::Binary(b) => std::str::from_utf8(b).ok().map(String::from),
        _ => None,
    }
}

async fn dispatch(
    env: Envelope,
    s: &AxumState,
    tx: &mpsc::UnboundedSender<Envelope>,
    peer_id: &str,
    context_peer: &str,
    display_name: &str,
    invite_label: &str,
) -> anyhow::Result<()> {
    match env {
        Envelope::Ping => {
            let _ = tx.send(Envelope::Pong);
        }
        Envelope::ListThreads => {
            let threads = s.app.db.list_threads(context_peer).await?;
            let _ = tx.send(Envelope::Threads { threads });
        }
        Envelope::LoadThread { thread_id } => {
            let messages = s
                .app
                .db
                .load_messages(context_peer, &thread_id, 500)
                .await?;
            let _ = tx.send(Envelope::ThreadMessages { thread_id, messages });
        }

        // ---- User facts (persistent memory) ----
        //
        // All four handlers dispatch with `context_peer` so each client
        // can only ever see/edit its own facts. Mutations re-list and
        // ship the fresh full set back as `UserFacts` so the client UI
        // can re-render off a single envelope. Errors land in the WS
        // Error response — the client side surfaces them as a red
        // banner just like the local error path on the host.
        Envelope::ListUserFacts => {
            let facts = s.app.db.list_user_facts(context_peer).await?;
            let _ = tx.send(Envelope::UserFacts { facts });
        }
        Envelope::SaveUserFact { key, value } => {
            // source = "manual" — same as the host's Settings → Memory
            // → Add a fact path. Saves under the CONNECTING client's
            // peer_id, not HOST_PEER, so a family member's manual
            // entries stay theirs.
            s.app
                .db
                .save_user_fact(context_peer, &key, &value, "manual", None)
                .await?;
            let facts = s.app.db.list_user_facts(context_peer).await?;
            let _ = tx.send(Envelope::UserFacts { facts });
        }
        Envelope::DeleteUserFact { id } => {
            s.app.db.delete_user_fact(context_peer, &id).await?;
            let facts = s.app.db.list_user_facts(context_peer).await?;
            let _ = tx.send(Envelope::UserFacts { facts });
        }
        Envelope::ClearUserFacts => {
            let _ = s.app.db.clear_user_facts(context_peer).await?;
            let facts = s.app.db.list_user_facts(context_peer).await?;
            let _ = tx.send(Envelope::UserFacts { facts });
        }
        Envelope::ListReminders => {
            let items = s.app.db.list_reminders(context_peer).await?;
            let _ = tx.send(Envelope::Reminders { items });
        }
        Envelope::ReminderAction {
            id,
            action,
            snooze_minutes,
        } => {
            // Never `?` out of here: the popup on the device is waiting
            // for THIS ack, and a generic Error frame can't be correlated.
            let (ok, message, reminder) = match s
                .app
                .db
                .apply_reminder_action(context_peer, &id, &action, snooze_minutes)
                .await
            {
                Ok(r) => (true, "Reminder updated.".to_string(), r),
                Err(e) => {
                    tracing::warn!(id = %id, "reminder action failed: {e:#}");
                    (
                        false,
                        "That reminder couldn't be updated — it may already be done.".to_string(),
                        None,
                    )
                }
            };
            let _ = tx.send(Envelope::ReminderActionAck {
                id,
                ok,
                message,
                reminder,
            });
        }
        // Response envelope — host shouldn't receive this from a client.
        // Silently ignore (better than panicking) since a buggy/old
        // client could conceivably echo it back.
        Envelope::UserFacts { .. }
        | Envelope::Reminders { .. }
        | Envelope::ReminderActionAck { .. }
        | Envelope::Reminder { .. } => {}
        Envelope::SendMessage {
            thread_id,
            content,
            sender,
            client_msg_id,
            attachments,
        } => {
            let actual_sender = if sender.is_empty() {
                display_name.to_string()
            } else {
                sender
            };
            // Spawn the turn instead of awaiting it inline. The connection's
            // read loop dispatches envelopes sequentially, so awaiting here
            // would block reads until the (possibly runaway) turn finished —
            // and the client's StopGeneration would never be seen in time.
            // Spawning frees the loop to receive StopGeneration mid-turn and
            // cancel the token run_chat_turn registers. Errors are surfaced
            // over the same writer channel from inside the task.
            let s2 = s.clone();
            let tx2 = tx.clone();
            let context_peer2 = context_peer.to_string();
            let peer_id2 = peer_id.to_string();
            tokio::spawn(async move {
                if let Err(e) = run_chat_turn(
                    &s2,
                    &tx2,
                    &context_peer2,
                    &thread_id,
                    &content,
                    &actual_sender,
                    &client_msg_id,
                    &peer_id2,
                    &attachments,
                )
                .await
                {
                    let _ = tx2.send(Envelope::Error {
                        message: e.to_string(),
                    });
                }
            });
        }
        // Client pressed Stop: cancel the matching in-flight turn's token
        // (registered in run_chat_turn). Breaks runaway tool/token loops
        // without the client having to disconnect. No-op if the turn
        // already finished.
        Envelope::StopGeneration { client_msg_id } => {
            if let Some(tok) = s.app.pending_turns.lock().get(&client_msg_id).cloned() {
                tracing::info!("client requested stop for turn {client_msg_id}");
                tok.cancel();
            }
        }
        // ---- Telegram pairing for client peers ----
        //
        // The handshake mirrors the host-mode Tauri commands, but the
        // *peer_id* used for DB writes is `context_peer` (the connecting
        // client's stable invite id) instead of the host's HOST_PEER.
        // Same `telegram_links` table, same `redeem_pair` function —
        // the bot's /start handler doesn't care whether the token was
        // minted for the host or a client peer; it just trusts the
        // peer_id stored on the pending row.
        Envelope::RequestTelegramPair => {
            // Resolve the client's pending oneshot one way or another so
            // the command-side `request_telegram_pair` returns promptly
            // instead of hitting its 15s timeout. Sentinel for "host
            // hasn't set up the bot" is an empty `url` field — the
            // client treats that as an error string.
            let cfg = s.app.config.read().clone();
            let bot_username = cfg.telegram.bot_username.clone();
            if bot_username.is_empty() {
                let _ = tx.send(Envelope::TelegramPair {
                    url: String::new(),
                    expires_in_secs: 0,
                    bot_username: String::new(),
                });
            } else {
                match crate::db::telegram::create_pending_pair(&s.app.db.pool, context_peer)
                    .await
                {
                    Ok(token) => {
                        let _ = tx.send(Envelope::TelegramPair {
                            url: format!("https://t.me/{bot_username}?start={token}"),
                            expires_in_secs: 600,
                            bot_username,
                        });
                    }
                    Err(e) => {
                        // DB failure is rare; send the sentinel + an
                        // Error toast for visibility.
                        let _ = tx.send(Envelope::TelegramPair {
                            url: String::new(),
                            expires_in_secs: 0,
                            bot_username: String::new(),
                        });
                        let _ = tx.send(Envelope::Error {
                            message: format!("Couldn't create Telegram pairing token: {e}"),
                        });
                    }
                }
            }
        }
        Envelope::DeleteThread { thread_id } => {
            // Scoped by context_peer: the SQL deletes only where the row's
            // peer_id matches, so one family member can never delete
            // another's conversation by guessing an id.
            let (ok, message) = match s.app.db.delete_thread(context_peer, &thread_id).await {
                Ok(()) => (true, "Conversation deleted.".to_string()),
                Err(e) => {
                    tracing::warn!("client thread delete failed: {e:#}");
                    (false, "The host couldn't delete that conversation.".to_string())
                }
            };
            let _ = tx.send(Envelope::ThreadOpAck { thread_id, ok, message });
        }
        Envelope::RenameThread { thread_id, title } => {
            let title: String = title.chars().take(200).collect();
            let (ok, message) = match s.app.db.rename_thread(context_peer, &thread_id, &title).await
            {
                Ok(()) => (true, "Conversation renamed.".to_string()),
                Err(e) => {
                    tracing::warn!("client thread rename failed: {e:#}");
                    (false, "The host couldn't rename that conversation.".to_string())
                }
            };
            let _ = tx.send(Envelope::ThreadOpAck { thread_id, ok, message });
        }
        Envelope::ReportAnswer { message_id, question, answer, model, slot } => {
            // Identity comes from the HOST-authored invite label, not the
            // client's self-chosen display name — otherwise a peer could
            // file a report as "Grandma". The display name is appended
            // only as an unauthenticated hint.
            let reporter = {
                let claimed = display_name.to_string();
                match (invite_label.trim(), claimed.trim()) {
                    ("", "") => "Family member".to_string(),
                    ("", c) => format!("{c} (unverified name)"),
                    (l, c) if c.is_empty() || c == l => l.to_string(),
                    (l, c) => format!("{l} (calls themself \"{c}\")"),
                }
            };
            // Truncate before storage: the wire cap bounds one frame, this
            // bounds what a peer can accumulate in the host's database.
            let question: String = question.chars().take(4_000).collect();
            let answer: String = answer.chars().take(20_000).collect();
            let model: String = model.chars().take(200).collect();
            let slot: String = slot.chars().take(40).collect();
            let (ok, msg) = match s
                .app
                .db
                .add_report(
                    context_peer,
                    &reporter,
                    &message_id,
                    &question,
                    &answer,
                    &model,
                    &slot,
                )
                .await
            {
                Ok(_) => {
                    // Wake the host UI: sidebar badge + list refresh.
                    let _ = s.tauri.emit("kinai://report", serde_json::json!({
                        "reporter": reporter,
                    }));
                    // Who reported it is on the report itself, in the
                    // Reported answers page. The log does not need a name.
                    tracing::info!("answer reported");
                    (true, "Thanks — the host can see this answer now.".to_string())
                }
                Err(e) => {
                    tracing::warn!("storing report failed: {e:#}");
                    (false, "Couldn't send the report — please try again.".to_string())
                }
            };
            let _ = tx.send(Envelope::ReportAck { message_id, ok, message: msg });
        }
        Envelope::FactCheckRequest { message_id } => {
            // Spawned — a fact check can take tens of seconds and must not
            // block this peer's WS read loop (chat turns spawn the same
            // way). Ownership check happens inside the peer-scoped DB
            // lookup — a peer can only fact-check its own threads.
            //
            // Guards (the checker slot is a PAID online API):
            //  * draws from the same per-peer rate bucket as chat (above);
            //  * one in-flight check per peer — rapid re-clicks across
            //    bubbles queue behind an immediate "already running" reply;
            //  * 150s hard bound (the client gives up at 120s);
            //  * provider/DB error details stay in the host log — peers
            //    get a generic message, not API billing/auth internals.
            {
                let mut checks = s.app.fact_checks_running.lock();
                if !checks.insert(context_peer.to_string()) {
                    let _ = tx.send(Envelope::FactCheckResult {
                        message_id,
                        ok: false,
                        report: "A fact check is already running — wait for it to finish."
                            .to_string(),
                    });
                    return Ok(());
                }
            }
            let s2 = s.clone();
            let tx2 = tx.clone();
            let peer = context_peer.to_string();
            tokio::spawn(async move {
                let cfg = s2.app.config.read().clone();
                let (ok, report) = if !crate::factcheck::is_configured(&cfg.llm_factcheck) {
                    // Config changed since this peer's Welcome advertised
                    // the feature — tell them something actionable.
                    (
                        false,
                        "The host's fact-check model is no longer configured — ask the family owner to check Settings → Models → Fact-check model."
                            .to_string(),
                    )
                } else {
                    match s2
                        .app
                        .db
                        .question_answer_for_fact_check(&peer, &message_id)
                        .await
                    {
                        Ok(Some((question, answer))) => {
                            let cancel = CancellationToken::new();
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(150),
                                crate::factcheck::run(&cfg, &question, &answer, cancel.clone()),
                            )
                            .await
                            {
                                Ok(Ok(report)) => (true, report),
                                Ok(Err(e)) => {
                                    tracing::warn!("fact check for peer {peer} failed: {e:#}");
                                    (
                                        false,
                                        "Fact check failed — the checker model returned an error. The host can see details in the KinAI log."
                                            .to_string(),
                                    )
                                }
                                Err(_) => {
                                    cancel.cancel();
                                    tracing::warn!("fact check for peer {peer} timed out");
                                    (false, "The fact check timed out — try again.".to_string())
                                }
                            }
                        }
                        Ok(None) => (false, "That message can't be fact-checked.".to_string()),
                        Err(e) => {
                            tracing::warn!("fact check db lookup for peer {peer} failed: {e:#}");
                            (false, "Fact check failed — please try again.".to_string())
                        }
                    }
                };
                s2.app.fact_checks_running.lock().remove(&peer);
                let _ = tx2.send(Envelope::FactCheckResult {
                    message_id,
                    ok,
                    report,
                });
            });
        }
        Envelope::RequestSlotHealth => {
            // Fresh liveness for the client's slash-menu markers, served
            // from the same short-TTL probe cache as everything else.
            let mut slots = crate::slash::active_slot_wires(&s.app.config.read());
            let probes = futures_util::future::join_all(
                slots
                    .iter()
                    .map(|w| crate::slash::slot_alive_cached(&s.app, &w.slug)),
            )
            .await;
            for (w, alive) in slots.iter_mut().zip(probes) {
                w.alive = Some(alive);
            }
            let _ = tx.send(Envelope::SlotHealth { slots });
        }
        Envelope::RequestTelegramStatus => {
            let cfg = s.app.config.read().clone();
            match crate::db::telegram::link_for_peer(&s.app.db.pool, context_peer).await {
                Ok(link) => {
                    let _ = tx.send(Envelope::TelegramStatus {
                        bot_configured: !cfg.telegram.bot_token.trim().is_empty(),
                        bot_username: cfg.telegram.bot_username.clone(),
                        paired: link.is_some(),
                        username: link.as_ref().and_then(|l| l.username.clone()),
                        first_name: link.as_ref().and_then(|l| l.first_name.clone()),
                        paired_at: link.as_ref().map(|l| l.paired_at.clone()),
                    });
                }
                Err(e) => {
                    let _ = tx.send(Envelope::Error {
                        message: format!("Couldn't read Telegram status: {e}"),
                    });
                }
            }
        }
        Envelope::RequestTelegramUnpair => {
            match crate::db::telegram::unpair(&s.app.db.pool, context_peer).await {
                Ok(()) => {
                    let _ = tx.send(Envelope::TelegramUnpairDone);
                }
                Err(e) => {
                    let _ = tx.send(Envelope::Error {
                        message: format!("Couldn't unpair Telegram: {e}"),
                    });
                }
            }
        }
        _ => {}
    }
    Ok(())
}

async fn run_chat_turn(
    s: &AxumState,
    tx: &mpsc::UnboundedSender<Envelope>,
    context_peer: &str,
    thread_id: &str,
    content: &str,
    sender: &str,
    client_msg_id: &str,
    _peer_id: &str,
    attachments: &[crate::db::Attachment],
) -> anyhow::Result<()> {
    // The client's thread row only lives in its local DB. Make sure the
    // host has a matching row, tagged with the connecting peer's id so
    // every member's history lives in its own bucket on disk.
    //
    // Title it from the message, NOT the sender. `create_thread` has no
    // `Mode::Client` branch (unlike list/rename/delete), so the host first
    // hears about a client's thread here — after the client's auto-rename
    // has already no-opped against a row that did not exist yet. Since
    // `upsert_thread` is INSERT OR IGNORE, whatever we write now is
    // permanent: using `sender` gave every family device a sidebar full of
    // the device name (1 of 61 non-Telegram client threads ever got a real title,
    // vs 26 of 26 eligible on the host, where the row already exists when
    // the rename lands). `sender` remains the fallback for empty content
    // (an attachment-only turn), so an admin reading the host DB can still
    // tell those threads apart — and every row carries peer_id regardless.
    let seed_title = crate::db::messages::derive_thread_title(content)
        .unwrap_or_else(|| sender.to_string());
    let _ = s.app.db.upsert_thread(context_peer, thread_id, &seed_title).await;

    // Persist exactly what the user typed. Attachment text is extracted
    // at context-build time (see `context::builder::format_user`) so the
    // chat history isn't littered with dumped PDF bodies.
    let user_msg = s.app.db
        .append_message(thread_id, "user", sender, content, attachments)
        .await?;
    // Echo the persisted user message back ONLY to the sender. The previous
    // implementation broadcast to every connected peer, which would leak
    // one family member's chat into every other member's UI. Same goes for
    // the host's own UI — when a peer chats, that conversation belongs to
    // the peer, not to whoever happens to own the host machine.
    let _ = tx.send(Envelope::Message { message: user_msg.clone() });

    // Bidirectional Telegram sync (client-peer side): show the
    // "typing…" indicator on the peer's Telegram chat while the host
    // LLM thinks. The actual Q&A echo (combined into one bot message)
    // happens after the assistant reply is finalized — see the two
    // `maybe_echo_qa` calls further down. No-op on non-Telegram threads.
    crate::telegram::echo::maybe_show_typing(&s.app, context_peer, thread_id).await;

    let cfg = s.app.config.read().clone();

    // Resolve which LLM slot ("fast" vs "deep") this turn targets and
    // strip the `/fast `/`/deep ` prefix before anything sees it —
    // both the in-message slash handler (/pic, /help) AND the LLM
    // context builder use the stripped content from here on out so
    // the routing token doesn't leak into the model's prompt or the
    // image-gen parser. `route_for` is now async + thread-aware so
    // the slot choice can stick across subsequent plain-text turns
    // until the user switches with /fast or /deep again.
    let route_pick =
        crate::slash::route_for(&s.app.db, &cfg, context_peer, thread_id, content).await;
    let llm_route_content = route_pick.stripped_content.clone();

    // Slash commands are intercepted BEFORE the LLM pipeline. The user
    // message is already persisted above so the chat history shows their
    // exact input ("/pic 1280x720 sunset over Miami"); the synthetic
    // assistant reply below shows the resulting image (or a usage hint /
    // error if generation fails).
    // Bare `/fast` / `/deep` → mode switch confirmation, no LLM turn.
    // Otherwise the normal slash handlers (/pic, /picHQ, /help, ?).
    let slash_reply = if route_pick.bare_switch {
        let alive = crate::slash::slot_alive_cached(&s.app, route_pick.slot_label).await;
        Some(crate::slash::switch_confirmation(&cfg, &route_pick, Some(alive)))
    } else if let Some(arg) = crate::telegram::router::strip_voice(&llm_route_content) {
        // /voice typed in a family member's KinAI app — toggles THEIR
        // Telegram voice-note opt-in (context_peer scoping), same as
        // sending /voice to the bot directly. Intercepted before the
        // LLM so the model can't roleplay a fake confirmation.
        Some(crate::telegram::router::voice_command_outcome(&s.app, context_peer, arg).await.reply)
    } else {
        crate::slash::handle(&cfg, &llm_route_content).await
    };
    if let Some(reply) = slash_reply {
        let started_at = std::time::Instant::now();
        let mut assistant_msg = s
            .app
            .db
            .append_message(thread_id, "assistant", "KinAI", &reply, &[])
            .await?;
        let total_ms = started_at.elapsed().as_millis() as u64;
        // Slash commands skip the LLM — leave model/slot empty so the
        // UI's model badge stays hidden for these turns.
        let metrics = crate::network::protocol::TurnMetricsWire {
            first_token_ms: 0,
            total_ms,
            output_tokens: 0,
            tps: 0.0,
            model: String::new(),
            slot: String::new(),
            question_msg_id: None,
        };
        let metrics_json = serde_json::to_value(&metrics).unwrap_or(serde_json::Value::Null);
        let _ = s.app.db.set_message_metrics(&assistant_msg.id, &metrics_json).await;
        assistant_msg.metrics = Some(metrics_json);
        let _ = tx.send(Envelope::AssistantDone {
            client_msg_id: client_msg_id.to_string(),
            message: assistant_msg.clone(),
            metrics,
        });
        // Mirror the full Q&A turn (user question + slash-command
        // reply) to Telegram as one combined bot message. No-op when
        // this isn't a Telegram thread or the peer hasn't paired.
        crate::telegram::echo::maybe_echo_qa(
            &s.app,
            context_peer,
            thread_id,
            sender,
            content,
            &assistant_msg.content,
        )
        .await;
        return Ok(());
    }

    // Build the LLM context from a copy of user_msg where the
    // model-routing prefix has been removed — without this strip the
    // model sees its own routing token in the prompt ("/fast hello"
    // instead of just "hello"). The persisted DB row still carries
    // the original prefix so chat history shows the user's exact
    // input, including the slash.
    let user_msg_for_llm = crate::db::Message {
        content: llm_route_content.clone(),
        ..user_msg.clone()
    };
    let messages =
        context::builder::build_context(&s.app.db, &cfg, route_pick.settings, context_peer, thread_id, &user_msg_for_llm)
            .await?;
    // Snapshot the prompt for the per-turn diagnostic panel. Replace
    // inline image data URLs with a tiny placeholder so a single
    // attached PNG doesn't bloat the JSON to 7-8 MB.
    let prompt_debug = serde_json::to_string_pretty(
        &messages
            .iter()
            .map(|m| m.redacted_for_debug())
            .collect::<Vec<_>>(),
    )
    .ok();
    let tools = registry::enabled(&cfg.tools);
    let tool_runtime = registry::ToolRuntime::from_tool_settings(&cfg.tools)
        .with_memory(s.app.db.clone(), context_peer)
        .with_source_msg(client_msg_id.to_string())
        .with_thread(thread_id.to_string());
    // Route from the active slot's settings (fast or deep), NOT the
    // cached `state.llm` — that one always holds the fast slot. The
    // LLM client itself is built per attempt inside
    // run_turn_with_slot_failover.
    let active_llm_settings = route_pick.settings.clone();
    let cancel = CancellationToken::new();

    // Register THIS turn's cancel token keyed by the client's msg id so an
    // inbound `Envelope::StopGeneration { client_msg_id }` (the client's
    // Stop button) can abort it — including a runaway tool/token loop. The
    // RAII guard removes it on every exit path (success, `?`-error, panic)
    // so a later turn reusing the same id can't be cancelled by accident.
    s.app
        .pending_turns
        .lock()
        .insert(client_msg_id.to_string(), cancel.clone());
    struct TurnGuard {
        id: String,
        pending: std::sync::Arc<
            parking_lot::Mutex<std::collections::HashMap<String, CancellationToken>>,
        >,
    }
    impl Drop for TurnGuard {
        fn drop(&mut self) {
            self.pending.lock().remove(&self.id);
        }
    }
    let _turn_guard = TurnGuard {
        id: client_msg_id.to_string(),
        pending: s.app.pending_turns.clone(),
    };

    let tx_token = tx.clone();
    let client_msg_id_token = client_msg_id.to_string();
    let tx_reasoning = tx.clone();
    let client_msg_id_reasoning = client_msg_id.to_string();
    let tx_tool = tx.clone();
    let client_msg_id_tool = client_msg_id.to_string();

    let started_at = std::time::Instant::now();
    let first_token_seen = Arc::new(parking_lot::Mutex::new(None::<u64>));
    let first_token_clone = first_token_seen.clone();

    let handlers = PipelineHandlers {
        on_token: Arc::new(move |t| {
            if first_token_clone.lock().is_none() {
                *first_token_clone.lock() = Some(started_at.elapsed().as_millis() as u64);
            }
            let _ = tx_token.send(Envelope::Token {
                client_msg_id: client_msg_id_token.clone(),
                delta: t,
            });
        }),
        on_reasoning: Arc::new(move |r| {
            let _ = tx_reasoning.send(Envelope::Reasoning {
                client_msg_id: client_msg_id_reasoning.clone(),
                delta: r,
            });
        }),
        on_tool: Arc::new(move |event: ToolEvent| {
            let _ = tx_tool.send(Envelope::Tool {
                client_msg_id: client_msg_id_tool.clone(),
                event,
            });
        }),
    };

    // Route based on attachments + model capability. The vast majority of
    // turns are plain chat → Route::Chat → same code path as before.
    // Image turns on a non-vision chat model route to the configured
    // vision endpoint (with optional failover). We feed the
    // model-routing slot's settings (fast vs deep) through both the
    // vision decision and the pipeline so the chosen slot's
    // vision-capability profile and context window apply.
    let route = crate::vision::decide(
        &active_llm_settings,
        attachments,
        &cfg.vision,
        crate::vision::history_has_image(&messages),
    )
    .await?;
    // Runtime copy for post-turn image recovery (run_with_route consumes it).
    let recover_runtime = tool_runtime.clone();
    let served = crate::slash::run_turn_with_slot_failover(
        route,
        &s.app,
        &cfg,
        route_pick.slot_label,
        messages,
        tools,
        tool_runtime,
        handlers,
        cancel,
        |s, msgs| compute_max_tokens(s, msgs),
    )
    .await?;
    let mut result = served.result;
    // Parity with the host-app (commands.rs) and Telegram paths — family
    // CLIENTS hit this WS path, and it was missed when both fixes landed:
    //  * recover fabricated image URLs (small models invent them instead of
    //    calling image_search);
    //  * never store/emit an EMPTY completion — a reasoning model (the deep
    //    slot) can spend its whole token budget thinking and produce no
    //    visible answer, which otherwise lands on the client as a blank
    //    bubble ("it does not produce an outcome").
    result.final_content =
        crate::tools::image_recover::recover_reply_images(&result.final_content, &recover_runtime)
            .await;
    if result.final_content.trim().is_empty() {
        result.final_content = crate::tools::loop_pipeline::EMPTY_REPLY_NOTE.to_string();
    }
    let total_ms = started_at.elapsed().as_millis() as u64;
    let mut assistant_msg = s
        .app
        .db
        .append_message(thread_id, "assistant", "KinAI", &result.final_content, &[])
        .await?;

    let first_token_ms = first_token_seen.lock().unwrap_or(0);
    let output_tokens =
        crate::context::token_guard::count_tokens(&result.final_content) as u64;
    let gen_ms = total_ms.saturating_sub(first_token_ms);
    let tps = if gen_ms < 200 || output_tokens == 0 {
        0.0
    } else {
        (output_tokens as f64) * 1000.0 / (gen_ms as f64)
    };
    s.app.stats.write().last_first_token_ms = Some(first_token_ms);

    let metrics = crate::network::protocol::TurnMetricsWire {
        first_token_ms,
        total_ms,
        output_tokens,
        tps,
        // The slot/model that ACTUALLY answered (differs from route_pick
        // after a failover).
        model: served.settings.model.clone(),
        slot: served.slot_label.clone(),
        question_msg_id: Some(user_msg.id.clone()),
    };
    let metrics_json = serde_json::to_value(&metrics).unwrap_or(serde_json::Value::Null);
    let _ = s.app.db.set_message_metrics(&assistant_msg.id, &metrics_json).await;
    assistant_msg.metrics = Some(metrics_json);

    // Send the prompt snapshot first so the UI has it on hand when
    // AssistantDone arrives and the 🔍 toggle becomes interactable.
    if let Some(p) = prompt_debug {
        let _ = tx.send(Envelope::PromptDebug {
            assistant_msg_id: assistant_msg.id.clone(),
            prompt: p,
        });
    }
    // AssistantDone goes only to the originating peer — same privacy rule
    // as the user echo above.
    let _ = tx.send(Envelope::AssistantDone {
        client_msg_id: client_msg_id.to_string(),
        message: assistant_msg.clone(),
        metrics,
    });

    // Bidirectional Telegram sync: mirror the full Q&A turn as one
    // combined bot message on the peer's Telegram chat. No-op on
    // non-Telegram threads; the echo helper also drops the question
    // quote when sender == "Telegram" so we don't bounce inbound.
    crate::telegram::echo::maybe_echo_qa(
        &s.app,
        context_peer,
        thread_id,
        sender,
        content,
        &assistant_msg.content,
    )
    .await;

    if let Err(e) =
        context::memory::maybe_summarize(&s.app.db, context_peer, thread_id).await
    {
        tracing::warn!("summarizer: {e:?}");
    }
    Ok(())
}

pub async fn stop(state: SharedState) -> Result<()> {
    let mut net = state.net.lock().await;
    if let Some(task) = net.server.take() {
        task.abort();
    }
    net.peers.clear();
    state.stats.write().peers_connected = 0;
    state.stats.write().host_url = None;
    let _ = state
        .handle
        .read()
        .as_ref()
        .map(|h| h.emit("kinai://host-status", serde_json::json!({"running": false})));
    Ok(())
}

/// Everyone who can reach this household, connected or not.
///
/// This used to list only `net.peers` — the sockets open at that instant —
/// which meant a member who had not opened their laptop simply was not
/// there. A credential issued months ago and never revoked was invisible
/// on the one page you would go to to look for it, and Disconnect could
/// not reach an offline device because there was no row to click. The
/// source of truth is the invites table; a live socket is decoration on
/// top of it.
///
/// Revoked invites are excluded: they cannot get in, so they are not
/// people who can reach the household. They remain on the Invites page.
pub async fn list_peers(state: &SharedState) -> Vec<PeerSummary> {
    let live: std::collections::HashMap<String, (String, String)> = {
        let net = state.net.lock().await;
        net.peers
            .values()
            .map(|i| {
                (
                    i.invite_id.clone(),
                    (i.display_name.clone(), i.last_seen.to_rfc3339()),
                )
            })
            .collect()
    };

    let rows = sqlx::query(
        "SELECT i.short_code    AS invite_id,
                i.label         AS label,
                p.display_name  AS device_name,
                p.first_seen    AS first_seen,
                p.last_seen     AS last_seen,
                COALESCE(p.paused, 0) AS paused
         FROM invites i
         LEFT JOIN peers p ON p.id = i.short_code
         WHERE i.revoked = 0
         ORDER BY i.created_at",
    )
    .fetch_all(&state.db.pool)
    .await
    .unwrap_or_default();

    rows.into_iter()
        .map(|r| {
            use sqlx::Row as _;
            let invite_id: String = r.get("invite_id");
            let label: String = r.get("label");
            let device_name: Option<String> = r.try_get("device_name").ok().flatten();
            let first_seen: Option<String> = r.try_get("first_seen").ok().flatten();
            let stored_last: Option<String> = r.try_get("last_seen").ok().flatten();
            let paused: i64 = r.try_get("paused").unwrap_or(0);

            let connected = live.get(&invite_id);
            let state_str = if connected.is_some() {
                "connected"
            } else if paused != 0 {
                "paused"
            } else {
                "offline"
            };
            PeerSummary {
                display_name: connected
                    .map(|(n, _)| n.clone())
                    .or(device_name)
                    .unwrap_or_else(|| label.clone()),
                label,
                state: state_str.to_string(),
                first_seen,
                last_seen: connected.map(|(_, s)| s.clone()).or(stored_last),
                invite_id,
            }
        })
        .collect()
}

/// One row on Manage family: a person who can reach this household.
///
/// Note there is no per-connection id here any more. This list is keyed on
/// the invite, because the question the page answers is "who can get in",
/// not "who happens to have a socket open this second".
#[derive(Serialize)]
pub struct PeerSummary {
    /// The invite short code. Stable across reconnects, and what
    /// pause/resume/disconnect act on. Deliberately NOT rendered by the
    /// UI: it is a working credential, and a screenshot of this page used
    /// to publish one per row.
    pub invite_id: String,
    /// The device's own name when it has ever said Hello, otherwise the
    /// label the invite was created with.
    pub display_name: String,
    /// The invite's label, always — "Quentin", "For Mom's iPad".
    pub label: String,
    /// `connected`, `paused`, or `offline`.
    pub state: String,
    /// When this device first connected, if it ever has.
    pub first_seen: Option<String>,
    /// When it was last seen. `None` for an invite nobody has used, and
    /// also for devices that last connected before 0.2.120, which is when
    /// the `peers` table got its first writer.
    pub last_seen: Option<String>,
}

/// End every live session belonging to one invite, for real.
///
/// Removing the map entry was all this used to do, and it disconnected
/// nobody: `PeerInfo.tx` is a clone, the connection's own task keeps the
/// original, and the read loop never re-checked membership. A member who
/// had just been told their access was revoked carried on chatting on the
/// same open socket. Cancelling the session token is what ends it.
///
/// Keyed on the INVITE, not the per-connection id: that is what survives a
/// reconnect, and "one invite, one live device" means it is the identity
/// the host actually means when they point at a row.
async fn end_sessions_for_invite(state: &SharedState, invite_id: &str, why: &str) {
    let ended: Vec<String> = {
        let mut net = state.net.lock().await;
        let ids: Vec<String> = net
            .peers
            .iter()
            .filter(|(_, info)| info.invite_id == invite_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            if let Some(info) = net.peers.remove(id) {
                // Tell them why, then actually cut the session. The send is
                // best-effort: the writer may already be gone.
                let _ = info.tx.send(Envelope::Error { message: why.into() });
                info.cancel.cancel();
            }
        }
        state.stats.write().peers_connected = net.peers.len();
        ids
    };
    if let Some(h) = state.handle.read().as_ref() {
        for id in &ended {
            let _ = h.emit("kinai://peer-left", serde_json::json!({ "id": id }));
        }
    }
    tracing::info!(sessions = ended.len(), "ended sessions for an invite");
}

/// Pause: keep this device out until the host lets it back in. The invite
/// survives, so resuming needs no new code.
///
/// The pause is stored, not just applied to the socket — clients auto-retry
/// within seconds, so a pause that lived only in memory would be undone by
/// the paused device itself almost immediately.
pub async fn pause_peer(state: &SharedState, invite_id: &str) -> Result<()> {
    state.db.set_peer_paused(invite_id, true).await?;
    end_sessions_for_invite(state, invite_id, "paused by the host").await;
    Ok(())
}

/// Let a paused device back in. It reconnects on its own.
pub async fn resume_peer(state: &SharedState, invite_id: &str) -> Result<()> {
    state.db.set_peer_paused(invite_id, false).await
}

/// Disconnect: end the session AND revoke the code. Coming back needs a
/// brand-new invite. Irreversible — there is no un-revoke.
pub async fn disconnect_peer(state: &SharedState, invite_id: &str) -> Result<()> {
    // Revoke FIRST. If it fails we have not told anyone they were removed,
    // and the caller gets a real error instead of a half-done removal.
    invite::revoke_by_short_code(&state.db.pool, invite_id).await?;
    end_sessions_for_invite(state, invite_id, "your access has been revoked").await;
    Ok(())
}

pub async fn return_unauthorized() -> impl IntoResponse {
    StatusCode::UNAUTHORIZED
}

// Shared with every other chat surface — crate::context::builder.
use crate::context::builder::compute_max_tokens;
