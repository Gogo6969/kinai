//! Regression test for the generation-budget collapse that made the
//! deep (reasoning) model go silent.
//!
//! Bug: build_context trimmed the prompt to `context_window - max_tokens`.
//! With max_tokens = 0 (auto), that's the FULL window — so a thread with
//! enough history packed the prompt right up to the context limit,
//! leaving ~0 tokens for generation. compute_max_tokens then floored at
//! 256. A reasoning model spends 256 tokens on <think> and emits nothing
//! visible → "thinks for a second then goes silent."
//!
//! Fix: build_context now reserves AUTO_GENERATION_RESERVE (8192) tokens
//! for generation when max_tokens is auto. This test stuffs a thread with
//! far more history than fits, runs the real build_context, and asserts
//! the resulting prompt leaves a usable generation budget — i.e. the
//! reasoning model would have room to think AND answer.
//!
//! Unlike deep_model_live.rs this needs no network — it exercises the
//! pure prompt-assembly + trim path against a temp SQLite DB.

use kinai::config::AppConfig;
use kinai::context::builder::build_context;
use kinai::context::token_guard::estimate_messages;
use kinai::db::{Db, Message, HOST_PEER};

#[tokio::test]
async fn build_context_reserves_generation_headroom() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(tmp.path().join("test.db")).await.unwrap();

    let thread = db
        .create_thread(HOST_PEER, Some("budget-test"))
        .await
        .unwrap();

    // Insert ~60 chunky messages — far more than a 32k window holds, so
    // trim_to_fit definitely has to cut. Each ~1500 chars ≈ ~400 tokens;
    // 60 × ~400 ≈ 24k tokens of history alone, plus we'll make them
    // bigger to be sure we blow past the window.
    let chunk = "The quick brown fox jumps over the lazy dog. ".repeat(120); // ~5400 chars ≈ ~1300 tokens
    for i in 0..60 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        db.append_message(&thread.id, role, "tester", &chunk, &[])
            .await
            .unwrap();
    }

    // A 32768-token window, auto max_tokens (the reasoning-model default).
    let mut cfg = AppConfig::default();
    cfg.llm.context_window = 32768;
    cfg.llm.max_tokens = 0;

    let new_message = Message {
        id: "new-turn".into(),
        thread_id: thread.id.clone(),
        role: "user".into(),
        sender: "tester".into(),
        content: "Summarize our conversation.".into(),
        attachments: vec![],
        created_at: chrono::Utc::now().to_rfc3339(),
        summarized_into: None,
        metrics: None,
    };

    let messages = build_context(&db, &cfg, HOST_PEER, &thread.id, &new_message)
        .await
        .unwrap();

    let prompt_tokens = estimate_messages(&messages);
    eprintln!("prompt tokens after build_context: {prompt_tokens}");
    eprintln!("context window: {}", cfg.llm.context_window);

    // The whole point: the prompt must NOT fill the window. With the
    // half-window reserve on a 32768 window, the prompt is capped near
    // 16384, so generation headroom should be ~16000+. Pre-fix this was
    // ~0; the v0.2.49 flat-8192 reserve gave ~8000 (still too little for
    // a reasoning model over a long context); v0.2.50's half-window
    // reserve gives ~16000.
    let headroom = cfg.llm.context_window.saturating_sub(prompt_tokens);
    eprintln!("generation headroom: {headroom}");
    assert!(
        headroom >= 15000,
        "build_context must leave a reasoning-model-sized generation \
         headroom — got only {headroom} tokens (prompt={prompt_tokens}). \
         A long thread on /deep needs room to think AND answer."
    );

    // And it must actually have trimmed (proving the test really
    // exercised the over-full path, not a trivially-small prompt).
    assert!(
        prompt_tokens > 10000,
        "expected the history to be large enough to exercise trimming; \
         got {prompt_tokens} — the test fixture is too small to be meaningful"
    );
}
