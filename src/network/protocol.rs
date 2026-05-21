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
}
