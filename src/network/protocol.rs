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
    },
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
