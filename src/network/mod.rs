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

use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;

use protocol::Envelope;

/// Async-response payloads for the three Telegram WS round-trips. The
/// command-side handlers (in `src/commands.rs`) park a oneshot here,
/// send the matching `Request*` envelope, then await — the WS read loop
/// completes the oneshot when the host's response envelope arrives.
#[derive(Debug, Clone)]
pub struct TelegramPairWire {
    pub url: String,
    pub expires_in_secs: i64,
    pub bot_username: String,
}

#[derive(Debug, Clone)]
pub struct TelegramStatusWire {
    pub bot_configured: bool,
    pub bot_username: String,
    pub paired: bool,
    pub username: Option<String>,
    pub first_name: Option<String>,
    pub paired_at: Option<String>,
}

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
    /// Pending Telegram WS round-trips. A single in-flight request per
    /// envelope kind is enough — the Settings UI only fires one at a
    /// time, and a stale sender (from a previous reconnect) just resolves
    /// then gets ignored by the new caller. If we ever expose this from
    /// somewhere with concurrent callers, swap to a request-id keyed map.
    pub telegram_pair_pending: Option<oneshot::Sender<TelegramPairWire>>,
    pub telegram_status_pending: Option<oneshot::Sender<TelegramStatusWire>>,
    pub telegram_unpair_pending: Option<oneshot::Sender<()>>,
    /// Pending thread-list round-trip. Client's `list_threads` Tauri
    /// command sends `Envelope::ListThreads` and parks a oneshot here;
    /// the WS read loop completes it when `Envelope::Threads` arrives.
    /// Critical so client peers can see threads that exist on the host
    /// but never landed in their LOCAL DB (Telegram-originated ones).
    pub threads_pending: Option<oneshot::Sender<Vec<crate::db::ThreadMeta>>>,
    /// Pending load-thread round-trip. Same shape — `LoadThread` →
    /// `ThreadMessages`. Lets clients page in message history that lives
    /// in the host's DB but not the client's own.
    pub thread_messages_pending: Option<oneshot::Sender<Vec<crate::db::Message>>>,
    /// Pending user-facts round-trip. All four mutations
    /// (List / Save / Delete / Clear) park a sender here; the host
    /// responds with the fresh `UserFacts` list which the read loop
    /// pipes back. One slot is enough because the Settings → Memory
    /// UI only ever has one in-flight request at a time.
    pub user_facts_pending: Option<oneshot::Sender<Vec<crate::db::UserFact>>>,
    /// Pending slot-health round-trip: `RequestSlotHealth` → `SlotHealth`.
    /// Refreshes the client slash menu's offline markers on demand (the
    /// Welcome snapshot goes stale the moment a server restarts).
    pub slot_health_pending: Option<oneshot::Sender<Vec<protocol::HostSlotWire>>>,
    /// Pending fact-check round-trip: `FactCheckRequest` → `FactCheckResult`
    /// as (ok, report). Single-flight — the button disables while running.
    pub fact_check_pending: Option<oneshot::Sender<(bool, String)>>,
}

impl Default for NetState {
    fn default() -> Self {
        Self {
            peers: HashMap::new(),
            server: None,
            client: None,
            client_tx: None,
            client_wake: Arc::new(Notify::new()),
            telegram_pair_pending: None,
            telegram_status_pending: None,
            telegram_unpair_pending: None,
            threads_pending: None,
            thread_messages_pending: None,
            user_facts_pending: None,
            slot_health_pending: None,
            fact_check_pending: None,
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
