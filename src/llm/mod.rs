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
            // 4s: governs only the TCP connect phase — LAN model servers
            // connect in <100ms, and a SYN-dropping endpoint (host asleep,
            // firewall drop) previously left the user staring at
            // thinking-dots for 20s per dead slot before failover kicked in.
            .connect_timeout(std::time::Duration::from_secs(4))
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
        self.stream_with_choice(messages, tools, max_tokens, cancel, false)
            .await
    }

    /// `stream`, but with the option to REQUIRE a tool call.
    ///
    /// `force_tool` sends `tool_choice: "required"`, which is the only
    /// forcing form both of our backends honour. The OpenAI
    /// "force this exact function" object — `{"type":"function",
    /// "function":{"name":…}}` — is silently ignored by llama.cpp: it
    /// answers normally, with no tool call and no error, so a caller that
    /// trusted it would think it had a guarantee it never had.
    ///
    /// Because `"required"` forces *some* tool rather than a specific one
    /// (Laguna answered a price question by calling `datetime` twice in
    /// six tries when the full toolset was offered), callers that need a
    /// particular tool must also pass only that tool in `tools`.
    pub async fn stream_with_choice(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        max_tokens: Option<usize>,
        cancel: tokio_util::sync::CancellationToken,
        force_tool: bool,
    ) -> Result<StreamHandle> {
        let payload_messages: Vec<serde_json::Value> =
            messages.iter().map(serialize_message).collect();
        let tools_json: Vec<serde_json::Value> = tools.iter().map(|t| t.schema.clone()).collect();

        let url = format!(
            "{}/v1/chat/completions",
            self.settings.base_url.trim_end_matches('/')
        );
        let tool_choice = if tools_json.is_empty() {
            None
        } else if force_tool {
            Some("required")
        } else {
            Some("auto")
        };
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
        let truncated = choice.finish_reason.as_deref() == Some("length");
        Ok(CompleteResult {
            content: choice.message.content.unwrap_or_default(),
            tool_calls: choice.message.tool_calls.unwrap_or_default(),
            reasoning: choice.message.reasoning.unwrap_or_default(),
            truncated,
        })
    }
}

