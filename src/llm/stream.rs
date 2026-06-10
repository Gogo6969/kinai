//! Server-Sent-Events parser for OpenAI-style streaming chat completions.
//!
//! Also handles OpenAI's "harmony" response format used by gpt-oss models,
//! which embeds channel markers inside `delta.content`:
//!
//!   * `<|channel|>thought ... <|end|>`       → reasoning (collapsible UI)
//!   * `<|channel|>analysis ... <|end|>`      → reasoning (alt name)
//!   * `<|channel|>final ... <|return|>`      → visible content
//!   * `<|channel|>commentary to=functions.X<|message|>{json}<|call|>`
//!                                            → synthesized as a structured
//!                                              tool_call so the existing
//!                                              tool-execution pipeline runs
//!                                              the call and feeds results
//!                                              back to the model.
//!
//! Some quantizations of gpt-oss emit a quirky abbreviated form where the
//! markers use only one pipe (`<|channel>thought` / `<channel|>`); we
//! recognize both.

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::ToolCallOut;

// ---- Harmony parser --------------------------------------------------------

#[derive(Default)]
struct HarmonyParser {
    buffer: String,
    channel: HarmonyChannel,
    /// While we're in Commentary with a known tool target, body bytes are
    /// accumulated here (not emitted to the stream). On channel close we
    /// emit a single ToolCall.
    pending_tool: Option<PendingTool>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum HarmonyChannel {
    #[default]
    Default,   // pre-marker / plain text → visible
    Thought,   // chain-of-thought → reasoning
    Final,     // user-facing answer → visible
    Commentary,// either status notes (visible) or tool calls (when pending_tool is Some)
}

#[derive(Debug, Clone, Default)]
struct PendingTool {
    name: String,
    args: String,
}

#[derive(Debug, Clone)]
enum HarmonyEmit {
    Visible(String),
    Reasoning(String),
    ToolCall { name: String, arguments: String },
}

impl HarmonyParser {
    fn feed(&mut self, chunk: &str) -> Vec<HarmonyEmit> {
        self.buffer.push_str(chunk);
        let mut out: Vec<HarmonyEmit> = Vec::new();

        loop {
            let hit = next_marker(&self.buffer);
            match hit {
                None => {
                    let safe = safe_emit_len(&self.buffer);
                    if safe == 0 {
                        break;
                    }
                    let segment: String = self.buffer.drain(..safe).collect();
                    self.absorb(segment, &mut out);
                    break;
                }
                Some(MarkerHit { start, end, kind }) => {
                    if start > 0 {
                        let before: String = self.buffer.drain(..start).collect();
                        self.absorb(before, &mut out);
                    }
                    // Drop the marker bytes.
                    let marker_len = end - start;
                    self.buffer.drain(..marker_len);
                    self.transition(kind, &mut out);
                }
            }
        }
        out
    }

    fn flush(&mut self) -> Vec<HarmonyEmit> {
        let mut out = Vec::new();
        if !self.buffer.is_empty() {
            let leftover: String = std::mem::take(&mut self.buffer);
            self.absorb(leftover, &mut out);
        }
        // If we ended mid-tool-call, surface what we have so the model gets
        // a result back rather than hanging.
        if let Some(pt) = self.pending_tool.take() {
            if !pt.name.is_empty() {
                out.push(HarmonyEmit::ToolCall {
                    name: pt.name,
                    arguments: if pt.args.trim().is_empty() {
                        "{}".into()
                    } else {
                        pt.args
                    },
                });
            }
        }
        out
    }

    fn absorb(&mut self, segment: String, out: &mut Vec<HarmonyEmit>) {
        if segment.is_empty() {
            return;
        }
        match (self.channel, self.pending_tool.as_mut()) {
            (HarmonyChannel::Thought, _) => out.push(HarmonyEmit::Reasoning(segment)),
            (HarmonyChannel::Commentary, Some(pt)) => {
                // Inside a tool-call commentary block — buffer the JSON args.
                pt.args.push_str(&segment);
            }
            (HarmonyChannel::Commentary, None) => {
                // Plain commentary (no tool target) — surface as reasoning,
                // since it's usually status notes about what the model is
                // doing rather than the final answer.
                out.push(HarmonyEmit::Reasoning(segment));
            }
            (HarmonyChannel::Default | HarmonyChannel::Final, _) => {
                out.push(HarmonyEmit::Visible(segment));
            }
        }
    }

