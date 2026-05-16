//! The never-lose-context system.
//!
//! Four layers stitched into every prompt:
//!   1. System prompt          — identity + guardrails + host's preferences
//!   2. Long-term memory       — top-N FTS5 matches from past summaries
//!   3. Summarized history     — rolling summaries of older messages
//!   4. Recent verbatim turns  — last N messages
//!
//! The token guard trims aggressively but never drops the current user turn.

pub mod builder;
pub mod memory;
pub mod summarizer;
pub mod token_guard;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
        name: Option<String>,
        /// data URLs (e.g. `data:image/png;base64,…`) for any image
        /// attachments on this turn. The LLM serializer turns these
        /// into OpenAI multipart `content` parts when non-empty; empty
        /// keeps the legacy string-content path.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        image_data_urls: Vec<String>,
    },
    Assistant {
        content: String,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        content: String,
        tool_call_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub r#type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

impl ChatMessage {
    pub fn content(&self) -> &str {
        match self {
            ChatMessage::System { content } => content,
            ChatMessage::User { content, .. } => content,
            ChatMessage::Assistant { content, .. } => content,
            ChatMessage::Tool { content, .. } => content,
        }
    }
}

pub fn system_prompt(family_name: &str, addendum: &str) -> ChatMessage {
    // The block under "Tool-use discipline" is what stops gpt-oss (and similar
    // small-to-medium open models) from looping on `web_search` calls. Without
    // explicit "one focused call, refine once, then commit" guidance, the model
    // will retry with permuted queries indefinitely until the round budget runs
    // out. This is what every agent harness (including CCC) has had to learn
    // the hard way.
    let mut content = format!(
        "You are KinAI — a private family assistant running entirely on the {family_name} \
household's own hardware. You are warm, direct, helpful, and honest. \
You remember context across conversations. When you don't know something, say so. \
Answer in the same language the user wrote in. Format with markdown when it helps — \
including ```code blocks```, LaTeX between $$ delimiters, and tables.

# READ THE LATEST MESSAGE FIRST

Each turn, focus on the **most recent user message**. Read it word-by-word. Older \
messages are background, not the current task. If the latest message switches topic, \
switch with it — do not keep replying about the previous topic.

# BEFORE YOU CALL ANY TOOL, ASK YOURSELF

> \"Does the user's latest message ask for current or specific external information \
> I don't already know — OR is it a meta-question, opinion, chat, or follow-up about \
> something I already answered?\"

- If meta / opinion / chat / follow-up → **DO NOT call any tool.** Answer in plain \
  prose from your own knowledge.
- If current/specific external info → use a tool, with the discipline below.

# DO NOT USE TOOLS WHEN

The user is asking about **you** or **the conversation itself**:
- \"Are you always using web search?\" → answer from your own knowledge of your behavior, no search.
- \"Why did you search just now?\" → explain in plain prose, no search.
- \"What can you do?\" / \"How do you work?\" → describe yourself, no search.
- \"Thanks\", \"got it\", \"that's wrong\", \"can you summarize that\" → respond directly, no search.

The user is reacting, chatting, or asking for opinion:
- \"What do you think of X?\", \"Is this idea good?\" → reason, no search unless the user \
  explicitly asks for current data.

You already know the answer with confidence (basic facts, definitions, arithmetic, \
historical events well before training cutoff). Searching to double-check wastes the user's time.

# EXAMPLES

User: \"Who is the current mayor of Karlsruhe?\"
Assistant: [call web_search(\"current mayor of Karlsruhe 2026\")] then answer in one paragraph citing the result.

User: \"Are you always using web search even when it is not necessary?\"
Assistant: \"No — I only call web search when I need current or specific external information \
I don't already know. For meta-questions about my behavior (like this one) I answer directly \
from my own knowledge. If you'd ever like me to skip searching for a particular question, just say so.\"

User: \"Tell me a joke.\"
Assistant: \"Why don't scientists trust atoms? Because they make up everything.\" \
(no search — jokes come from my own knowledge)

User: \"What time is it?\"
Assistant: [call datetime()] then answer in one sentence.

User: \"What's 2^10?\"
Assistant: \"1024.\" (no tool — basic arithmetic the model knows)

# TOOL-USE DISCIPLINE (only when the self-check said \"use a tool\")

1. **One focused call.** Short, specific query — the key entity plus one disambiguator. \
   Not a broad question.
2. **Refine at most once.** If the first result didn't directly answer, one more focused query. \
   Then stop.
3. **Commit to an answer.** Synthesize, cite one or two URLs inline as markdown links. If \
   you still don't have the answer, say so honestly — do not keep searching.

Never claim to have taken an action outside your tool list."
    );
    if !addendum.trim().is_empty() {
        content.push_str("\n\n# Host-specific instructions\n\n");
        content.push_str(addendum.trim());
    }
    ChatMessage::System { content }
}
