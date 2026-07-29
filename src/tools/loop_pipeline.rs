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

    // An earlier reply in this conversation that denied having web access
    // latches the model into repeating that refusal — measured 0/8 on the
    // balanced slot with an otherwise identical prompt. Injecting a
    // correction restored 8/8. Cheap, and only present when the poison is.
    if let Some(note) = correction_for_history(&messages) {
        messages.push(ChatMessage::System { content: note });
    }

    // For questions that plainly need live data, don't ask the model
    // whether to search — require it. See `force_search` for why this is
    // `"required"` with a single-tool list rather than a named function.
    let search_tool: Vec<ToolDef> = tools
        .iter()
        .filter(|t| t.name == "web_search")
        .cloned()
        .collect();
    let mut force_rounds_left = if !search_tool.is_empty() && should_force_search(&messages) {
        tracing::info!("forcing web_search for this turn (question needs live data)");
        // Two attempts: llama.cpp honours `required` most of the time but
        // not always (5/6 on one phrasing), and a second ask is far
        // cheaper than the wrong answer.
        2
    } else {
        0
    };

    for _round in 0..MAX_ROUNDS {
        let forcing = force_rounds_left > 0;
        let (round_tools, force_flag) = if forcing {
            (search_tool.as_slice(), true)
        } else {
            (tools.as_slice(), false)
        };
        let mut handle = llm
            .stream_with_choice(&messages, round_tools, max_tokens, cancel.clone(), force_flag)
            .await?;
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
                let partial = accumulated.lock().await.clone();
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
                break;
            }
            tracing::warn!(
                any_reasoning,
                tool_invocations = total_tool_invocations,
                "model closed a round with no content and no tool calls"
            );
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

        for call in assistant_calls {
            total_tool_invocations += 1;
            (handlers.on_tool)(ToolEvent::Started {
                name: call.function.name.clone(),
                args: call.function.arguments.clone(),
            });
            // Tool calls are logged on BOTH paths. Until 0.2.89 a failed
            // tool left no trace anywhere: a field report of "the model
            // says it can't browse the web" could not be diagnosed after
            // the fact, because the only record of the failed search was
            // the model's own (invented) explanation of it. Log the tool,
            // the arguments, the error and the duration so the next
            // occurrence is answerable from the host log alone.
            let started = std::time::Instant::now();
            let (result, ok) = match registry::execute(
                &call.function.name,
                &call.function.arguments,
                &runtime,
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
                    tracing::warn!(
                        tool = %call.function.name,
                        args = %truncate_for_log(&call.function.arguments),
                        ms = started.elapsed().as_millis() as u64,
                        error = %format!("{e:#}"),
                        "TOOL CALL FAILED"
                    );
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
            if !ok {
                total_tool_failures += 1;
            }
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
        let synthesis_note = if total_tool_failures >= total_tool_invocations {
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
        messages.push(ChatMessage::System {
            content: synthesis_note.into(),
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
        let note = empty_turn_note(any_reasoning, total_tool_invocations);
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
