//! Local LLM client.
//!
//! Every supported backend (Ollama, LM Studio, vLLM, llama.cpp, Open WebUI,
//! AnythingLLM) speaks the OpenAI Chat Completions API, so a single client
//! works for all of them. Auto-detection probes the well-known ports.

pub mod detect;
pub mod stream;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::config::LlmSettings;
use crate::context::ChatMessage;
use crate::tools::registry::ToolDef;

pub use stream::{ChatDelta, StreamHandle};

#[derive(Debug, Clone)]
pub struct LlmClient {
    pub settings: LlmSettings,
    pub http: reqwest::Client,
}

#[derive(Debug, Clone, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [serde_json::Value],
    temperature: f32,
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    /// When tools are present, send `"auto"` — this is what well-behaved
    /// agent harnesses do (CCC, OpenAI Agents SDK, etc.), and several vLLM
    /// serving paths only dispatch into the harmony tool-call parser when
    /// the flag is set explicitly. With no tools, omit the field entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ToolCallOut {
    pub id: Option<String>,
    pub function: ToolCallOutFn,
}

#[derive(Debug, Deserialize)]
pub struct ToolCallOutFn {
    pub name: String,
    pub arguments: String,
}

impl LlmClient {
    pub fn new(settings: LlmSettings) -> Self {
        let http = reqwest::Client::builder()
            // No OVERALL request timeout. A streaming generation on the deep
            // slot (a large reasoning model on llama.cpp) routinely runs
            // longer than a few minutes, and a total-request cap (was 180s)
            // killed it mid-stream — surfacing as
            // "sse: Transport error: error decoding response body". Streaming
            // liveness is instead enforced by a per-chunk inactivity timeout
            // in the pump (src/llm/stream.rs); a runaway request is bounded by
            // the user's Stop (CancellationToken). Non-streaming `complete()`
            // calls set their own per-request ceiling below.
            .connect_timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("reqwest client");
        Self { settings, http }
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        detect::list_models(&self.settings).await
    }

    /// Streaming chat completion.
    /// `max_tokens` overrides the settings value when provided. Passing
    /// `None` omits the field from the request entirely so the server uses
    /// its own ceiling (the model's full context).
    pub async fn stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        max_tokens: Option<usize>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<StreamHandle> {
        let payload_messages: Vec<serde_json::Value> =
            messages.iter().map(serialize_message).collect();
        let tools_json: Vec<serde_json::Value> = tools.iter().map(|t| t.schema.clone()).collect();

        let url = format!(
            "{}/v1/chat/completions",
            self.settings.base_url.trim_end_matches('/')
        );
        let tool_choice = if tools_json.is_empty() { None } else { Some("auto") };
        let req = ChatRequest {
            model: &self.settings.model,
            messages: &payload_messages,
            temperature: self.settings.temperature,
            stream: true,
            tools: tools_json,
            tool_choice,
            max_tokens,
        };
        let mut builder = self.http.post(&url).json(&req);
        // Only attach an Authorization header when there's actually a key.
        // A blank/whitespace key means "local server" (llama.cpp, LM Studio,
        // Ollama, vLLM) — those need no auth, and sending `Bearer ` (empty)
        // makes some of them answer 401. Cloud endpoints always set a key.
        if let Some(key) = self.settings.api_key.as_deref() {
            if !key.trim().is_empty() {
                builder = builder.bearer_auth(key);
            }
        }
        stream::open(builder, cancel).await
    }

