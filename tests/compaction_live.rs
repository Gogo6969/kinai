//! Live: conversation compaction against the REAL fast-slot model.
//!
//! A long invented thread whose very first message holds a fact the rest
//! never repeats. Without compaction that message has scrolled out of the
//! history cap and the model cannot know it; after one fold the thread's
//! digest carries it, and the model answers from the digest.
//!
//! Uses a throwaway database — never the family's — and the fast slot
//! from the host's config. Run on the build host:
//!   cargo test --test compaction_live -- --ignored --nocapture
use std::time::Instant;

use kinai::config::AppConfig;
use kinai::context::builder::build_context;
use kinai::db::Db;
use kinai::llm::LlmClient;

const TOPICS: &[&str] = &[
    "bread dough that will not rise in a cold kitchen",
    "choosing a tent for a rainy long weekend",
    "why the tomato seedlings are turning yellow",
    "a birthday quiz for ten-year-olds",
    "fixing a squeaky bicycle chain",
    "planning a picnic menu for twelve people",
    "how tides work at a small harbour",
    "keeping a sourdough starter alive on holiday",
    "painting a garden fence in autumn",
    "teaching a dog to stop pulling on the lead",
    "packing a first-aid kit for hiking",
    "the difference between baking soda and baking powder",
];

fn filler(topic: &str, turn: usize, words: usize) -> String {
    let base = format!(
        "On the question about {topic}: consider the conditions, the timing and the tools \
         involved, and compare what usually works with what went wrong last time (note {turn}). "
    );
    let mut s = String::new();
    while s.split_whitespace().count() < words {
        s.push_str(&base);
    }
    s
}

#[tokio::test]
#[ignore = "live: needs the fast-slot model server"]
async fn a_fold_keeps_an_early_fact_the_window_had_lost() {
    let cfg = AppConfig::load_or_default();
    let llm = cfg.llm.clone();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("compaction.db")).await.unwrap();
    let t = db.create_thread("host", Some("compaction live")).await.unwrap();

    db.append_message(
        &t.id,
        "user",
        "Alex",
        "Quick note before the questions: our sailing dinghy is called Blue Heron and it is \
         moored at pier 7. Now, first topic.",
        &[],
    )
    .await
    .unwrap();
    db.append_message(&t.id, "assistant", "KinAI", "Got it. Go ahead.", &[])
        .await
        .unwrap();
    for (i, topic) in TOPICS.iter().cycle().take(30).enumerate() {
        db.append_message(&t.id, "user", "Alex", &format!("Tell me about {topic}."), &[])
            .await
            .unwrap();
        db.append_message(&t.id, "assistant", "KinAI", &filler(topic, i, 420), &[])
            .await
            .unwrap();
    }

    let question = "What is our dinghy called, and where is it moored? Answer in one sentence.";
    let ask = db.append_message(&t.id, "user", "Alex", question, &[]).await.unwrap();
    let before = build_context(&db, &cfg, &llm, "host", &t.id, &ask).await.unwrap();
    let before_text: String = before.iter().map(|m| m.content()).collect::<Vec<_>>().join("\n");
    println!("before fold: {} messages in the prompt", before.len());
    assert!(
        !before_text.contains("Blue Heron"),
        "setup error: the early fact is still inside the history cap"
    );
    // The question itself is part of the thread; fold what came before it.
    db.delete_messages_from("host", &t.id, &ask.created_at).await.unwrap();

    let started = Instant::now();
    kinai::context::compaction::maybe_compact(&db, &llm, "host", &t.id).await;
    let fold_ms = started.elapsed().as_millis();
    let digest = db
        .thread_digest("host", &t.id)
        .await
        .unwrap()
        .expect("the fold stored no digest — see the compaction WARN above");
    println!("fold took {fold_ms} ms on {}; digest ({} chars):\n{}", llm.model, digest.text.len(), digest.text);
    assert!(digest.text.contains("Blue Heron"), "the digest lost the early fact");

    let ask = db.append_message(&t.id, "user", "Alex", question, &[]).await.unwrap();
    let after = build_context(&db, &cfg, &llm, "host", &t.id, &ask).await.unwrap();
    println!("after fold: {} messages in the prompt", after.len());
    let reply = LlmClient::new(llm.clone())
        .complete_without_thinking(&after, &[], Some(400))
        .await
        .unwrap();
    println!("answer: {}", reply.content.trim());
    assert!(reply.content.contains("Blue Heron"), "the model did not recall the dinghy's name");
    assert!(reply.content.contains('7'), "the model did not recall the pier");
}
