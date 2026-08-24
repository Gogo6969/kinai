//! Live integration test for the deep-model streaming path.
//!
//! Exercises KinAI's REAL pipeline — `LlmClient::stream` → `stream::open`
//! → `pump` (the SSE parser, including the `reasoning_content` serde
//! alias added in v0.2.47) — against an actual llama.cpp server. This is
//! the test the user (rightly) demanded before shipping: curling the
//! backend proves the SERVER works, but only running KinAI's own code
//! proves KINAI works.
//!
//! `#[ignore]` by default because it needs a live server. Run explicitly:
//!
//!   KINAI_DEEP_URL=http://127.0.0.1:8081 \
//!   KINAI_DEEP_MODEL=Qwen3.6-35B-A3B-MTP-UD-Q6_K.gguf \
//!   cargo test --test deep_model_live -- --ignored --nocapture
//!
//! Asserts that we receive at least one Reasoning delta (proving the
//! reasoning_content alias works), at least one visible Token delta
//! (the final answer), and a clean Done.

use kinai::config::{LlmSettings, ToolSettings};
use kinai::context::ChatMessage;
use kinai::llm::{ChatDelta, LlmClient};
use kinai::tools::loop_pipeline::{run_pipeline, PipelineHandlers, ToolEvent};
use kinai::tools::registry::{self, ToolRuntime};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio_util::sync::CancellationToken;

fn deep_settings() -> LlmSettings {
    LlmSettings {
        provider: "llamacpp".into(),
        base_url: std::env::var("KINAI_DEEP_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8081".into()),
        model: std::env::var("KINAI_DEEP_MODEL")
            .unwrap_or_else(|_| "Qwen3.6-35B-A3B-MTP-UD-Q6_K.gguf".into()),
        context_window: 32768,
        api_key: None,
        temperature: 0.7,
        max_tokens: 0,
        system_addendum: String::new(),
        enabled: true,
        image_recognition: "auto".into(),
    }
}

