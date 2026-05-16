//! Host (Axum HTTP+WS server) and Client (WS dialer).

pub mod client;
pub mod invite;
pub mod pics;
pub mod protocol;
pub mod ratelimit;
pub mod server;
pub mod updates;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

use protocol::Envelope;

pub struct NetState {
    pub peers: HashMap<String, PeerInfo>,
    pub server: Option<JoinHandle<()>>,
    pub client: Option<JoinHandle<()>>,
    /// Writer-side channel of the active client WebSocket. While a client is
    /// connected to a host, Tauri commands push outbound envelopes (a chat
    /// `SendMessage`, a `ListThreads` request, etc.) onto this and the client
    /// task drains it into the live socket. `None` whenever the client is
    /// not connected.
    pub client_tx: Option<mpsc::UnboundedSender<Envelope>>,
    /// Wakes the reconnect-backoff sleeper inside the client supervisor.
    /// Used by the "Reconnect now" UI button to skip the wait between
    /// retry attempts — and by `connect_client` when a fresh code is
    /// redeemed, so the new credentials are tried immediately rather
    /// than after the current backoff expires.
    pub client_wake: Arc<Notify>,
}

impl Default for NetState {
    fn default() -> Self {
        Self {
            peers: HashMap::new(),
            server: None,
            client: None,
            client_tx: None,
            client_wake: Arc::new(Notify::new()),
        }
    }
}

pub struct PeerInfo {
    pub display_name: String,
    pub invite_id: String,
    pub tx: mpsc::UnboundedSender<Envelope>,
    pub first_seen: chrono::DateTime<chrono::Utc>,
    pub last_seen: chrono::DateTime<chrono::Utc>,
}
