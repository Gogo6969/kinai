//! Does the model actually SEARCH when asked for a link?
//!
//! The 2026-07-27 field report: asked "Olares One ethernet speed - with
//! link", the balanced model ran no search at all. It lifted the URL from an
//! earlier reply in the same thread and presented it as the official spec
//! sheet — turning that reply's hedge ("typically 2.5GbE or 1GbE") into a
//! stated fact. Measured on the pre-fix prompt, this happened on 7 of 15
//! turns, and produced a flatly wrong "1 GbE" answer.
//!
//! Prompt behaviour is statistical, so this samples rather than asserting
//! once. It is the regression gate for edits to the tool-discipline rules in
//! `context::system_prompt` — including the lesson that cost a round here:
//! a rule that QUOTES the sentence you don't want ("never say 'I can't
//! browse'") hands the model that sentence to copy. State it positively.
//!
//! #[ignore] — needs the live balanced server; ~15 completions.

use kinai::config::AppConfig;
use kinai::context::{system_prompt, ChatMessage};
use kinai::llm::LlmClient;
use kinai::tools::loop_pipeline::{run_pipeline, PipelineHandlers, ToolEvent};
use kinai::tools::registry::{self, ToolRuntime};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// An earlier assistant reply that already carries the link AND a hedge —
/// the exact bait the model used to swallow instead of searching.
const PRIOR_REPLY: &str = "The exact Ethernet (LAN) speed for the Olares One is detailed \
inside the Hardware Overview table on their official technical specifications page:\n\
https://www.olares.com/docs/one/spec\n\nWhile the search preview didn't capture the \
specific number from that page, mini-PCs in this performance tier are typically equipped \
with a 2.5GbE or 1GbE LAN port.";

const RUNS: usize = 10;
/// Pre-fix behaviour was 8/15 (53%); post-fix 14/15 (93%). Gate well below
/// the fix but far above the regression, so this fails loudly if a prompt
/// edit undoes the discipline without tripping on ordinary sampling noise.
const MIN_SEARCHES: usize = 7;

#[tokio::test]
#[ignore = "live server + real config; ~10 completions"]
async fn asking_for_a_link_triggers_a_real_search() {
    let cfg = AppConfig::load_or_default();
    assert!(cfg.llm_balanced.is_active(), "balanced slot must be configured");

    let mut searched = 0usize;
    let mut denials = Vec::new();

    for run in 0..RUNS {
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
        let messages = vec![
            system_prompt(&cfg.host.family_name, "", ""),
            ChatMessage::User {
                content: "Olares One ethernet speed - with link".into(),
                name: Some("Wolf".into()),
                image_data_urls: vec![],
            },
            ChatMessage::Assistant {
                content: PRIOR_REPLY.into(),
                tool_calls: vec![],
            },
            ChatMessage::User {
                content: "Olares One ethernet speed - with link".into(),
                name: Some("Wolf".into()),
                image_data_urls: vec![],
            },
        ];
        let out = run_pipeline(
            LlmClient::new(cfg.llm_balanced.clone()),
            messages,
            registry::enabled(&cfg.tools),
            Some(1024),
            ToolRuntime::from_tool_settings(&cfg.tools),
            handlers,
            CancellationToken::new(),
        )
        .await
        .expect("pipeline");

        if calls.load(Ordering::SeqCst) > 0 {
            searched += 1;
        } else {
            let a = out.final_content.to_lowercase();
            // Catch the capability denial in all the shapes seen in the
            // field. The first version of this list missed "the ability to
            // browse the web" and scored a bad arm as clean.
            if ["browse the web", "live web access", "live web browser", "browsing",
                "access to the internet", "fetch a link"]
                .iter()
                .any(|p| a.contains(p))
            {
                denials.push(out.final_content.chars().take(140).collect::<String>());
            }
        }
        eprintln!("run {run}: searched={}", calls.load(Ordering::SeqCst) > 0);
    }

    eprintln!("searched {searched}/{RUNS}; {} capability denials", denials.len());
    for d in &denials {
        eprintln!("  denial: {d}");
    }
    assert!(
        searched >= MIN_SEARCHES,
        "only {searched}/{RUNS} turns searched (need >= {MIN_SEARCHES}); the model is \
answering link requests from conversation history again"
    );
}