#[tokio::test]
#[ignore = "needs a live llama.cpp deep server; run with --ignored"]
async fn deep_model_streams_reasoning_and_answer() {
    let base_url =
        std::env::var("KINAI_DEEP_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
    let model = std::env::var("KINAI_DEEP_MODEL")
        .unwrap_or_else(|_| "Qwen3.6-35B-A3B-MTP-UD-Q6_K.gguf".into());

    let _ = (base_url, model); // settings come from deep_settings()
    let client = LlmClient::new(deep_settings());
    let messages = vec![ChatMessage::User {
        content: "Reply with exactly: DEEP OK".into(),
        name: None,
        image_data_urls: vec![],
    }];

    // max_tokens = Some(1024): a reasoning model needs headroom for the
    // <think> phase before it emits the final answer. None (auto) would
    // also work but Some keeps the test bounded.
    let mut handle = client
        .stream(&messages, &[], Some(1024), CancellationToken::new())
        .await
        .expect("stream open should succeed against a reachable deep server");

    let mut reasoning_chars = 0usize;
    let mut visible = String::new();
    let mut got_done = false;

    // Bound the whole collection so a hung server fails the test instead
    // of hanging CI forever.
    let collect = async {
        while let Some(delta) = handle.rx.recv().await {
            match delta {
                ChatDelta::Reasoning(r) => reasoning_chars += r.len(),
                ChatDelta::Token(t) => visible.push_str(&t),
                ChatDelta::Done { .. } => {
                    got_done = true;
                    break;
                }
                ChatDelta::Error(e) => panic!("stream errored: {e}"),
                _ => {}
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(120), collect)
        .await
        .expect("deep model should respond within 120s");

    eprintln!("reasoning chars: {reasoning_chars}");
    eprintln!("visible answer: {visible:?}");

    assert!(got_done, "stream must terminate with Done");
    assert!(
        reasoning_chars > 0,
        "expected reasoning deltas (reasoning_content alias) — got none; \
         the v0.2.47 serde alias is the thing under test"
    );
    assert!(
        !visible.trim().is_empty(),
        "expected a visible final answer in the content channel — got empty; \
         the deep turn would show as dead air / no reply in the UI"
    );
}

/// The REAL chat path: run_pipeline with the full enabled tool list and
/// tool_choice=auto, exactly as commands::send_message / the network
/// server do for a `/deep` turn. The isolated LlmClient::stream test
/// skips this — and the tool list + Qwen3's native tool-call format is
/// the most likely place a deep turn silently produces nothing in the
/// real app. This asserts the pipeline returns non-empty final content.
#[tokio::test]
#[ignore = "needs a live llama.cpp deep server; run with --ignored"]
async fn deep_model_pipeline_with_tools_returns_answer() {
    let client = LlmClient::new(deep_settings());

    // Full default tool set — web_search, calculator, datetime,
    // image_search, remember, forget — same as a real host turn.
    let tools = registry::enabled(&ToolSettings::default());
    eprintln!("tools sent: {}", tools.len());
    let runtime = ToolRuntime::from_tool_settings(&ToolSettings::default());

    let messages = vec![
        ChatMessage::System {
            content: "You are KinAI, a helpful family assistant.".into(),
        },
        ChatMessage::User {
            content: "What is 12 times 12? Answer in one short sentence.".into(),
            name: None,
            image_data_urls: vec![],
        },
    ];

    let token_count = Arc::new(AtomicUsize::new(0));
    let reasoning_count = Arc::new(AtomicUsize::new(0));
    let tc = token_count.clone();
    let rc = reasoning_count.clone();

    let handlers = PipelineHandlers {
        on_token: Arc::new(move |_t| {
            tc.fetch_add(1, Ordering::Relaxed);
        }),
        on_reasoning: Arc::new(move |_r| {
            rc.fetch_add(1, Ordering::Relaxed);
        }),
        on_tool: Arc::new(|e: ToolEvent| eprintln!("tool event: {e:?}")),
    };

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        run_pipeline(
            client,
            messages,
            tools,
            Some(2048),
            runtime,
            handlers,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("pipeline should finish within 180s")
    .expect("pipeline should not error");

    eprintln!("token deltas: {}", token_count.load(Ordering::Relaxed));
    eprintln!("reasoning deltas: {}", reasoning_count.load(Ordering::Relaxed));
    eprintln!("final_content: {:?}", result.final_content);

    assert!(
        !result.final_content.trim().is_empty(),
        "the real /deep pipeline returned EMPTY final content — this is \
         exactly the 'deep model shows nothing' bug. Tool list + Qwen3 \
         tool-call format is the prime suspect."
    );
}

/// Demonstrates the ROOT CAUSE of "deep model thinks for a second then
/// goes silent": a reasoning model handed a small max_tokens budget
/// burns it all on the <think> phase and emits EMPTY visible content
/// (finish_reason=length), whereas an adequate budget yields a real
/// answer. Same model, same prompt, opposite outcome — exactly why the
/// fast slot (gpt-oss, answers immediately) worked while the deep slot
/// (Qwen3, reasons first) didn't, once a long thread shrank the budget.
///
/// The fix lives in context::builder::build_context (reserve generation
/// headroom so the budget never collapses), but the symptom is a
/// property of the model + budget, proven here directly.
#[tokio::test]
#[ignore = "needs a live llama.cpp deep server; run with --ignored"]
async fn deep_model_starves_at_small_budget_succeeds_at_adequate() {
    let client = LlmClient::new(deep_settings());
    let messages = vec![ChatMessage::User {
        content: "What is 12 times 12? Answer in one short sentence.".into(),
        name: None,
        image_data_urls: vec![],
    }];

    // --- BUG REPRODUCTION: small budget (what the old build_context
    //     produced once history filled the window) → empty answer.
    let mut small = String::new();
    let mut h = client
        .stream(&messages, &[], Some(64), CancellationToken::new())
        .await
        .unwrap();
    while let Some(d) = h.rx.recv().await {
        match d {
            ChatDelta::Token(t) => small.push_str(&t),
            ChatDelta::Done { .. } => break,
            _ => {}
        }
    }
    eprintln!("small-budget visible answer: {small:?}");
    assert!(
        small.trim().is_empty(),
        "expected the reasoning model to starve at a 64-token budget \
         (all spent on <think>) — if this is non-empty the repro \
         assumptions changed"
    );

    // --- FIX VALIDATION: adequate budget (what the reserve guarantees)
    //     → real answer.
    let mut big = String::new();
    let mut h2 = client
        .stream(&messages, &[], Some(8064), CancellationToken::new())
        .await
        .unwrap();
    let collect = async {
        while let Some(d) = h2.rx.recv().await {
            match d {
                ChatDelta::Token(t) => big.push_str(&t),
                ChatDelta::Done { .. } => break,
                _ => {}
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(120), collect)
        .await
        .expect("should finish within 120s");
    eprintln!("adequate-budget visible answer: {big:?}");
    assert!(
        big.contains("144"),
        "with an adequate budget the deep model must produce the actual \
         answer (expected to contain '144'), got: {big:?}"
    );
}
