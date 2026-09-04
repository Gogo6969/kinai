//! The latch, and the fix for it, measured through the REAL pipeline.
//!
//! Field report (2026-07-29): "the balanced model keeps forgetting that it
//! has tool access… I ask for the Bitcoin price and it says it has no
//! real-time data. Then I tell it that it can look it up and it does. Then
//! it remembers for a session but forgets again the next session."
//!
//! It is not forgetting. With one capability refusal in the conversation,
//! `Laguna-XS-2.1` searched 0/8 — against 8/8 with a clean history and 8/8
//! after a good answer, same system prompt throughout. The model copies its
//! own previous turn, so a single bad answer poisons every turn after it,
//! and the user's correction is what breaks the spell.
//!
//! `force_search` takes the decision away from the model for questions
//! that plainly need live data. This test proves it on the exact history
//! that produced 0/8, and guards the two ways the fix could rot: the
//! classifier ceasing to fire, and a backend quietly ignoring `required`.
//!
//! #[ignore] — needs the live balanced server; ~24 completions.

use kinai::config::AppConfig;
use kinai::context::{system_prompt, ChatMessage};
use kinai::llm::LlmClient;
use kinai::tools::loop_pipeline::{run_pipeline, PipelineHandlers, ToolEvent};
use kinai::tools::registry::{self, ToolRuntime};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Verbatim from the host DB, 2026-07-29 12:24 — the reply that latched it.
const REFUSAL: &str = "I don’t have live access to current events, and my training data only \
goes up to 2024. For the very latest information, please check a news site directly.";

fn user(s: &str) -> ChatMessage {
    ChatMessage::User { content: s.into(), name: Some("Wolf".into()), image_data_urls: vec![] }
}

/// Runs one turn, returns (searched, answer).
async fn turn(cfg: &AppConfig, history: Vec<ChatMessage>) -> (bool, String) {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let handlers = PipelineHandlers {
        on_token: Arc::new(|_| {}),
        on_reasoning: Arc::new(|_| {}),
        on_tool: Arc::new(move |e: ToolEvent| {
            if let ToolEvent::Started { name, .. } = e {
                if name == "web_search" {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
            }
        }),
    };
    let out = run_pipeline(
        LlmClient::new(cfg.llm_balanced.clone()),
        history,
        registry::enabled(&cfg.tools),
        Some(1024),
        ToolRuntime::from_tool_settings(&cfg.tools),
        handlers,
        CancellationToken::new(),
    )
    .await
    .expect("pipeline");
    (calls.load(Ordering::SeqCst) > 0, out.final_content)
}

#[tokio::test]
#[ignore = "live server + real config; ~24 completions"]
async fn a_poisoned_conversation_still_searches() {
    let cfg = AppConfig::load_or_default();
    assert!(cfg.llm_balanced.is_active(), "balanced slot must be configured");

    const N: usize = 12;
    let mut searched = 0usize;
    let mut denials = Vec::new();

    for i in 0..N {
        let history = vec![
            system_prompt(&cfg.host.family_name, "", ""),
            user("What's the earthquake in Japan?"),
            ChatMessage::Assistant { content: REFUSAL.into(), tool_calls: vec![] },
            user("What is the Bitcoin price rn?"),
        ];
        let (did, answer) = turn(&cfg, history).await;
        if did {
            searched += 1;
        } else if kinai::tools::force_search::is_capability_denial(&answer) {
            denials.push(answer.chars().take(120).collect::<String>());
        }
        eprintln!("run {i}: searched={did}");
    }

    eprintln!("poisoned history: searched {searched}/{N}, {} denials", denials.len());
    for d in &denials {
        eprintln!("  denial: {d}");
    }
    // Pre-fix this was 0/8. Anything below a clear majority means the
    // forcing path has stopped working — most likely a backend that no
    // longer honours `tool_choice: "required"`.
    assert!(
        searched >= 10,
        "only {searched}/{N} searched on a poisoned history (pre-fix baseline was 0/8) — \
the forced-search path is not holding"
    );
}

#[tokio::test]
#[ignore = "live server + real config; ~12 completions"]
async fn a_timeless_question_is_left_alone() {
    // The other direction: forcing must not fire for questions that don't
    // need the web, or every "what is 2+2" burns a search and drags in
    // irrelevant context.
    let cfg = AppConfig::load_or_default();
    const N: usize = 6;
    let mut searched = 0usize;
    for _ in 0..N {
        let history = vec![
            system_prompt(&cfg.host.family_name, "", ""),
            user("What is a mixture-of-experts language model? Answer in two sentences."),
        ];
        let (did, _) = turn(&cfg, history).await;
        if did {
            searched += 1;
        }
    }
    eprintln!("timeless question: searched {searched}/{N}");
    assert!(
        searched <= 2,
        "{searched}/{N} searches for a definitional question — the classifier is over-firing"
    );
}