    /// Non-streaming chat completion. Used internally for short calls like tool re-prompting.
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        max_tokens: Option<usize>,
    ) -> Result<CompleteResult> {
        let payload_messages: Vec<serde_json::Value> =
            messages.iter().map(serialize_message).collect();
        let tools_json: Vec<serde_json::Value> = tools.iter().map(|t| t.schema.clone()).collect();
        let url = format!(
            "{}/v1/chat/completions",
            self.settings.base_url.trim_end_matches('/')
        );
        let tool_choice = if tools_json.is_empty() { None } else { Some("auto") };
        let req = ChatRequest {
            model: &self.settings.model,
            messages: &payload_messages,
            temperature: self.settings.temperature,
            stream: false,
            tools: tools_json,
            tool_choice,
            max_tokens,
        };
        // Non-streaming call → keep a finite ceiling (the client itself no
        // longer sets one, since streaming requests must be uncapped).
        let mut builder = self
            .http
            .post(&url)
            .timeout(std::time::Duration::from_secs(180))
            .json(&req);
        // Only attach an Authorization header when there's actually a key.
        // A blank/whitespace key means "local server" (llama.cpp, LM Studio,
        // Ollama, vLLM) — those need no auth, and sending `Bearer ` (empty)
        // makes some of them answer 401. Cloud endpoints always set a key.
        if let Some(key) = self.settings.api_key.as_deref() {
            if !key.trim().is_empty() {
                builder = builder.bearer_auth(key);
            }
        }
        let resp = builder.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("LLM error {status}: {body}"));
        }
        let parsed: ChatRespFull = resp.json().await?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no choices"))?;
        Ok(CompleteResult {
            content: choice.message.content.unwrap_or_default(),
            tool_calls: choice.message.tool_calls.unwrap_or_default(),
        })
    }
}

#[derive(Debug, Default)]
pub struct CompleteResult {
    pub content: String,
    pub tool_calls: Vec<ToolCallOut>,
}

#[derive(Debug, Deserialize)]
struct ChatRespFull {
    choices: Vec<ChatChoiceFull>,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceFull {
    message: ChatChoiceMsgFull,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceMsgFull {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallOut>>,
}

fn serialize_message(m: &ChatMessage) -> serde_json::Value {
    match m {
        ChatMessage::System { content } => serde_json::json!({
            "role": "system",
            "content": content,
        }),
        ChatMessage::User { content, name, image_data_urls } => {
            // When images are attached we emit the OpenAI multipart
            // `content` array form — `[{type:"text",...}, {type:"image_url",...}]`.
            // Every supported vision endpoint (Gemini OpenAI-compat shim,
            // Anthropic OpenAI-compat shim, vLLM/Ollama llava/qwen-vl,
            // OpenAI proper) accepts this shape. For text-only turns we
            // keep the string form so non-vision endpoints don't break.
            let content_value = if image_data_urls.is_empty() {
                serde_json::Value::String(content.clone())
            } else {
                let mut parts: Vec<serde_json::Value> = Vec::with_capacity(image_data_urls.len() + 1);
                if !content.is_empty() {
                    parts.push(serde_json::json!({
                        "type": "text",
                        "text": content,
                    }));
                }
                for url in image_data_urls {
                    parts.push(serde_json::json!({
                        "type": "image_url",
                        "image_url": { "url": url },
                    }));
                }
                serde_json::Value::Array(parts)
            };
            let mut obj = serde_json::json!({
                "role": "user",
                "content": content_value,
            });
            if let Some(n) = name {
                obj["name"] = serde_json::Value::String(n.clone());
            }
            obj
        }
        ChatMessage::Assistant { content, tool_calls } => {
            let mut obj = serde_json::json!({
                "role": "assistant",
                "content": content,
            });
            if !tool_calls.is_empty() {
                obj["tool_calls"] = serde_json::json!(tool_calls
                    .iter()
                    .map(|tc| serde_json::json!({
                        "id": tc.id,
                        "type": tc.r#type,
                        "function": {
                            "name": tc.function.name,
                            "arguments": tc.function.arguments,
                        }
                    }))
                    .collect::<Vec<_>>());
            }
            obj
        }
        ChatMessage::Tool { content, tool_call_id } => serde_json::json!({
            "role": "tool",
            "content": content,
            "tool_call_id": tool_call_id,
        }),
    }
}
