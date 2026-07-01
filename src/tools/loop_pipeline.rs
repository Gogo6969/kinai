//! Tool-calling pipeline.
//!
//! Flow per user message:
//!   1. Build the context (system + memory + history + new user turn).
//!   2. Stream the model's response.
//!   3. If the response includes tool calls, run them concurrently.
//!   4. Inject the tool results as `role: "tool"` messages and re-stream.
//!   5. Stop when finish_reason is `stop` (or after MAX_ROUNDS to avoid loops).
//!
//! Each token is forwarded to `on_token`; tool invocations call `on_tool`.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::context::{ChatMessage, ToolCall, ToolCallFunction};
use crate::llm::stream::{ChatDelta, ToolCallAccum};
use crate::llm::LlmClient;
use crate::tools::registry::{self, ToolDef, ToolRuntime};

/// Shown in place of an empty assistant reply. A completion can come back
/// with no visible content — a reasoning model (the deep slot) that spent its
/// whole budget "thinking", or a backend that returned an empty body. Sending
/// the emptiness onward means a blank chat bubble and, on Telegram, the cryptic
/// "Bad Request: message text is empty".
pub const EMPTY_REPLY_NOTE: &str = "⚠️ The model returned an empty response. \
    If you used /deep, its model server may still be loading, may have hit its \
    output-token limit, or may be offline — please try again in a moment.";

/// Number of streaming rounds where the model is allowed to call tools.
/// After this many rounds without a visible final answer we still do **one
/// more** synthesis round with the tool list emptied — see the loop bottom.
const MAX_ROUNDS: usize = 5;

