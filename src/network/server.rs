//! Host-side Axum HTTP+WebSocket server.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{any, get};
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

#[derive(Clone)]
pub(crate) struct AxumState {
    pub(crate) app: SharedState,
    pub(crate) tauri: AppHandle,
    pub(crate) rate: Arc<RateLimiter>,
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

    axum::serve(listener, router).await?;
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
    peers: usize,
}

async fn info(State(s): State<AxumState>) -> Json<InfoResp> {
    let cfg = s.app.config.read().clone();
    let peers = s.app.net.lock().await.peers.len();
    Json(InfoResp {
        family_name: cfg.host.family_name,
        host_version: env!("CARGO_PKG_VERSION"),
        model: cfg.llm.model,
        peers,
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
async fn redeem_invite(
    State(s): State<AxumState>,
    Query(q): Query<RedeemQuery>,
) -> Result<Json<RedeemResp>, (StatusCode, String)> {
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
        Err(e) => Err((StatusCode::NOT_FOUND, e.to_string())),
    }
}

async fn ws_upgrade(
    State(s): State<AxumState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(s, socket))
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
    let (token, display_name) = match envelope {
        Envelope::Hello { token, display_name, .. } => (token, display_name),
        _ => return Err(anyhow::anyhow!("expected Hello frame")),
    };

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

    let peer_id = uuid::Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::unbounded_channel::<Envelope>();

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
            },
        );
        s.app.stats.write().peers_connected = net.peers.len();
    }
    let _ = s.tauri.emit(
        "kinai://peer-joined",
        serde_json::json!({"id": peer_id, "name": display_name}),
    );

    let (family_name, host_model, host_search_engine, host_vision) = {
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
        (
            cfg.host.family_name.clone(),
            cfg.llm.model.clone(),
            format!("{:?}", cfg.tools.search_engine).to_lowercase(),
            vision_label,
        )
    };
    let _ = tx.send(Envelope::Welcome {
        family_name,
        host_version: env!("CARGO_PKG_VERSION").into(),
        host_model,
        host_search_engine,
        host_vision,
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

    while let Some(frame) = source.next().await {
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
        if matches!(env, Envelope::SendMessage { .. }) && !s.rate.allow(&peer_id) {
            let _ = tx.send(Envelope::Error {
                message: "rate limit exceeded; slow down a moment".into(),
            });
            continue;
        }
        // `claims.sub` is the invite's short_code — stable across reconnects
        // and used as the storage-level peer_id. `peer_id` is the per-WS
        // session UUID, only used for rate-limiting + peer-list bookkeeping.
        if let Err(e) =
            dispatch(env, &s, &tx, &peer_id, &claims.sub, &display_name).await
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
            run_chat_turn(
                s,
                tx,
                context_peer,
                &thread_id,
                &content,
                &actual_sender,
                &client_msg_id,
                peer_id,
                &attachments,
            )
            .await?;
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
    // every member's history lives in its own bucket on disk. Title falls
    // back to the peer's display name so any admin reviewing the host DB
    // can still tell threads apart.
    let _ = s.app.db.upsert_thread(context_peer, thread_id, sender).await;

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

    let cfg = s.app.config.read().clone();
    let messages =
        context::builder::build_context(&s.app.db, &cfg, context_peer, thread_id, &user_msg)
            .await?;
    let tools = registry::enabled(&cfg.tools);
    let tool_runtime = registry::ToolRuntime::from_tool_settings(&cfg.tools);
    let max_tokens = compute_max_tokens(&cfg, &messages);
    let llm = s.app.llm.lock().await.clone();
    let cancel = CancellationToken::new();

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
    // vision endpoint (with optional failover).
    let route = crate::vision::decide(&cfg.llm.model, attachments, &cfg.vision)?;
    let result = crate::vision::run_with_route(
        route,
        llm,
        &cfg.llm,
        messages,
        tools,
        tool_runtime,
        max_tokens,
        handlers,
        cancel,
    )
    .await?;
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
    };
    let metrics_json = serde_json::to_value(&metrics).unwrap_or(serde_json::Value::Null);
    let _ = s.app.db.set_message_metrics(&assistant_msg.id, &metrics_json).await;
    assistant_msg.metrics = Some(metrics_json);

    // AssistantDone goes only to the originating peer — same privacy rule
    // as the user echo above.
    let _ = tx.send(Envelope::AssistantDone {
        client_msg_id: client_msg_id.to_string(),
        message: assistant_msg.clone(),
        metrics,
    });

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

pub async fn list_peers(state: &SharedState) -> Vec<PeerSummary> {
    let net = state.net.lock().await;
    net.peers
        .iter()
        .map(|(id, info)| PeerSummary {
            id: id.clone(),
            display_name: info.display_name.clone(),
            invite_id: info.invite_id.clone(),
            first_seen: info.first_seen.to_rfc3339(),
            last_seen: info.last_seen.to_rfc3339(),
        })
        .collect()
}

#[derive(Serialize)]
pub struct PeerSummary {
    pub id: String,
    pub display_name: String,
    pub invite_id: String,
    pub first_seen: String,
    pub last_seen: String,
}

pub async fn revoke_peer(state: &SharedState, peer_id: &str) -> Result<()> {
    let mut net = state.net.lock().await;
    if let Some(info) = net.peers.remove(peer_id) {
        let _ = info.tx.send(Envelope::Error {
            message: "your access has been revoked".into(),
        });
        drop(info);
    }
    state.stats.write().peers_connected = net.peers.len();
    if let Some(h) = state.handle.read().as_ref() {
        let _ = h.emit("kinai://peer-left", serde_json::json!({"id": peer_id}));
    }
    Ok(())
}

pub async fn return_unauthorized() -> impl IntoResponse {
    StatusCode::UNAUTHORIZED
}

fn compute_max_tokens(cfg: &AppConfig, messages: &[crate::context::ChatMessage]) -> Option<usize> {
    const SAFETY: usize = 128;
    let prompt = crate::context::token_guard::estimate_messages(messages);
    let budget = cfg
        .llm
        .context_window
        .saturating_sub(prompt + SAFETY)
        .max(256);
    Some(if cfg.llm.max_tokens == 0 {
        budget
    } else {
        cfg.llm.max_tokens.min(budget)
    })
}
