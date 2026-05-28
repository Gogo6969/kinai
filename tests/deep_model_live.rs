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
//!   KINAI_DEEP_URL=http://192.168.1.91:8081 \
//!   KINAI_DEEP_MODEL=Qwen3.6-35B-A3B-MTP-UD-Q6_K.gguf \
//!   cargo test --test deep_model_live -- --ignored --nocapture
//!
//! Asserts that we receive at least one Reasoning delta (proving the
//! reasoning_content alias works), at least one visible Token delta
//! (the final answer), and a clean Done.

use kinai::config::LlmSettings;
use kinai::context::ChatMessage;
use kinai::llm::{ChatDelta, LlmClient};
use tokio_util::sync::CancellationToken;

#[tokio::test]
#[ignore = "needs a live llama.cpp deep server; run with --ignored"]
async fn deep_model_streams_reasoning_and_answer() {
    let base_url =
        std::env::var("KINAI_DEEP_URL").unwrap_or_else(|_| "http://192.168.1.91:8081".into());
    let model = std::env::var("KINAI_DEEP_MODEL")
        .unwrap_or_else(|_| "Qwen3.6-35B-A3B-MTP-UD-Q6_K.gguf".into());

    let settings = LlmSettings {
        provider: "llamacpp".into(),
        base_url,
        model,
        context_window: 32768,
        api_key: None,
        temperature: 0.7,
        max_tokens: 0,
        system_addendum: String::new(),
        enabled: true,
    };

    let client = LlmClient::new(settings);
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