#[derive(Clone)]
pub struct PipelineHandlers {
    pub on_token: Arc<dyn Fn(String) + Send + Sync>,
    pub on_reasoning: Arc<dyn Fn(String) + Send + Sync>,
    pub on_tool: Arc<dyn Fn(ToolEvent) + Send + Sync>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind")]
pub enum ToolEvent {
    Started { name: String, args: String },
    Finished { name: String, ok: bool, result: String },
}

pub struct PipelineResult {
    pub final_content: String,
}

pub async fn run_pipeline(
    llm: LlmClient,
    initial_messages: Vec<ChatMessage>,
    tools: Vec<ToolDef>,
    max_tokens: Option<usize>,
    runtime: ToolRuntime,
    handlers: PipelineHandlers,
    cancel: CancellationToken,
) -> Result<PipelineResult> {
    let mut messages = initial_messages;
    let accumulated: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let mut any_reasoning = false;
    let mut total_tool_invocations: usize = 0;

    for _round in 0..MAX_ROUNDS {
        let mut handle = llm.stream(&messages, &tools, max_tokens, cancel.clone()).await?;
        let mut content_buf = String::new();
        let mut tool_calls: Vec<ToolCallAccum> = Vec::new();
        let mut finished = false;

        while let Some(delta) = handle.rx.recv().await {
            if cancel.is_cancelled() {
                return Ok(PipelineResult {
                    final_content: accumulated.lock().await.clone(),
                });
            }
            match delta {
                ChatDelta::Token(t) => {
                    content_buf.push_str(&t);
                    accumulated.lock().await.push_str(&t);
                    (handlers.on_token)(t);
                }
                ChatDelta::Reasoning(r) => {
                    any_reasoning = true;
                    (handlers.on_reasoning)(r);
                }
                ChatDelta::ToolCall(tc) => {
                    tool_calls.push(tc);
                }
                ChatDelta::Done { .. } => {
                    finished = true;
                    break;
                }
                ChatDelta::Error(e) => {
                    // Don't discard a partial answer the model already
                    // streamed before the connection broke — common on the
                    // slow deep slot when a stream hiccups mid-generation.
                    // Keep what we have and flag that it was cut short; only
                    // surface a hard error when nothing usable arrived.
                    let partial = accumulated.lock().await.clone();
                    if partial.trim().is_empty() {
                        // Nothing usable arrived. Log the upstream error so a
                        // silent failure (e.g. a vision endpoint returning a
                        // 413 / model-decommissioned that the user only sees
                        // as "no reply") is debuggable from the host log.
                        tracing::warn!("llm stream produced no content: {e}");
                        return Err(anyhow::anyhow!("llm stream: {e}"));
                    }
                    tracing::warn!(
                        "stream error after {} chars of partial answer: {e}",
                        partial.len()
                    );
                    return Ok(PipelineResult {
                        final_content: format!(
                            "{partial}\n\n_(⚠️ reply cut off — the model stopped responding mid-answer)_"
                        ),
                    });
                }
            }
        }
        if !finished {
            break;
        }

        if tool_calls.is_empty() {
            return Ok(PipelineResult {
                final_content: accumulated.lock().await.clone(),
            });
        }

        let assistant_calls: Vec<ToolCall> = tool_calls
            .iter()
            .filter_map(|tc| {
                tc.name.as_ref().map(|name| ToolCall {
                    id: tc.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    r#type: "function".into(),
                    function: ToolCallFunction {
                        name: name.clone(),
                        arguments: if tc.arguments.is_empty() {
                            "{}".into()
                        } else {
                            tc.arguments.clone()
                        },
                    },
                })
            })
            .collect();

        messages.push(ChatMessage::Assistant {
            content: content_buf,
            tool_calls: assistant_calls.clone(),
        });

        for call in assistant_calls {
            total_tool_invocations += 1;
            (handlers.on_tool)(ToolEvent::Started {
                name: call.function.name.clone(),
                args: call.function.arguments.clone(),
            });
            let (result, ok) = match registry::execute(
                &call.function.name,
                &call.function.arguments,
                &runtime,
            )
            .await
            {
                Ok(r) => (r, true),
                Err(e) => (format!("Error running {}: {e}", call.function.name), false),
            };
            (handlers.on_tool)(ToolEvent::Finished {
                name: call.function.name.clone(),
                ok,
                result: result.clone(),
            });
            messages.push(ChatMessage::Tool {
                content: result,
                tool_call_id: call.id,
            });
        }
    }

    // If MAX_ROUNDS exhausted but the model kept calling tools and never
    // produced visible content, force one final round with the tool list
    // empty. The model can't loop further (no tools available), and the
    // accumulated tool results are still in `messages`, so it has all the
    // material it needs to write an answer. This rescues the common
    // "small model can't decide when to stop searching" failure mode.
    if accumulated.lock().await.trim().is_empty() && total_tool_invocations > 0 {
        tracing::info!(
            "tool loop exhausted ({} invocations, no final); forcing a no-tools synthesis round",
            total_tool_invocations
        );
        messages.push(ChatMessage::System {
            content: "You have already gathered enough information from the tool calls above. \
You no longer have any tools available — write the final answer to the user's question now, \
in plain prose, citing the most relevant facts from the tool results."
                .into(),
        });
        let mut handle = llm.stream(&messages, &[], max_tokens, cancel.clone()).await?;
        while let Some(delta) = handle.rx.recv().await {
            if cancel.is_cancelled() {
                break;
            }
            match delta {
                ChatDelta::Token(t) => {
                    accumulated.lock().await.push_str(&t);
                    (handlers.on_token)(t);
                }
                ChatDelta::Reasoning(r) => {
                    (handlers.on_reasoning)(r);
                }
                ChatDelta::Done { .. } | ChatDelta::Error(_) => break,
                _ => {}
            }
        }
    }

    // Per-turn diagnostic: at most one note, only if not a single visible
    // token arrived across every round. The text shape depends on whether
    // we at least saw reasoning / tool calls (so the user knows the model
    // was *doing* something but never produced an answer).
    let mut final_content = accumulated.lock().await.clone();
    if final_content.trim().is_empty() {
        let note = if total_tool_invocations > 0 {
            "_I searched but couldn't pull together a clear answer to that one — it happens with \
very specific or hard-to-find facts. Try rephrasing or narrowing the question, or ask `/deep` \
for a more thorough attempt. (If this happens on every question, your model may not handle \
tool-calling well — you can turn tools off in Settings → Tools.)_"
        } else if any_reasoning {
            "_The model reasoned through your request but didn't produce a visible answer. \
This usually means it tried to call a tool from inside its reasoning channel but the call \
wasn't surfaced as a structured tool_call. Try disabling tools in Settings, or rephrase to \
ask it to answer from its own knowledge._"
        } else {
            "_The model returned an empty response. It may have tried (and failed) to call a \
tool, or hit a stop condition before producing any output. Try rephrasing the question, or — \
if your server doesn't support harmony tool-calling — toggle tools off in Settings._"
        };
        (handlers.on_token)(note.to_string());
        final_content.push_str(note);
    }
    Ok(PipelineResult { final_content })
}
