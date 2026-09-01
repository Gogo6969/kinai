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
use crate::tools::force_search;
use crate::tools::registry::{self, ToolDef, ToolRuntime};

/// Shown in place of an empty assistant reply. A completion can come back
/// with no visible content — a reasoning model (the deep slot) that spent its
/// whole budget "thinking", or a backend that returned an empty body. Sending
/// the emptiness onward means a blank chat bubble and, on Telegram, the cryptic
/// "Bad Request: message text is empty".
pub const EMPTY_REPLY_NOTE: &str = "⚠️ The model returned an empty response. \
    If you used /deep, its model server may still be loading, may have hit its \
    output-token limit, or may be offline — please try again in a moment.";

/// Shown when the user pressed Stop before any visible token arrived.
/// Without it, an empty result is rewritten by every consumer into
/// EMPTY_REPLY_NOTE, which blames the model server for the user's own
/// deliberate action.
pub const STOPPED_NOTE: &str = "_(stopped)_";

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
    let mut total_tool_failures: usize = 0;
    // Tools that failed permanently this turn (billing/auth/config).
    // Shared with the concurrent execution futures so a permanent failure
    // shields every LATER-STARTING call — including same-round calls still
    // queued behind the concurrency cap. Plain std Mutex: never held
    // across an await.
    let dead_tools: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    // remember/forget mutate the family DB; two writes racing would make
    // the surviving value a coin flip. They serialize through this gate —
    // FIFO, and `buffered` starts futures in call order, so mutators also
    // KEEP the model's call order relative to each other.
    let mutation_gate = Arc::new(tokio::sync::Semaphore::new(1));

    // An earlier reply in this conversation that denied having web access
    // latches the model into repeating that refusal — measured 0/8 on the
    // balanced slot with an otherwise identical prompt. Injecting a
    // correction restored 8/8. Cheap, and only present when the poison is.
    //
    // Only when web_search is actually IN this request's toolset: the
    // correction asserts "you have a working `web_search` tool ... right
    // now", which on a tool-free turn (the external Vision route) or a
    // host with search disabled is false — and instructing a model to use
    // a tool it doesn't have is precisely how tool-call syntax ends up
    // pasted into a visible reply.
    let has_web_search = tools.iter().any(|t| t.name == "web_search");
    if let Some(note) = has_web_search.then(|| correction_for_history(&messages)).flatten() {
        // Merge into the LEADING system message rather than appending a
        // second one at the end. Strict chat templates reject a system
        // message anywhere but position 0 —
        // `Qwen3.6-35B-A3B-base-static` raises "System message must be at
        // the beginning" and the whole turn dies with a 400. It also
        // belongs at the top semantically: it is a standing instruction,
        // not a mid-conversation aside.
        match messages.first_mut() {
            Some(ChatMessage::System { content }) => {
                content.push_str("\n\n");
                content.push_str(&note);
            }
            _ => messages.insert(0, ChatMessage::System { content: note }),
        }
    }

    // For questions that plainly need live data, don't ask the model
    // whether to search — require it. See `force_search` for why this is
    // `"required"` with a single-tool list rather than a named function.
    let search_tool: Vec<ToolDef> = tools
        .iter()
        .filter(|t| t.name == "web_search")
        .cloned()
        .collect();
    // Ceiling on tool output for this turn, measured against the prompt we
    // are actually starting from. Everything the loop appends is charged
    // against it, so a tool-heavy turn degrades to truncated results
    // instead of a 400 from the model server after the work is done.
    let window = llm.settings.context_window;
    let mut tool_tokens_left = tool_output_budget(
        window,
        crate::context::token_guard::estimate_messages(&messages),
        max_tokens,
    );
    tracing::debug!(window, tool_tokens_left, "tool-output budget for this turn");

    let mut force_rounds_left = if !search_tool.is_empty() && should_force_search(&messages) {
        tracing::info!("forcing web_search for this turn (question needs live data)");
        // Two attempts: llama.cpp honours `required` most of the time but
        // not always (5/6 on one phrasing), and a second ask is far
        // cheaper than the wrong answer.
        2
    } else {
        0
    };

    // True when the loop burned every round still calling tools — the
    // model was mid-work when the budget ran out, so the accumulated
    // content is narration, not a conclusion.
    let mut rounds_exhausted = true;
    for _round in 0..MAX_ROUNDS {
        let forcing = force_rounds_left > 0;
        let (round_tools, force_flag) = if forcing {
            (search_tool.as_slice(), true)
        } else {
            (tools.as_slice(), false)
        };
        let mut handle = match llm
            .stream_with_choice(&messages, round_tools, max_tokens, cancel.clone(), force_flag)
            .await
        {
            Ok(h) => h,
            // The provider refuses to be forced. DeepSeek's thinking mode
            // rejects `tool_choice: "required"` outright, and before this
            // the whole turn died with a raw 400 in the chat. Forcing is
            // an optimisation, not a requirement: drop it and run the
            // round normally, with the full toolset, so the model can
            // still choose to search.
            Err(e) if forcing && crate::llm::is_tool_choice_rejection(&e.to_string()) => {
                tracing::warn!(
                    "provider rejects tool_choice=required ({e:#}); running this turn unforced"
                );
                force_rounds_left = 0;
                llm.stream_with_choice(&messages, tools.as_slice(), max_tokens, cancel.clone(), false)
                    .await?
            }
            Err(e) => return Err(e),
        };
        let mut content_buf = String::new();
        let mut tool_calls: Vec<ToolCallAccum> = Vec::new();
        let mut finished = false;

        while let Some(delta) = handle.rx.recv().await {
            if cancel.is_cancelled() {
                // Returning an empty string here made every consumer
                // substitute EMPTY_REPLY_NOTE — "the model returned an
                // empty response … may be offline" — for a turn the user
                // deliberately stopped. A forced round widened that window,
                // because no token is visible until the search has run, so
                // say plainly that it was stopped.
                let partial = sanitize_partial(&accumulated.lock().await);
                return Ok(PipelineResult {
                    final_content: if partial.trim().is_empty() {
                        STOPPED_NOTE.to_string()
                    } else {
                        partial
                    },
                });
            }
            match delta {
                ChatDelta::Token(t) => {
                    // A forced round is supposed to produce a tool call, not
                    // prose. Anything it writes is either a throwaway
                    // preamble or — the case this exists for — the refusal
                    // we are overriding. Don't stream it to the user: it
                    // would appear on screen and then have to be taken back
                    // when the round is retried. The real answer streams in
                    // the next (unforced) round.
                    if forcing {
                        content_buf.push_str(&t);
                        continue;
                    }
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
                    let partial = sanitize_partial(&partial);
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

        if tool_calls.is_empty() && forcing {
            // We required a tool and the backend produced none anyway —
            // llama.cpp does this occasionally. Discard whatever prose it
            // wrote instead (that text is exactly the refusal we are
            // trying to prevent) and ask again; on the last attempt fall
            // through to normal behaviour rather than failing the turn.
            force_rounds_left -= 1;
            tracing::warn!(
                attempts_left = force_rounds_left,
                dropped_chars = content_buf.len(),
                "forced search round returned no tool call; retrying"
            );
            continue;
        }

        if tool_calls.is_empty() {
            // A round with no tool calls is the normal end of a turn — but
            // only if the model actually said something. A reasoning model
            // (deep slot's Qwen3.6) routinely spends a round entirely in
            // its `reasoning_content` channel and closes with `stop` and an
            // empty `content`. Returning here handed the caller an empty
            // string, which surfaced as "the model server may still be
            // loading … or may be offline" — blaming a server that was
            // healthy and had just streamed several hundred characters of
            // thinking. Fall through to the diagnostics below instead, so
            // the user is told what actually happened.
            let visible = accumulated.lock().await.clone();
            if !visible.trim().is_empty() {
                // `break`, not `return` — the tail of this function owns
                // finalisation, and returning from here skipped it. That
                // silently disabled the 0.2.89 "every lookup failed"
                // banner for the COMMON case: a model that writes its
                // answer in a final round with no tool call exits through
                // here, so the banner only appeared when the round limit
                // happened to be exhausted first. Caught by
                // tests/search_failure_live.rs after the fact.
                //
                // Natural end only when THIS round contributed content: a
                // closing round that adds nothing on top of earlier
                // narration ("let me dig deeper…" then a bare stop) is a
                // dangling turn and takes the finish-NOW rescue below.
                rounds_exhausted =
                    content_buf.trim().is_empty() && total_tool_invocations > 0;
                break;
            }
            tracing::warn!(
                any_reasoning,
                tool_invocations = total_tool_invocations,
                "model closed a round with no content and no tool calls"
            );
            rounds_exhausted = false;
            break;
        }

        // The forced round did its job. Every later round gets the full
        // toolset and normal `auto` choice, so the model can answer from
        // the results — or reach for another tool if it genuinely needs one.
        force_rounds_left = 0;

        // Drop any prose the forced round wrote before its tool call.
        //
        // We already keep it off the screen, but it was still being stored
        // as the assistant turn — and Laguna's habitual preamble IS the
        // refusal ("I don't have real-time market data, but let me look").
        // That put a fresh capability denial into the very context this
        // feature exists to keep clean, one message before the model wrote
        // its visible answer, and after the correction note had already
        // been placed. An assistant message carrying only tool_calls is
        // valid and is what a silent round produces anyway.
        if forcing {
            content_buf.clear();
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

        // The round's calls run CONCURRENTLY (capped), then their results
        // are accounted for strictly in the model's call order. A research
        // question fires 3–8 web searches per round, each a 4–15s network
        // wait; running them back to back cost one family member a
        // 284-second first token. `buffered` preserves output order, so
        // everything order-sensitive — tool_call_id pairing, the token
        // budget, dead-tool bookkeeping — happens identically to the old
        // sequential loop, just without the serialized waiting.
        //
        // A permanent failure shields every call that has not yet STARTED
        // — across rounds and within one, via the shared dead set. Calls
        // already in flight beside the failing one complete normally; a
        // real result from a doomed-anyway call is strictly more
        // information, so nothing is lost.
        const TOOL_CONCURRENCY: usize = 4;
        enum Outcome {
            /// Tool was already dead when this call would have started.
            Skipped { note: String },
            /// Ran; `text` is what the model will read (result or failure
            /// note), already emitted to the UI as a Finished event.
            Done { text: String, ok: bool },
        }
        let executed: Vec<(ToolCall, Outcome)> = {
            use futures_util::stream::{self, StreamExt};
            let runtime_ref = &runtime;
            stream::iter(assistant_calls.into_iter().map(|call| {
                let on_tool = handlers.on_tool.clone();
                let dead_tools = dead_tools.clone();
                let mutation_gate = mutation_gate.clone();
                async move {
                    // Checked at START time, not round-planning time: a
                    // permanent failure in an already-finished call of this
                    // same round shields calls still queued behind the cap.
                    if dead_tools.lock().unwrap().contains(&call.function.name) {
                        let note = format!(
                            "TOOL UNAVAILABLE ({}): an earlier call this turn failed permanently \
(billing, authentication or configuration) and will not succeed by retrying. Do NOT call \
it again in this turn. Tell the user once what is unavailable and answer as best you can \
without it.",
                            call.function.name
                        );
                        tracing::info!(tool = %call.function.name, "skipping tool: already failed permanently this turn");
                        (on_tool)(ToolEvent::Started {
                            name: call.function.name.clone(),
                            args: call.function.arguments.clone(),
                        });
                        (on_tool)(ToolEvent::Finished {
                            name: call.function.name.clone(),
                            ok: false,
                            result: note.clone(),
                        });
                        return (call, Outcome::Skipped { note });
                    }
                    // Writers wait their turn; read-only tools run freely.
                    let _permit = if matches!(call.function.name.as_str(), "remember" | "forget") {
                        Some(mutation_gate.acquire().await)
                    } else {
                        None
                    };
                    (on_tool)(ToolEvent::Started {
                        name: call.function.name.clone(),
                        args: call.function.arguments.clone(),
                    });
                    // Tool calls are logged on BOTH paths. Until 0.2.89 a
                    // failed tool left no trace anywhere: a field report of
                    // "the model says it can't browse the web" could not be
                    // diagnosed after the fact. Log the tool, arguments,
                    // error and duration so the next occurrence is
                    // answerable from the host log alone.
                    let started = std::time::Instant::now();
                    let (text, ok) = match registry::execute(
                        &call.function.name,
                        &call.function.arguments,
                        runtime_ref,
                    )
                    .await
                    {
                        Ok(r) => {
                            tracing::info!(
                                tool = %call.function.name,
                                args = %truncate_for_log(&call.function.arguments),
                                ms = started.elapsed().as_millis() as u64,
                                result_chars = r.len(),
                                "tool call ok"
                            );
                            (r, true)
                        }
                        Err(e) => {
                            let chain = format!("{e:#}");
                            tracing::warn!(
                                tool = %call.function.name,
                                args = %truncate_for_log(&call.function.arguments),
                                ms = started.elapsed().as_millis() as u64,
                                error = %chain,
                                "TOOL CALL FAILED"
                            );
                            if is_permanent_tool_failure(&chain) {
                                dead_tools.lock().unwrap().insert(call.function.name.clone());
                            }
                            (
                                // The model reads this as the tool's output — make the
                                // failure impossible to gloss over. Without this, a turn
                                // whose every search errored still got told to "answer
                                // from the results", and it fabricated (the 2026 World
                                // Cup incident: all Exa calls failed, the model recited
                                // the 2022 final as current news).
                                //
                                // The capability clause is the 0.2.89 addition. Told only
                                // that a tool "failed", models rationalise the failure as
                                // a limitation of themselves — a field report came back
                                // with "I can't provide a link, as my knowledge doesn't
                                // include live web browsing", which is false (the tool
                                // exists and works) and sends the user off to do the
                                // lookup by hand.
                                //
                                // Display formatting ({e}), not the {e:#} cause chain —
                                // the model-visible text stays as terse as the
                                // sequential loop's.
                                format!(
                                    "TOOL FAILED ({}): {e}. This tool returned NO information. Do not \
invent or recall-from-memory what it was meant to provide — if the user's question \
depends on it, say plainly that the lookup failed and suggest trying again.\n\
IMPORTANT: this was a temporary failure of ONE call, not a limitation of yours. You \
DO have working web access through your tools. Never tell the user you cannot browse \
the web, lack live search, or only have training data — that is false and unhelpful. \
Say the lookup failed and offer to try again.",
                                    call.function.name
                                ),
                                false,
                            )
                        }
                    };
                    // Emitted here, as each call completes, so the UI shows
                    // per-call progress instead of a burst at round end —
                    // and every Finished immediately follows its own
                    // Started. The event carries the UNTRIMMED text; only
                    // what the model has to carry is shortened later.
                    (on_tool)(ToolEvent::Finished {
                        name: call.function.name.clone(),
                        ok,
                        result: text.clone(),
                    });
                    (call, Outcome::Done { text, ok })
                }
            }))
            .buffered(TOOL_CONCURRENCY)
            .collect()
            .await
        };

        for (call, outcome) in executed {
            total_tool_invocations += 1;
            let (result, ok) = match outcome {
                Outcome::Skipped { note } => {
                    total_tool_failures += 1;
                    messages.push(ChatMessage::Tool {
                        content: note,
                        tool_call_id: call.id,
                    });
                    continue;
                }
                Outcome::Done { text, ok } => (text, ok),
            };
            if !ok {
                total_tool_failures += 1;
            }
            // Calls that were already in flight when a peer's PERMANENT
            // failure landed couldn't be shielded by the dead set — they
            // ran, failed identically, and their notes say "offer to try
            // again", which is exactly wrong for a dead tool. By
            // accounting time the set is settled, so upgrade those notes.
            let result = if !ok && dead_tools.lock().unwrap().contains(&call.function.name) {
                format!(
                    "{result}\nUPDATE: this tool has failed PERMANENTLY for this turn \
(billing, authentication or configuration). Do NOT call it again in this turn, and do \
not offer to retry it."
                )
            } else {
                result
            };
            // Search results reach the model through this preamble because
            // a model whose training data confidently "knows" the answer
            // will override fresh results with its prior: asked for QQQ's
            // top holding, one slot cited Apple straight over a search
            // result saying NVIDIA. Scoped to the current state of the
            // world — snippets can be stale or cached, so evergreen
            // knowledge must not be overridden by an SEO fragment. Skipped
            // when the engine found nothing: an empty result set under a
            // "results win" order reads as "refuse to answer". Prepended,
            // not appended, so `fit_tool_result` trims results — never the
            // instruction.
            let result = if ok && call.function.name == "web_search" && search_found_something(&result) {
                format!(
                    "(Live web search results, retrieved just now. Your training data is \
older than these results: for questions about the current state of the world — prices, \
holdings, standings, office-holders, news — answer ONLY from these results, never from \
memory, even when memory feels certain. Where the results disagree with each other, go \
with what the majority of the most recent ones say — a single stale page does not beat \
several current ones. If the results do not answer the question, say the search came up \
short rather than guessing.)\n{result}"
                )
            } else {
                result
            };
            let (result, truncated) = fit_tool_result(result, tool_tokens_left);
            let cost = crate::context::token_guard::count_tokens(&result);
            tool_tokens_left = tool_tokens_left.saturating_sub(cost);
            if truncated {
                tracing::warn!(
                    tool = %call.function.name,
                    tokens_left = tool_tokens_left,
                    "tool result truncated to fit the model's context"
                );
            }
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
    let narration_only = rounds_exhausted && !accumulated.lock().await.trim().is_empty();
    if total_tool_invocations > 0
        && (accumulated.lock().await.trim().is_empty() || narration_only)
    {
        tracing::info!(
            narration_only,
            "tool loop exhausted ({} invocations); forcing a no-tools synthesis round",
            total_tool_invocations
        );
        let synthesis_note = if narration_only {
            // The model spent every round narrating its search ("let me
            // look deeper…") without ever concluding — the family saw a
            // stream of intentions that just stopped. CONTINUATION
            // semantics, not a restart: a round-5 reply that already
            // contained the full answer must not be answered twice.
            "You have used every available tool round. CONTINUE your reply above to its \
conclusion using only the results already gathered — do not restart it and do not repeat \
anything already written. If it already states the final answer, add at most one short \
closing sentence. If the results did not settle the question, say exactly that instead \
of promising more searching. Do not write tool-call syntax."
        } else if total_tool_failures >= total_tool_invocations {
            // EVERY tool call failed — there are no results to cite. The old
            // text claimed the model had "gathered enough information", which
            // invited fabrication dressed up as fresh results.
            "Every tool call above FAILED — you have no fresh information. You no longer \
have any tools available. Tell the user plainly that the lookup failed and could not be \
completed right now, suggest they try again in a moment, and do NOT present anything from \
memory as if it were a current result."
        } else {
            "You have gathered information from the tool calls above. You no longer have any \
tools available — write the final answer to the user's question now, in plain prose, citing \
the most relevant facts from the SUCCESSFUL tool results. Results marked TOOL FAILED \
contained no information — do not fabricate what they were meant to provide."
        };
        // Sent as a USER turn, not a system one: it has to sit AFTER the
        // tool results to mean anything, and a system message in that
        // position is rejected outright by strict templates (see the
        // correction-note comment above). It follows tool messages, so
        // this never creates two user turns in a row.
        messages.push(ChatMessage::User {
            content: synthesis_note.into(),
            name: None,
            image_data_urls: vec![],
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
    // A model under round pressure sometimes writes raw tool-call syntax
    // into its visible answer instead of calling a tool (the deep slot did
    // this in the synthesis round: the whole reply was `<tool_call>...`).
    // The family must never see that. Strip the syntax; if nothing
    // substantive remains, an honest note replaces it.
    let mut syntax_only = false;
    if contains_tool_syntax(&final_content) {
        tracing::warn!("final content contained raw tool-call syntax; sanitizing");
        final_content = match strip_tool_syntax(&final_content) {
            Some(clean) => clean,
            None => {
                syntax_only = true;
                String::new()
            }
        };
    }
    if final_content.trim().is_empty() {
        let note = if syntax_only {
            TOOL_SYNTAX_NOTE
        } else {
            empty_turn_note(any_reasoning, total_tool_invocations)
        };
        (handlers.on_token)(note.to_string());
        final_content.push_str(note);
    } else if every_lookup_failed(total_tool_invocations, total_tool_failures) {
        (handlers.on_token)(ALL_LOOKUPS_FAILED_BANNER.to_string());
        final_content.push_str(ALL_LOOKUPS_FAILED_BANNER);
    }
    Ok(PipelineResult { final_content })
}

/// EVERY lookup this turn failed, yet the model still wrote prose. Whatever
/// it says came from memory, not from a live result — and we can't rely on
/// it saying so: the instruction in the TOOL FAILED payload is guidance, and
/// the field report that prompted this had the model claim outright that it
/// had no web access. State the truth deterministically, so the warning is
/// there no matter how the model chose to narrate the failure.
/// Shown when the model's entire visible reply was raw tool-call syntax
/// (stripped above). Honest about whose fault it is — retrying usually
/// works, and nothing is wrong with the server or KinAI's tools.
pub const TOOL_SYNTAX_NOTE: &str = "⚠️ The model tried to keep using its tools \
instead of writing an answer — a model quirk, not a KinAI or server problem. \
Please ask again; a retry usually completes normally.";

pub(crate) const ALL_LOOKUPS_FAILED_BANNER: &str =
    "\n\n_⚠️ Every web lookup for this answer failed, so nothing above comes from a live \
search — it is the model's own recollection and may be wrong or out of date. KinAI's \
search does work; this was a failure of the lookup itself. Ask again to retry it._";

/// Show the banner only when every lookup that ran failed. A turn with one
/// good result and one failure still has live grounding, and warning there
/// would train the family to ignore the banner.
pub(crate) fn every_lookup_failed(invocations: usize, failures: usize) -> bool {
    invocations > 0 && failures >= invocations
}

/// Which diagnostic to show when a turn produced no visible text at all.
/// Split out so the wording is testable: the `any_reasoning` branch used to
/// blame the model server ("may still be loading … or may be offline") for
/// what is actually a healthy server whose reasoning model simply never left
/// its thinking channel.
pub(crate) fn empty_turn_note(any_reasoning: bool, tool_invocations: usize) -> &'static str {
    if tool_invocations > 0 {
        "_I searched but couldn't pull together a clear answer to that one — it happens with \
very specific or hard-to-find facts. Try rephrasing or narrowing the question, or ask `/deep` \
for a more thorough attempt. (If this happens on every question, your model may not handle \
tool-calling well — you can turn tools off in Settings → Tools.)_"
    } else if any_reasoning {
        "_The model spent this turn \"thinking\" and never wrote a visible answer — its whole \
reply went to the hidden reasoning channel. Your model server is fine; this is the model \
stopping after its own thoughts. Ask again (it often clears on its own), or rephrase more \
directly — \"answer in one short line\" tends to snap it out of it._"
    } else {
        "_The model returned an empty response. It may have tried (and failed) to call a \
tool, or hit a stop condition before producing any output. Try rephrasing the question, or — \
if your server doesn't support harmony tool-calling — toggle tools off in Settings._"
    }
}

/// Failures that will fail again identically for the rest of this turn:
/// billing, auth, and missing configuration. Retrying them costs the user
/// a wait and an apology per round and cannot change the outcome.
///
/// Field case (2026-07-29): Exa hit its credit limit and the model called
/// `web_search` four times in one turn, each returning the same
/// `402 NO_MORE_CREDITS`, so the reply opened with four near-identical
/// apologies before finally answering. Timeouts, 5xx and 429 are NOT in
/// here — those genuinely can succeed on a later call, and web_search
/// retries them at the HTTP layer.
/// Substring matching is safe here because this only ever sees error
/// strings from `registry::execute`, never successful result text — a
/// search result *about* HTTP 402 cannot disable the tool.
/// Whether a successful `web_search` result actually contains results.
/// Every engine reports an empty set as a final "No results…" line — which
/// survives as the last line even under the fallback chain's "(Exa is
/// unavailable — …)" prefix.
fn search_found_something(result: &str) -> bool {
    result
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| !l.trim().starts_with("No results"))
}

/// Markers of raw tool-call syntax leaking into visible content. Covers
/// the formats seen in the field: llama-style `<function=...>`, Qwen/GLM
/// `<tool_call>` blocks, and the invented `<toolcall>` variant.
const TOOL_SYNTAX_MARKERS: &[&str] = &["<tool_call", "<function=", "<toolcall"];

fn line_starts_syntax(line: &str) -> bool {
    let t = line.trim_start().to_ascii_lowercase();
    TOOL_SYNTAX_MARKERS.iter().any(|m| t.starts_with(m))
        // A bare close tag on its own line is syntax too — nested blocks
        // (<tool_call> wrapping <function=…>) end the inner skip at the
        // inner closer and would otherwise leave the outer closer behind.
        || ["</tool_call", "</toolcall", "</function"].iter().any(|m| t.starts_with(m))
}

/// Only LINE-STARTING markers outside code fences count: the field leaks
/// are block-shaped, while a legitimate answer that MENTIONS the syntax
/// ("use `<tool_call>` tags") keeps it inline or fenced and must survive
/// untouched.
pub(crate) fn contains_tool_syntax(content: &str) -> bool {
    let mut in_fence = false;
    for line in content.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && line_starts_syntax(line) {
            return true;
        }
    }
    false
}

/// Remove block-shaped tool-syntax from visible content, line-wise:
/// from a line that starts with a marker through the line containing its
/// close tag (or end of message when unclosed — the streamed-halfway
/// case). Fenced code and inline mentions pass through. Returns the
/// surviving prose, or `None` only when nothing at all survives.
pub(crate) fn strip_tool_syntax(content: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    let mut in_fence = false;
    let mut skipping = false;
    for line in content.lines() {
        if line.trim_start().starts_with("```") && !skipping {
            in_fence = !in_fence;
            out.push(line);
            continue;
        }
        if skipping {
            let lc = line.to_ascii_lowercase();
            if ["</tool_call>", "</toolcall>", "</function>"].iter().any(|c| lc.contains(c)) {
                skipping = false;
            }
            continue;
        }
        if !in_fence && line_starts_syntax(line) {
            skipping = true;
            // A one-line block closes on its own line.
            let lc = line.to_ascii_lowercase();
            if ["</tool_call>", "</toolcall>", "</function>"].iter().any(|c| lc.contains(c)) {
                skipping = false;
            }
            continue;
        }
        out.push(line);
    }
    let cleaned = out.join("\n").trim().to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Early-return variant of the final-content sanitizing: a stream that
/// dies or is stopped mid-generation is PRECISELY when half-streamed tool
/// syntax sits in the buffer. Empty result means "nothing usable" — the
/// callers substitute their own notes.
fn sanitize_partial(content: &str) -> String {
    if contains_tool_syntax(content) {
        tracing::warn!("partial content contained raw tool-call syntax; sanitizing");
        strip_tool_syntax(content).unwrap_or_default()
    } else {
        content.to_string()
    }
}

fn is_permanent_tool_failure(err: &str) -> bool {
    let e = err.to_lowercase();
    // A failed fallback CHAIN is never permanent for the tool: the message
    // carries Exa's 402/401 verbatim, but the chain's tail (SearXNG,
    // DuckDuckGo) fails transiently, so a later call this turn can succeed.
    if e.contains("fallback also failed") {
        return false;
    }
    ["(401", "(402", "(403", "(404", "no_more_credits", "payment required",
     "no api key", "api key is not configured", "invalid api key", "unauthorized"]
        .iter()
        .any(|m| e.contains(m))
}

/// No single tool result may exceed this many characters, whatever the
/// context window. One `web_search` returns 4–5k chars; a page fetch can
/// return far more, and a single monster result should never be able to
/// crowd out the conversation on its own.
const MAX_CHARS_PER_TOOL_RESULT: usize = 8_000;

/// Tokens held back for the model's own answer when sizing tool output.
///
/// This is a realistic ANSWER length, not the caller's `max_tokens`.
/// `compute_max_tokens` returns "everything left in the window" — on a
/// 32k model with a 4k prompt that is ~28k — so treating it as the
/// reserve subtracted the whole window and left a zero budget, silently
/// truncating every tool result to nothing. Caught in the 0.2.92 smoke
/// test, where nine searches all logged `tokens_left=0` on a thread of
/// barely 1,800 tokens and the model answered "let me try a different
/// approach". A pinned, smaller `max_tokens` is still honoured.
const GENERATION_RESERVE: usize = 2_048;

/// How many tokens of tool output this turn may accumulate.
///
/// The pipeline appends every tool result to the message list and used to
/// never look at the total again — the prompt is trimmed once, before the
/// turn, and nothing bounded what the loop added afterwards. A single
/// research question on 2026-07-30 ran 13 searches, appended 62,590 chars
/// (~15.6k tokens) and pushed the request to 16,698 tokens against a
/// 16,384-token server, which answered `400 exceed_context_size_error`.
/// The whole turn was lost at the last round, after all 13 searches had
/// been paid for.
///
/// Budget = window − (prompt already built) − (room to answer) − safety.
/// Returns 0 when the prompt alone has already filled the window, in
/// which case results are replaced by a short note rather than truncated.
fn tool_output_budget(window: usize, prompt_tokens: usize, max_tokens: Option<usize>) -> usize {
    const SAFETY: usize = 256;
    // Cap the reserve at a realistic answer: a caller-supplied `max_tokens`
    // is usually the whole remaining window, not an intended answer length.
    let reserve = max_tokens
        .map(|m| m.min(GENERATION_RESERVE))
        .unwrap_or(GENERATION_RESERVE)
        .max(512);
    window
        .saturating_sub(prompt_tokens)
        .saturating_sub(reserve)
        .saturating_sub(SAFETY)
}

/// Shrink one tool result to `budget` tokens, on a char boundary, with a
/// marker so the model knows it is reading a fragment and does not report
/// a partial list as complete.
fn fit_tool_result(result: String, budget_tokens: usize) -> (String, bool) {
    const NOTE: &str = "\n\n[… truncated to fit the model's context. This result is INCOMPLETE — \
say so if the answer depends on what was cut.]";
    let capped = if result.chars().count() > MAX_CHARS_PER_TOOL_RESULT {
        let head: String = result.chars().take(MAX_CHARS_PER_TOOL_RESULT).collect();
        format!("{head}{NOTE}")
    } else {
        result
    };
    if budget_tokens == 0 {
        return (
            "[tool result omitted — the conversation has filled this model's context. \
Tell the user the lookup succeeded but could not be included, and suggest a narrower question.]"
                .to_string(),
            true,
        );
    }
    if crate::context::token_guard::count_tokens(&capped) <= budget_tokens {
        return (capped, false);
    }
    // Cut by characters using the observed chars-per-token ratio, then
    // shrink until it really fits — the ratio varies with the content
    // (JSON and CJK tokenize very differently from English prose).
    let total_tokens = crate::context::token_guard::count_tokens(&capped).max(1);
    let ratio = capped.chars().count() as f64 / total_tokens as f64;
    let mut take = ((budget_tokens as f64) * ratio) as usize;
    loop {
        let head: String = capped.chars().take(take).collect();
        let candidate = format!("{head}{NOTE}");
        if crate::context::token_guard::count_tokens(&candidate) <= budget_tokens || take < 200 {
            return (candidate, true);
        }
        take = take * 3 / 4;
    }
}

/// The newest user message, which is the question this turn must answer.
fn latest_user_question(messages: &[ChatMessage]) -> Option<&str> {
    messages.iter().rev().find_map(|m| match m {
        ChatMessage::User { content, .. } => Some(content.as_str()),
        _ => None,
    })
}

/// Force a search only for a fresh question that needs live data, and only
/// when this turn hasn't already run tools (a re-entry with tool results
/// in `messages` must not loop back into forcing).
pub(crate) fn should_force_search(messages: &[ChatMessage]) -> bool {
    if messages.iter().any(|m| matches!(m, ChatMessage::Tool { .. })) {
        return false;
    }
    // Never force a search on a turn whose message carries an image: the
    // caption is about the picture ("how much is this worth today?"), and
    // a mandatory search round would answer the caption instead of the
    // image. The model can still CHOOSE to search — image turns keep the
    // toolset since 0.2.104 — forcing is what stays off.
    if matches!(
        messages.iter().rev().find(|m| matches!(m, ChatMessage::User { .. })),
        Some(ChatMessage::User { content, image_data_urls, .. })
            if !image_data_urls.is_empty()
                || content.contains(crate::vision::IMAGE_STRIP_MARKER)
    ) {
        return false;
    }
    // Classify only what the user TYPED. `format_user` concatenates the
    // typed prose with the full text extracted from any attached PDF, so
    // classifying the raw field let a private document decide to send a
    // query to a third-party search engine — on a turn where the user
    // typed nothing but "summarise this".
    latest_user_question(messages)
        .map(|q| force_search::needs_live_data(force_search::typed_prose(q)))
        .unwrap_or(false)
}

/// A correction to inject when an earlier assistant turn in this
/// conversation denied having web access. `None` when the history is clean
/// — the note is only worth its tokens when there is poison to counter.
pub(crate) fn correction_for_history(messages: &[ChatMessage]) -> Option<String> {
    let poisoned = messages.iter().any(|m| match m {
        ChatMessage::Assistant { content, .. } => force_search::is_capability_denial(content),
        _ => false,
    });
    poisoned.then(|| force_search::CORRECTION_NOTE.to_string())
}

/// Arguments are model-authored and can be long (an image prompt, a pasted
/// paragraph). Keep log lines readable and bounded — and slice on a char
/// boundary, since a query can be any language.
fn truncate_for_log(s: &str) -> String {
    const MAX: usize = 160;
    if s.chars().count() <= MAX {
        return s.replace('\n', " ");
    }
    let cut: String = s.chars().take(MAX).collect();
    format!("{}…", cut.replace('\n', " "))
}

#[cfg(test)]
mod log_tests {
    use super::truncate_for_log;

    #[test]
    fn short_args_pass_through_with_newlines_flattened() {
        assert_eq!(truncate_for_log(r#"{"query":"a b"}"#), r#"{"query":"a b"}"#);
        assert_eq!(truncate_for_log("line1\nline2"), "line1 line2");
    }

    #[test]
    fn long_args_are_cut_on_a_char_boundary() {
        // A query can be in any language; slicing by byte would panic here.
        let cyrillic = "я".repeat(400);
        let out = truncate_for_log(&cyrillic);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 161, "160 chars + ellipsis");

        let emoji = "🇩🇪".repeat(300);
        assert!(truncate_for_log(&emoji).ends_with('…'));
    }
}

#[cfg(test)]
mod failure_surface_tests {
    use super::{empty_turn_note, every_lookup_failed, EMPTY_REPLY_NOTE};

    #[test]
    fn banner_only_when_every_lookup_failed() {
        assert!(!every_lookup_failed(0, 0), "no tools ran — nothing to warn about");
        assert!(!every_lookup_failed(2, 0), "both searches worked");
        assert!(
            !every_lookup_failed(2, 1),
            "one good result still grounds the answer; crying wolf trains users to ignore it"
        );
        assert!(every_lookup_failed(1, 1));
        assert!(every_lookup_failed(3, 3));
    }

    #[test]
    fn a_thinking_only_turn_does_not_blame_the_server() {
        // The 2026-07-27 field report: deep streamed hundreds of chars of
        // reasoning against a healthy server, and the user was told it
        // "may still be loading … or may be offline".
        let note = empty_turn_note(true, 0);
        assert!(note.contains("thinking"), "should name the real cause");
        assert!(note.contains("server is fine"));
        for wrong in ["offline", "still be loading"] {
            assert!(!note.contains(wrong), "must not blame the server: {wrong}");
        }
        assert!(EMPTY_REPLY_NOTE.contains("may be offline"), "unchanged fallback");
    }

    #[test]
    fn tool_turn_and_bare_empty_turn_get_their_own_notes() {
        assert_ne!(empty_turn_note(true, 0), empty_turn_note(false, 0));
        assert!(empty_turn_note(false, 2).contains("searched"));
        assert!(empty_turn_note(true, 2).contains("searched"), "tools win over reasoning");
    }
}

#[cfg(test)]
mod force_tests {
    use super::*;

    fn user(s: &str) -> ChatMessage {
        ChatMessage::User { content: s.into(), name: None, image_data_urls: vec![] }
    }
    fn assistant(s: &str) -> ChatMessage {
        ChatMessage::Assistant { content: s.into(), tool_calls: vec![] }
    }
    const REFUSAL: &str =
        "I don't have live access to current events, and my training data only goes up to 2024.";

    #[test]
    fn never_forces_on_an_image_bearing_turn() {
        // The caption is about the PICTURE — a mandatory search round
        // would answer the caption instead of looking at the image. The
        // model keeps its tools and can still choose to search.
        let with_image = ChatMessage::User {
            content: "how much is this worth today?".into(),
            name: None,
            image_data_urls: vec!["data:image/png;base64,AAAA".into()],
        };
        assert!(!should_force_search(&[with_image]));
        // A failover to a non-vision slot strips the image and leaves the
        // marker — the caption is still about a picture.
        let stripped = user(&format!(
            "how much is this worth today?\n\n{}",
            crate::vision::IMAGE_STRIP_MARKER
        ));
        assert!(!should_force_search(&[stripped]));
        // The same phrasing without an image still forces.
        assert!(should_force_search(&[user("how much is bitcoin worth today?")]));
    }

    #[test]
    fn forces_on_a_live_question_and_not_otherwise() {
        assert!(should_force_search(&[user("What is the Bitcoin price rn?")]));
        assert!(should_force_search(&[user("Tell me the news of today.")]));
        assert!(!should_force_search(&[user("what is 2+2")]));
        assert!(!should_force_search(&[user("can you summarize that")]));
    }

    #[test]
    fn judges_the_newest_question_not_an_older_one() {
        // A live question earlier in the thread must not force a search on
        // a later "thanks" — the turn being answered is the last one.
        let msgs = vec![
            user("What is the Bitcoin price rn?"),
            assistant("$63,204 as of today (source: CoinGecko)."),
            user("thanks, that's all"),
        ];
        assert!(!should_force_search(&msgs));
    }

    #[test]
    fn empty_search_results_are_recognized_across_engines() {
        use super::search_found_something;
        // The three engines' empty-set markers, bare and behind the
        // fallback chain's note — none may earn the "results win" preamble.
        for empty in [
            "No results.",
            "No results found",
            "(Exa is unavailable — it did not respond. These results come \
from DuckDuckGo instead.)\nNo results found",
            "  \n",
            "",
        ] {
            assert!(!search_found_something(empty), "for: {empty:?}");
        }
        for real in [
            "1. NVIDIA tops QQQ\n   nvda is 8.3%\n   https://example.com",
            "(Exa is unavailable — its credits are used up. These results \
come from your own SearXNG instead.)\n1. A result\n   https://example.com",
        ] {
            assert!(search_found_something(real), "for: {real:?}");
        }
    }

    #[test]
    fn legitimate_mentions_of_tool_syntax_survive_sanitizing() {
        use super::{contains_tool_syntax, strip_tool_syntax};
        // Inline mention in prose — a dev-family answer about the syntax.
        let inline = "Yes — wrap the call in `<tool_call>` tags and the server parses it.";
        assert!(!contains_tool_syntax(inline));
        // Fenced example blocks are teaching material, not leaks.
        let fenced = "The format looks like this:\n```\n<tool_call>\n<function=web_search>\n</function>\n</tool_call>\n```\nThat is the whole shape.";
        assert!(!contains_tool_syntax(fenced));
        // A short real answer followed by a leaked block keeps the answer.
        let short = "It's about $110k right now.\n<tool_call>{\"name\":\"web_search\"}</tool_call>";
        assert_eq!(strip_tool_syntax(short).as_deref(), Some("It's about $110k right now."));
    }

    #[test]
    fn tool_syntax_is_stripped_from_visible_replies() {
        use super::{contains_tool_syntax, strip_tool_syntax};
        // The exact failure from the 2026-08-31 hallucination test: deep's
        // whole reply was one raw block.
        let pure = "<tool_call>\n<function=web_search>\n<parameter=query>\n\"Kepler-Vogt instability\" definition\n</parameter>\n</function>\n</tool_call>";
        assert!(contains_tool_syntax(pure));
        assert_eq!(strip_tool_syntax(pure), None);
        // Mixed: prose survives, syntax goes.
        let mixed = "Here is what I found about the treaty, based on the two sources that mention it in passing.\n<tool_call>{\"name\":\"web_search\"}</tool_call>\nNothing verifiable exists.";
        let cleaned = strip_tool_syntax(mixed).unwrap();
        assert!(cleaned.contains("what I found"));
        assert!(cleaned.contains("Nothing verifiable"));
        assert!(!contains_tool_syntax(&cleaned));
        // Unclosed (stream cut mid-syntax): everything from the marker on is dropped.
        let unclosed = "The answer needs one more lookup, which I will run right away for you now:\n<function=web_search>...";
        let cleaned = strip_tool_syntax(unclosed).unwrap();
        assert!(!cleaned.contains("<function"));
        // Clean prose passes through untouched.
        assert!(!contains_tool_syntax("A perfectly normal answer about < 5 things."));
    }

    #[test]
    fn chain_failures_never_kill_the_tool_permanently() {
        use super::is_permanent_tool_failure;
        // Exa's 402 is permanent alone, but a failed chain's tail is
        // transient — the combined message must not disable web_search.
        assert!(is_permanent_tool_failure("Exa search failed (402): no credits"));
        assert!(!is_permanent_tool_failure(
            "Exa failed ((402): no credits); the SearXNG fallback also \
failed (connect refused); the DuckDuckGo fallback also failed (timed out)"
        ));
    }

    #[test]
    fn never_forces_once_tool_results_are_in_play() {
        // Re-entering with results present must not loop back into forcing,
        // or the turn could never get past the search round.
        let msgs = vec![
            user("What is the Bitcoin price rn?"),
            ChatMessage::Tool { content: "1. BTC $63,204".into(), tool_call_id: "t1".into() },
            user("What is the Bitcoin price rn?"),
        ];
        assert!(!should_force_search(&msgs));
    }

    #[test]
    fn correction_appears_only_when_history_is_poisoned() {
        assert!(correction_for_history(&[user("hi"), assistant("Hello!")]).is_none());
        let poisoned = vec![user("news?"), assistant(REFUSAL), user("bitcoin price?")];
        let note = correction_for_history(&poisoned).expect("should correct");
        assert!(note.contains("FALSE"), "must contradict the refusal");
        assert!(note.contains("web_search"));
    }

    #[test]
    fn a_reply_that_merely_mentions_searching_is_not_poison() {
        let clean = vec![
            user("news?"),
            assistant("I searched for real-time headlines and found three stories."),
        ];
        assert!(correction_for_history(&clean).is_none());
    }
}

#[cfg(test)]
mod force_regression_tests {
    use super::*;

    fn user(s: &str) -> ChatMessage {
        ChatMessage::User { content: s.into(), name: None, image_data_urls: vec![] }
    }

    /// The critical finding from the 2026-07-29 adversarial review: the
    /// classifier read `ChatMessage::User.content`, which `format_user`
    /// fills with the typed prose PLUS the full extracted text of any
    /// attached PDF. A private invoice mentioning prices and dates could
    /// therefore cause KinAI to send a query to Exa on a turn where the
    /// user asked only for a summary.
    #[test]
    fn an_attached_document_cannot_trigger_a_search() {
        let pdf_turn = user(
            "summarise this please\n\nINVOICE 2026-07-29\nCurrent price list\n\
             Total cost EUR 12,480.00\nPayment due today\nBitcoin accepted",
        );
        assert!(
            !should_force_search(&[pdf_turn]),
            "document text must not decide to search"
        );
    }

    #[test]
    fn the_typed_question_still_decides() {
        let live = user("what's the bitcoin price today?\n\n[attached image: chart.png]");
        assert!(should_force_search(&[live]));
    }
}

#[cfg(test)]
mod tool_budget_tests {
    use super::*;
    use crate::context::token_guard::count_tokens;

    #[test]
    fn a_generous_max_tokens_does_not_zero_the_budget() {
        // THE 0.2.92 SMOKE-TEST BUG. compute_max_tokens hands us
        // "everything left in the window" (~28k on a 32k model with a 4k
        // prompt). Subtracting that as the reserve left nothing for tool
        // output, so every search result was truncated to zero and the
        // model answered from nothing. The budget must stay large.
        let b = tool_output_budget(32_768, 4_000, Some(28_640));
        assert!(b > 25_000, "budget collapsed to {b} — the reserve is eating the window");

        // The real thread from the smoke test: ~1,800 tokens of history.
        let b = tool_output_budget(32_768, 1_819, Some(30_821));
        assert!(b > 28_000, "budget {b} too small for a 32k window and a tiny thread");
    }

    #[test]
    fn a_pinned_small_max_tokens_is_still_honoured() {
        // A user who pins max_tokens=512 wants a short answer; don't
        // reserve more than they asked for.
        let b = tool_output_budget(16_384, 2_000, Some(512));
        assert_eq!(b, 16_384 - 2_000 - 512 - 256);
    }

    #[test]
    fn budget_leaves_room_to_answer() {
        // 16k window, 2k of prompt already built, 1k reserved to answer.
        let b = tool_output_budget(16_384, 2_000, Some(1_024));
        assert!(b > 12_000 && b < 13_500, "unexpected budget {b}");
        // Room to answer is never given away, even with a huge prompt.
        assert_eq!(tool_output_budget(16_384, 16_000, Some(1_024)), 0);
        assert_eq!(tool_output_budget(16_384, 99_999, None), 0);
    }

    #[test]
    fn a_result_that_fits_is_untouched() {
        let small = "1. Some search hit\n   https://example.com".to_string();
        let (out, truncated) = fit_tool_result(small.clone(), 4_000);
        assert_eq!(out, small);
        assert!(!truncated);
    }

    #[test]
    fn an_oversized_result_is_cut_to_the_budget() {
        // The real case: ~4.8k chars per search, 13 of them, tiny budget left.
        let big = "lorem ipsum dolor sit amet ".repeat(2_000); // ~54k chars
        let (out, truncated) = fit_tool_result(big, 500);
        assert!(truncated);
        assert!(count_tokens(&out) <= 500, "still {} tokens", count_tokens(&out));
        assert!(out.contains("INCOMPLETE"), "model must be told it is a fragment");
    }

    #[test]
    fn a_single_monster_result_is_capped_even_with_budget_to_spare() {
        let huge = "x".repeat(400_000);
        let (out, _) = fit_tool_result(huge, 1_000_000);
        assert!(out.chars().count() <= MAX_CHARS_PER_TOOL_RESULT + 200);
    }

    #[test]
    fn an_exhausted_budget_omits_rather_than_lies() {
        let (out, truncated) = fit_tool_result("real results here".into(), 0);
        assert!(truncated);
        assert!(out.contains("omitted"));
        assert!(out.contains("lookup succeeded"), "must not read as a failed lookup");
    }

    #[test]
    fn thirteen_searches_stay_inside_a_16k_window() {
        // Replays the 2026-07-30 failure: 13 results of ~4.8k chars each
        // against a 16,384-token window with a 2k prompt already built.
        let mut left = tool_output_budget(16_384, 2_000, Some(1_024));
        let mut total = 0usize;
        for _ in 0..13 {
            let (out, _) = fit_tool_result("search result text ".repeat(260), left);
            let cost = count_tokens(&out);
            total += cost;
            left = left.saturating_sub(cost);
        }
        assert!(
            total + 2_000 + 1_024 < 16_384,
            "13 results totalled {total} tokens — would still overflow"
        );
    }
}

#[cfg(test)]
mod permanent_failure_tests {
    use super::is_permanent_tool_failure;

    #[test]
    fn billing_auth_and_config_failures_are_permanent() {
        for e in [
            // The exact 2026-07-29 field error.
            "Exa search failed (402 Payment Required): {\"error\":\"You have exceeded your \
credits limit\",\"tag\":\"NO_MORE_CREDITS\"}",
            "Exa search failed (401): invalid API key",
            "Exa search failed (403): forbidden",
            "Exa search failed (404): not found",
            "Exa is selected as the search engine but no API key is configured.",
        ] {
            assert!(is_permanent_tool_failure(e), "should not be retried: {e}");
        }
    }

    #[test]
    fn transient_failures_stay_retryable() {
        // These can succeed on a later call — web_search already retries
        // them at the HTTP layer, and the model may legitimately try again.
        for e in [
            "Exa search failed (500): upstream error",
            "Exa search failed (502): bad gateway",
            "Exa search failed (503): unavailable",
            "Exa search failed (429): slow down",
            "operation timed out",
            "error sending request: connection closed",
        ] {
            assert!(!is_permanent_tool_failure(e), "must stay retryable: {e}");
        }
    }

}
