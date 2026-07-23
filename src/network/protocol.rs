//! Envelope shared between Host and Client over WebSocket.

use serde::{Deserialize, Serialize};

use crate::db::{Message, ThreadMeta};
use crate::tools::loop_pipeline::ToolEvent;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnMetricsWire {
    pub first_token_ms: u64,
    pub total_ms: u64,
    pub output_tokens: u64,
    pub tps: f64,
    /// LLM model id that produced this reply (e.g. "olares/gpt-oss-20b").
    /// Empty when the turn was a slash command (no model involved).
    /// Surfaced in the per-message metrics row so users can tell at a
    /// glance which model answered — especially relevant with the
    /// fast/deep dual-slot routing introduced in v0.2.25.
    #[serde(default)]
    pub model: String,
    /// Slot label: "fast", "deep", or "" when not applicable.
    #[serde(default)]
    pub slot: String,
}

/// One active model slot as advertised to clients in `Welcome`. Clients
/// have no local LLM config, so without this they can't know which
/// `/fast` / `/balanced` / `/deep` switches the host actually serves —
/// the slash menu on client peers stayed empty (0.2.79 bug).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostSlotWire {
    /// Slot slug: "fast", "balanced", or "deep".
    pub slug: String,
    /// Model id served by that slot (e.g. "Laguna-XS-2.1") — shown in
    /// the client's slash-menu entry. Informational only.
    pub model: String,
    /// Whether the slot's model server answered a liveness probe at
    /// connect time. `None` = unknown (older host, or probe skipped).
    /// Connect-time snapshot only — mid-session outages are handled by
    /// the turn-level failover, not by refreshing this flag.
    #[serde(default)]
    pub alive: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Envelope {
    Hello {
        token: String,
        display_name: String,
        client_version: String,
    },
    Welcome {
        family_name: String,
        host_version: String,
        /// The model the host is serving — surfaced read-only on the client so
        /// users can see what's actually running, but never expose anything
        /// they could use to override the host.
        #[serde(default)]
        host_model: String,
        /// Search engine the host has configured (`duckduckgo` / `exa`). Same
        /// rationale — informational only.
        #[serde(default)]
        host_search_engine: String,
        /// One-line label for the vision setup ("Gemini 2.5 Flash",
        /// "Local llava", "off"). Empty / "off" means the host can't
        /// process image attachments — clients use this to disable the
        /// image-attach button preemptively rather than letting the
        /// user paste an image and get a wire-time error.
        #[serde(default)]
        host_vision: String,
        /// `@username` of the host's Telegram bot, or empty string when
        /// the host hasn't configured one yet. Clients display this so
        /// the family member knows which bot to expect when scanning
        /// the QR; it also gates the "Connect Telegram" button on the
        /// client (no bot configured → no point pairing).
        #[serde(default)]
        host_telegram_bot: String,
        /// Active model slots in `slash::SLOTS` order. Drives the
        /// `/fast` / `/balanced` / `/deep` entries in the client's
        /// slash menu (≥2 slots → show the switches, same rule as the
        /// host UI). Snapshot at connect time — refreshed on the next
        /// reconnect if the host owner re-configures slots.
        #[serde(default)]
        host_slots: Vec<HostSlotWire>,
    },
    /// Client → Host: please mint a pairing token for me (the requesting
    /// client peer). The host responds with `TelegramPair`. No payload —
    /// the client peer is identified by the WS session (`context_peer`).
    RequestTelegramPair,
    /// Host → Client: the pairing token's URL + how long it's valid for
    /// + the bot username (so the client can label the QR card).
    TelegramPair {
        url: String,
        expires_in_secs: i64,
        bot_username: String,
    },
    /// Client → Host: what's my current Telegram pairing state? Used
    /// when the Settings card first mounts on a client, and during the
    /// 2s poll loop after the user starts a pairing handshake so the
    /// UI flips from "QR shown" → "✓ Paired" automatically.
    RequestTelegramStatus,
    /// Host → Client: snapshot of the requesting peer's pairing row.
    TelegramStatus {
        bot_configured: bool,
        bot_username: String,
        paired: bool,
        username: Option<String>,
        first_name: Option<String>,
        paired_at: Option<String>,
    },
    /// Client → Host: drop my Telegram pairing. Host responds with
    /// `TelegramUnpairDone` so the UI can refresh deterministically
    /// (rather than relying on the user clicking Refresh).
    RequestTelegramUnpair,
    /// Host → Client: pairing removed; please refresh.
    TelegramUnpairDone,
    SendMessage {
        thread_id: String,
        content: String,
        sender: String,
        client_msg_id: String,
        /// Files the user attached to this turn — images, PDFs, etc.
        /// Empty for plain text. The host extracts text from supported
        /// types (PDF today) and may also route the turn to a vision
        /// endpoint if an image is present (future).
        #[serde(default)]
        attachments: Vec<crate::db::Attachment>,
    },
    StopGeneration {
        client_msg_id: String,
    },
    /// Streaming assistant token.
    Token {
        client_msg_id: String,
        delta: String,
    },
    /// Streaming chain-of-thought trace from a reasoning model.
    Reasoning {
        client_msg_id: String,
        delta: String,
    },
    /// Tool execution event surfaced to the user.
    Tool {
        client_msg_id: String,
        event: ToolEvent,
    },
    /// A complete message has been persisted (user or assistant).
    Message {
        message: Message,
    },
    /// End-of-turn — assistant finished.
    AssistantDone {
        client_msg_id: String,
        message: Message,
        metrics: TurnMetricsWire,
    },
    ListThreads,
    Threads {
        threads: Vec<ThreadMeta>,
    },
    LoadThread {
        thread_id: String,
    },
    ThreadMessages {
        thread_id: String,
        messages: Vec<Message>,
    },
    Ping,
    Pong,
    Error {
        message: String,
    },
    /// Diagnostic snapshot of the full prompt that was sent to the LLM
    /// for a given turn. Emitted right after `AssistantDone` so the
    /// frontend can show a "🔍 prompt" toggle next to the metrics line.
    /// Per-session in-memory cache only — never persisted, so older
    /// messages from before this session won't have a snapshot.
    PromptDebug {
        /// ID of the assistant message this prompt produced.
        assistant_msg_id: String,
        /// Pretty-printed JSON of the full ChatMessage array sent to the
        /// LLM (system prompt + memory recalls + recent turns + current).
        prompt: String,
    },

    // ---- User facts (persistent memory) — client ↔ host ----
    //
    // The host stores per-peer user_facts in its DB, scoped by peer_id.
    // The LLM already reads them in build_context — what these envelopes
    // add is a way for the CLIENT to see/edit its own facts via the
    // Settings → Memory page. Without these, the client's UI reads its
    // local DB (which is empty because the LLM runs on the host).
    //
    // Privacy: every host-side handler dispatches with `context_peer`
    // (the connecting client's peer_id), so a peer can only ever see/
    // modify its own facts. A client cannot impersonate another peer's
    // scope because the peer_id is derived from the connection's JWT,
    // not the envelope body.
    //
    // The host responds to ALL four mutations with a fresh `UserFacts`
    // list. That keeps the client UI in sync with one round-trip per
    // action and removes the need for separate ack envelopes.
    /// Client → Host: fresh liveness for the advertised slots (the
    /// Welcome snapshot is connect-time only). Host answers `SlotHealth`
    /// from its short-TTL probe cache, so this is cheap to call when the
    /// slash menu opens.
    RequestSlotHealth,
    /// Host → Client: the active slots with up-to-date `alive` flags.
    SlotHealth {
        slots: Vec<HostSlotWire>,
    },
    ListUserFacts,
    SaveUserFact {
        key: String,
        value: String,
    },
    DeleteUserFact {
        id: String,
    },
    ClearUserFacts,
    UserFacts {
        facts: Vec<crate::db::UserFact>,
    },
}