#[derive(Debug, Default)]
pub struct CompleteResult {
    pub content: String,
    pub tool_calls: Vec<ToolCallOut>,
    /// Hidden chain-of-thought, when the backend exposes one. Kept ONLY so
    /// an empty `content` can be explained: a reasoning model that spends
    /// its whole budget thinking returns nothing visible, and without this
    /// the caller cannot tell that apart from a dead endpoint.
    pub reasoning: String,
    /// `finish_reason == "length"`: generation stopped at the output
    /// ceiling. This is the *reliable* "ran out of room" signal — several
    /// hosted models bill reasoning against `max_tokens` while returning
    /// no reasoning text at all, so an empty `reasoning` cannot rule
    /// truncation out.
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
struct ChatRespFull {
    choices: Vec<ChatChoiceFull>,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceFull {
    message: ChatChoiceMsgFull,
    /// Why generation stopped. `"length"` means the output ceiling was
    /// reached — the only trustworthy signal that a reply was cut off
    /// rather than simply empty. Absent on some servers, hence Option.
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceMsgFull {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallOut>>,
    /// Chain-of-thought channel. Same field-name zoo as the streaming
    /// path (see `StreamDelta` in stream.rs): `reasoning` on vLLM and
    /// several OpenAI-compatible gateways, `reasoning_content` on
    /// llama.cpp, DeepSeek and Qwen3. Accept both — reading only one
    /// spelling is what broke the deep slot back in 0.2.46, and here it
    /// would silently disable the fact-check retry on half the backends
    /// KinAI supports.
    #[serde(default, alias = "reasoning_content")]
    reasoning: Option<String>,
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

/// True when an LLM-turn error means the model SERVER itself is
/// unreachable or not ready — the failure class another slot can rescue
/// (connection refused, connect/DNS trouble, 5xx incl. llama.cpp's
/// "503 Loading model", timeouts, and the stall watchdog). Deliberately
/// narrower than `vision::is_transient_failure`: content-level failures
/// (bad request, context overflow) are NOT another slot's business.
/// True when the provider rejected `tool_choice: "required"` itself,
/// rather than failing for any other reason.
///
/// DeepSeek answers a forced tool choice in thinking mode with
/// `400 {"error":{"message":"Thinking mode does not support this
/// tool_choice",...}}`. llama.cpp and vLLM both accept `required`, so
/// KinAI's forced-search round worked everywhere until the Online slot
/// pointed at a hosted reasoning model (field report 2026-08-12: "which
/// are the top 10 stocks in the QQQ fund — look it up for today?" died
/// with a raw 400 instead of an answer).
///
/// Matched on the phrase rather than the status code: providers differ
/// on whether this is a 400 or a 422, but all of them name the
/// parameter they are refusing.
pub fn is_tool_choice_rejection(err: &str) -> bool {
    let lc = err.to_ascii_lowercase();
    (lc.contains("tool_choice") || lc.contains("tool choice"))
        && (lc.contains("not support")
            || lc.contains("unsupported")
            || lc.contains("invalid")
            || lc.contains("does not"))
}

pub fn is_server_down_error(err: &str) -> bool {
    let lc = err.to_ascii_lowercase();
    [
        "connection refused",
        "error sending request",
        "connect error",
        "tcp connect",
        "dns error",
        "no route",
        "timed out",
        "timeout",
        "went silent",
        "llm error 5",
        "service unavailable",
        "loading model",
    ]
    .iter()
    .any(|n| lc.contains(n))
}

/// Compress a raw server-down error into a 2-3 word reason for the
/// user-facing failover notice ("isn't responding (still loading)").
pub fn short_server_down_reason(err: &str) -> &'static str {
    let lc = err.to_ascii_lowercase();
    if lc.contains("loading model") || lc.contains("503") {
        "model still loading"
    } else if lc.contains("timed out") || lc.contains("timeout") || lc.contains("went silent") {
        "not answering"
    } else if lc.contains("refused")
        || lc.contains("connect")
        || lc.contains("sending request")
        || lc.contains("dns")
        || lc.contains("no route")
    {
        "server unreachable"
    } else {
        "server error"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reasoning channel has two spellings in the wild and the
    /// non-streaming path reads it to decide whether an empty reply was a
    /// budget overrun (retry, explain) or a dead endpoint (don't retry).
    /// Reading only one spelling silently disables that on every backend
    /// using the other — the same mistake that broke the deep slot in
    /// 0.2.46, which is why stream.rs guards both. Guard both here too.
    #[test]
    fn parses_reasoning_content_spelling() {
        let raw = r#"{"choices":[{"message":{"content":"","reasoning_content":"thinking"}}]}"#;
        let parsed: ChatRespFull = serde_json::from_str(raw).unwrap();
        let msg = &parsed.choices[0].message;
        assert_eq!(msg.reasoning.as_deref(), Some("thinking"));
    }

    #[test]
    fn parses_reasoning_canonical_spelling() {
        let raw = r#"{"choices":[{"message":{"content":"","reasoning":"thinking"}}]}"#;
        let parsed: ChatRespFull = serde_json::from_str(raw).unwrap();
        let msg = &parsed.choices[0].message;
        assert_eq!(msg.reasoning.as_deref(), Some("thinking"));
    }

    /// `finish_reason == "length"` is the only trustworthy truncation
    /// signal: models that bill reasoning against `max_tokens` without
    /// returning any reasoning text produce an empty reply that is
    /// otherwise indistinguishable from a dead endpoint.
    #[test]
    fn detects_truncation_from_finish_reason() {
        let raw = r#"{"choices":[{"message":{"content":""},"finish_reason":"length"}]}"#;
        let parsed: ChatRespFull = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.choices[0].finish_reason.as_deref(), Some("length"));
    }

    /// Servers that omit finish_reason entirely must still parse.
    #[test]
    fn missing_finish_reason_is_not_truncation() {
        let raw = r#"{"choices":[{"message":{"content":"hi"}}]}"#;
        let parsed: ChatRespFull = serde_json::from_str(raw).unwrap();
        assert!(parsed.choices[0].finish_reason.is_none());
    }
}

#[cfg(test)]
mod tool_choice_tests {
    use super::is_tool_choice_rejection;

    /// Verbatim from the field, 2026-08-12: a `/online` turn on
    /// deepseek-v4-flash asking for today's QQQ holdings.
    #[test]
    fn deepseek_thinking_mode_rejection_is_recognised() {
        let err = r#"LLM error 400 Bad Request: {"error":{"message":"Thinking mode does not support this tool_choice","type":"invalid_request_error","param":null,"code":"invalid_request_error"}}"#;
        assert!(is_tool_choice_rejection(err));
    }

    #[test]
    fn other_provider_phrasings_are_recognised() {
        assert!(is_tool_choice_rejection(
            r#"400: {"error":{"message":"tool_choice is not supported for this model"}}"#
        ));
        assert!(is_tool_choice_rejection(
            r#"422: {"detail":"Unsupported tool choice: required"}"#
        ));
    }

    /// Must NOT swallow unrelated failures — those still have to reach
    /// the user instead of being retried into a different error.
    #[test]
    fn unrelated_errors_are_left_alone() {
        assert!(!is_tool_choice_rejection("LLM error 401 Unauthorized: bad api key"));
        assert!(!is_tool_choice_rejection("connection refused"));
        assert!(!is_tool_choice_rejection(
            r#"400: {"error":{"message":"Invalid max_tokens value, the valid range of max_tokens is [1, 393216]"}}"#
        ));
        assert!(!is_tool_choice_rejection("context length exceeded"));
    }
}