    fn transition(&mut self, kind: Marker, out: &mut Vec<HarmonyEmit>) {
        match kind {
            Marker::ThoughtOpen => {
                self.flush_pending_tool(out);
                self.channel = HarmonyChannel::Thought;
            }
            Marker::FinalOpen => {
                self.flush_pending_tool(out);
                self.channel = HarmonyChannel::Final;
            }
            Marker::CommentaryOpen { tool } => {
                self.flush_pending_tool(out);
                self.channel = HarmonyChannel::Commentary;
                self.pending_tool = tool.map(|name| PendingTool {
                    name,
                    args: String::new(),
                });
            }
            Marker::CallClose | Marker::Close => {
                self.flush_pending_tool(out);
                self.channel = HarmonyChannel::Default;
            }
            Marker::Skip => {
                // <|message|>, <|start|>, <|constrain|>… — purely structural.
            }
        }
    }

    fn flush_pending_tool(&mut self, out: &mut Vec<HarmonyEmit>) {
        if let Some(pt) = self.pending_tool.take() {
            if !pt.name.is_empty() {
                out.push(HarmonyEmit::ToolCall {
                    name: pt.name,
                    arguments: if pt.args.trim().is_empty() {
                        "{}".into()
                    } else {
                        pt.args.trim().to_string()
                    },
                });
            }
        }
    }
}

// ---- Marker recognition ---------------------------------------------------

#[derive(Debug, Clone)]
enum Marker {
    ThoughtOpen,
    FinalOpen,
    CommentaryOpen { tool: Option<String> },
    /// `<|call|>` — only used to close tool-call commentary blocks.
    CallClose,
    /// `<|end|>`, `<|return|>`, `<channel|>` — generic channel close.
    Close,
    /// `<|message|>`, `<|start|>`, `<|constrain|>…` — structural, skip.
    Skip,
}

#[derive(Debug)]
struct MarkerHit {
    start: usize,
    end: usize,
    kind: Marker,
}

/// Find the earliest harmony marker in `s` and return its byte range + kind.
fn next_marker(s: &str) -> Option<MarkerHit> {
    // Look for any '<' in the buffer; that's the only way a marker starts.
    // For each candidate, try to recognize a marker pattern.
    let mut best: Option<MarkerHit> = None;
    let consider = |hit: MarkerHit, best: &mut Option<MarkerHit>| {
        if best.as_ref().map_or(true, |b| hit.start < b.start) {
            *best = Some(hit);
        }
    };

    // Simple literal markers we can find with `find`.
    const SIMPLE: &[(&str, fn() -> Marker)] = &[
        ("<|channel|>thought", || Marker::ThoughtOpen),
        ("<|channel>thought", || Marker::ThoughtOpen),
        ("<|channel|>analysis", || Marker::ThoughtOpen),
        ("<|channel>analysis", || Marker::ThoughtOpen),
        ("<|channel|>final", || Marker::FinalOpen),
        ("<|channel>final", || Marker::FinalOpen),
        ("<|call|>", || Marker::CallClose),
        ("<|end|>", || Marker::Close),
        ("<|return|>", || Marker::Close),
        ("<channel|>", || Marker::Close),
        ("<|message|>", || Marker::Skip),
        ("<|start|>", || Marker::Skip),
        ("<|constrain|>", || Marker::Skip),
    ];
    for (pat, make) in SIMPLE {
        if let Some(idx) = s.find(pat) {
            consider(
                MarkerHit { start: idx, end: idx + pat.len(), kind: make() },
                &mut best,
            );
        }
    }

    // Commentary openers carry an optional `to=functions.<name>` directive
    // that ends at the next `<|message|>` (or quirky `<message>`). We need a
    // tiny custom scanner for this.
    for prefix in ["<|channel|>commentary", "<|channel>commentary"] {
        if let Some(idx) = s.find(prefix) {
            let after = idx + prefix.len();
            // Find the end of the opener: first occurrence of `<|message|>`
            // or `<message>` after `after`.
            let term_rel_msg = s[after..].find("<|message|>");
            let term_rel_msg_short = s[after..].find("<message>");
            let (term_end, term_len) = match (term_rel_msg, term_rel_msg_short) {
                (Some(a), Some(b)) if a <= b => (after + a, "<|message|>".len()),
                (Some(a), None) => (after + a, "<|message|>".len()),
                (None, Some(b)) => (after + b, "<message>".len()),
                (Some(_), Some(b)) => (after + b, "<message>".len()),
                (None, None) => {
                    // The opener exists but its terminator hasn't streamed
                    // in yet — skip for now, we'll see it on the next feed.
                    continue;
                }
            };
            let directive = &s[after..term_end];
            let tool = parse_commentary_tool(directive);
            consider(
                MarkerHit {
                    start: idx,
                    end: term_end + term_len,
                    kind: Marker::CommentaryOpen { tool },
                },
                &mut best,
            );
        }
    }

    best
}

/// Extract `to=functions.<name>` from a commentary directive like
/// ` to=functions.web_search <|constrain|>json`.
fn parse_commentary_tool(directive: &str) -> Option<String> {
    let needle = "to=functions.";
    let i = directive.find(needle)?;
    let rest = &directive[i + needle.len()..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '<' || c == ',' || c == ';')
        .unwrap_or(rest.len());
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// How many leading bytes are safe to emit (i.e., can't be the prefix of a
/// not-yet-complete marker). Keeps the tail buffered for the next chunk.
fn safe_emit_len(s: &str) -> usize {
    const MAX_MARKER_LEN: usize = 64;
    let len = s.len();
    let mut tail_start = len.saturating_sub(MAX_MARKER_LEN);
    // Align forward to a UTF-8 char boundary. `len - 64` can land inside a
    // multi-byte character (emoji/CJK/accented Latin — common in normal
    // model output), and slicing there panics, killing the pump task and
    // silently truncating the reply. Keeping a few extra tail bytes
    // buffered is harmless.
    while tail_start < len && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if let Some(rel) = s[tail_start..].rfind('<') {
        return tail_start + rel;
    }
    len
}

#[cfg(test)]
mod safe_emit_tests {
    use super::safe_emit_len;

    #[test]
    fn never_slices_inside_a_multibyte_char() {
        // 40 emoji × 4 bytes = 160 bytes, no '<' — pre-fix this panicked
        // on the s[len-64..] slice landing mid-character.
        let s = "🎉".repeat(40);
        let n = safe_emit_len(&s);
        assert!(s.is_char_boundary(n), "offset must be a char boundary");
        let _ = &s[..n]; // must not panic
    }

    #[test]
    fn cjk_and_accented_text_is_boundary_safe() {
        for s in ["日本語のテキストです".repeat(10), "café ".repeat(30)] {
            let n = safe_emit_len(&s);
            assert!(s.is_char_boundary(n));
            let _ = &s[..n];
        }
    }
}

// ---- Stream types ---------------------------------------------------------

#[derive(Debug, Clone)]
pub enum ChatDelta {
    Token(String),
    /// Reasoning trace from a chain-of-thought model (gpt-oss, DeepSeek-R1,
    /// QwQ, o1-like). Streamed separately from final answer tokens.
    Reasoning(String),
    ToolCall(ToolCallAccum),
    Done { reason: String },
    Error(String),
}

#[derive(Debug, Clone, Default)]
pub struct ToolCallAccum {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: String,
}

pub struct StreamHandle {
    pub rx: mpsc::UnboundedReceiver<ChatDelta>,
    pub cancel: CancellationToken,
}

pub async fn open(
    request: reqwest::RequestBuilder,
    cancel: CancellationToken,
) -> Result<StreamHandle> {
    // Honor cancellation while the initial HTTP send is in flight.
    // Without this select, a stalled LLM (TCP connect succeeded but
    // headers never arrive) leaves `request.send().await` blocked
    // forever — the Stop button's `cancel.cancel()` would fire,
    // pump() would never get to see it, and the user is stuck with
    // the "typing…" indicator until they kill the process. The
    // select races the request against the token; first wins.
    let resp = tokio::select! {
        _ = cancel.cancelled() => {
            return Err(anyhow!("cancelled before LLM responded"));
        }
        result = request.send() => result?,
    };
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("LLM error {status}: {body}"));
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel_for_task = cancel.clone();

    tokio::spawn(async move {
        if let Err(e) = pump(resp, tx.clone(), cancel_for_task).await {
            let _ = tx.send(ChatDelta::Error(e.to_string()));
        }
    });

    Ok(StreamHandle { rx, cancel })
}

// ---- Pump (SSE → ChatDelta) -----------------------------------------------

async fn pump(
    resp: reqwest::Response,
    tx: mpsc::UnboundedSender<ChatDelta>,
    cancel: CancellationToken,
) -> Result<()> {
    use eventsource_stream::Eventsource;
    let mut events = resp.bytes_stream().eventsource();
    let mut openai_tool_buf: Vec<ToolCallAccum> = Vec::new();
    let mut harmony = HarmonyParser::default();
    let mut saw_final_or_default_token = false;
    let mut saw_thought_only = false;
    let mut harmony_tool_index: usize = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = tx.send(ChatDelta::Done { reason: "cancelled".into() });
                return Ok(());
            }
            next = events.next() => {
                let Some(item) = next else {
                    let _ = tx.send(ChatDelta::Done { reason: "stream-end".into() });
                    return Ok(());
                };
                let event = item.map_err(|e| anyhow!("sse: {e}"))?;
                if event.data == "[DONE]" {
                    finalize(&mut harmony, &tx, &mut saw_final_or_default_token, &mut saw_thought_only, &mut harmony_tool_index);
                    for tc in openai_tool_buf.drain(..) {
                        let _ = tx.send(ChatDelta::ToolCall(tc));
                    }
                    let _ = tx.send(ChatDelta::Done { reason: "done".into() });
                    return Ok(());
                }
                let parsed: StreamChunk = match serde_json::from_str(&event.data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("bad sse chunk: {e} :: {}", event.data);
                        continue;
                    }
                };
                for choice in parsed.choices {
                    if let Some(content) = choice.delta.content {
                        if !content.is_empty() {
                            for emit in harmony.feed(&content) {
                                dispatch_emit(emit, &tx, &mut saw_final_or_default_token, &mut saw_thought_only, &mut harmony_tool_index);
                            }
                        }
                    }
                    if let Some(reasoning) = choice.delta.reasoning {
                        if !reasoning.is_empty() {
                            saw_thought_only = true;
                            let _ = tx.send(ChatDelta::Reasoning(reasoning));
                        }
                    }
                    if let Some(tcs) = choice.delta.tool_calls {
                        for tc in tcs {
                            let idx = tc.index.unwrap_or(0);
                            while openai_tool_buf.len() <= idx {
                                openai_tool_buf.push(ToolCallAccum::default());
                            }
                            let slot = &mut openai_tool_buf[idx];
                            slot.index = idx;
                            if let Some(id) = tc.id { slot.id = Some(id); }
                            if let Some(func) = tc.function {
                                if let Some(n) = func.name { slot.name = Some(n); }
                                if let Some(a) = func.arguments { slot.arguments.push_str(&a); }
                            }
                        }
                    }
                    if let Some(reason) = choice.finish_reason {
                        finalize(&mut harmony, &tx, &mut saw_final_or_default_token, &mut saw_thought_only, &mut harmony_tool_index);
                        for tc in openai_tool_buf.drain(..) {
                            let _ = tx.send(ChatDelta::ToolCall(tc));
                        }
                        let _ = tx.send(ChatDelta::Done { reason });
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn dispatch_emit(
    emit: HarmonyEmit,
    tx: &mpsc::UnboundedSender<ChatDelta>,
    saw_final: &mut bool,
    saw_thought: &mut bool,
    tool_index: &mut usize,
) {
    match emit {
        HarmonyEmit::Visible(t) => {
            *saw_final = true;
            let _ = tx.send(ChatDelta::Token(t));
        }
        HarmonyEmit::Reasoning(r) => {
            *saw_thought = true;
            let _ = tx.send(ChatDelta::Reasoning(r));
        }
        HarmonyEmit::ToolCall { name, arguments } => {
            let acc = ToolCallAccum {
                index: *tool_index,
                id: Some(format!("call_{}", uuid::Uuid::new_v4())),
                name: Some(name),
                arguments,
            };
            *tool_index += 1;
            *saw_final = true; // a tool call counts as productive output
            let _ = tx.send(ChatDelta::ToolCall(acc));
        }
    }
}

fn finalize(
    harmony: &mut HarmonyParser,
    tx: &mpsc::UnboundedSender<ChatDelta>,
    saw_final: &mut bool,
    saw_thought: &mut bool,
    tool_index: &mut usize,
) {
    for emit in harmony.flush() {
        dispatch_emit(emit, tx, saw_final, saw_thought, tool_index);
    }
    // NOTE: the empty-answer diagnostic used to live here, but it could fire
    // multiple times per user turn (once per LLM round). It now belongs to
    // loop_pipeline.rs, which has full turn context and can decide to emit
    // the note at most once, only if no visible content arrived from any
    // round.
}

// ---- SSE chunk shape ------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    /// Chain-of-thought trace emitted by reasoning models when the
    /// server surfaces it as a separate field (not inline in content).
    ///
    /// Field-name zoo across backends — we accept all of them via serde
    /// aliases so the reasoning panel lights up regardless of which
    /// server the host points a slot at:
    ///   * `reasoning`          — vLLM / some OpenAI-compatible servers
    ///   * `reasoning_content`  — llama.cpp, DeepSeek-R1, Qwen3 (this is
    ///     what the deep slot's Qwen3.6-35B emits — without the alias
    ///     the entire multi-second thinking phase was silently dropped
    ///     and the user saw dead air under the thinking-dots until the
    ///     final answer finally arrived in `content`)
    #[serde(default, alias = "reasoning_content")]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCall {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamToolFn>,
}

#[derive(Debug, Deserialize)]
struct StreamToolFn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

impl ToolCallAccum {
    pub fn into_finished(self) -> Option<ToolCallOut> {
        let name = self.name?;
        Some(ToolCallOut {
            id: self.id,
            function: super::ToolCallOutFn {
                name,
                arguments: if self.arguments.is_empty() {
                    "{}".into()
                } else {
                    self.arguments
                },
            },
        })
    }
}

#[cfg(test)]
mod reasoning_field_tests {
    use super::*;

    /// The deep slot's Qwen3.6-35B (via llama.cpp) streams its
    /// chain-of-thought in a `reasoning_content` delta field, while
    /// vLLM-style servers use `reasoning`. KinAI's StreamDelta must
    /// accept BOTH or the entire thinking phase gets silently dropped
    /// (serde ignores unknown fields), leaving the user staring at
    /// dead air under the thinking-dots. This was the v0.2.46 "deep
    /// model doesn't work" bug. Guard both spellings.
    #[test]
    fn parses_reasoning_content_alias() {
        let json = r#"{"delta":{"reasoning_content":"thinking..."},"finish_reason":null}"#;
        let choice: StreamChoice = serde_json::from_str(json).unwrap();
        assert_eq!(choice.delta.reasoning.as_deref(), Some("thinking..."));
    }

    #[test]
    fn parses_reasoning_canonical() {
        let json = r#"{"delta":{"reasoning":"thinking..."},"finish_reason":null}"#;
        let choice: StreamChoice = serde_json::from_str(json).unwrap();
        assert_eq!(choice.delta.reasoning.as_deref(), Some("thinking..."));
    }

    #[test]
    fn parses_final_content() {
        let json = r#"{"delta":{"content":"Hi"},"finish_reason":"stop"}"#;
        let choice: StreamChoice = serde_json::from_str(json).unwrap();
        assert_eq!(choice.delta.content.as_deref(), Some("Hi"));
        assert_eq!(choice.finish_reason.as_deref(), Some("stop"));
    }
}
