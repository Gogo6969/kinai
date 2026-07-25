//! Tests for slash routing — specifically the bare `/fast` / `/deep`
//! mode switch that must NOT run an empty LLM turn.

use kinai::config::AppConfig;
use kinai::db::{Db, HOST_PEER};
use kinai::slash::route_for;

fn two_slot_cfg() -> AppConfig {
    let mut cfg = AppConfig::default();
    cfg.llm.provider = "vllm".into();
    cfg.llm.base_url = "http://fast.local:8000".into();
    cfg.llm.model = "fast-model".into();
    cfg.llm.enabled = true;
    cfg.llm_deep.provider = "llamacpp".into();
    cfg.llm_deep.base_url = "http://deep.local:8081".into();
    cfg.llm_deep.model = "deep-model".into();
    cfg.llm_deep.enabled = true;
    cfg
}

#[tokio::test]
async fn bare_deep_is_a_switch_not_an_empty_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(tmp.path().join("t.db")).await.unwrap();
    let thread = db.create_thread(HOST_PEER, Some("t")).await.unwrap();
    let cfg = two_slot_cfg();

    // Bare "/deep" — must flag as a mode switch, route to the deep slot,
    // and produce empty stripped content (no prompt to run).
    let r = route_for(&db, &cfg, HOST_PEER, &thread.id, "/deep").await;
    assert!(r.bare_switch, "bare /deep must be a mode switch");
    assert_eq!(r.slot_label, "deep");
    assert!(r.stripped_content.trim().is_empty());

    // The switch must be sticky: a following plain message routes deep.
    let r2 = route_for(&db, &cfg, HOST_PEER, &thread.id, "hello there").await;
    assert_eq!(r2.slot_label, "deep", "sticky slot should persist after bare /deep");
    assert!(!r2.bare_switch);

    // Bare "/fast" flips it back, also a switch.
    let r3 = route_for(&db, &cfg, HOST_PEER, &thread.id, "/fast").await;
    assert!(r3.bare_switch);
    assert_eq!(r3.slot_label, "fast");
}

#[tokio::test]
async fn deep_with_prompt_is_not_a_bare_switch() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(tmp.path().join("t.db")).await.unwrap();
    let thread = db.create_thread(HOST_PEER, Some("t")).await.unwrap();
    let cfg = two_slot_cfg();

    // "/deep test" carries a prompt — must run the LLM, not just switch.
    let r = route_for(&db, &cfg, HOST_PEER, &thread.id, "/deep test").await;
    assert!(!r.bare_switch, "/deep with a prompt is a normal turn");
    assert_eq!(r.slot_label, "deep");
    assert_eq!(r.stripped_content.trim(), "test");
}

#[tokio::test]
async fn switch_confirmation_names_the_model() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(tmp.path().join("t.db")).await.unwrap();
    let thread = db.create_thread(HOST_PEER, Some("t")).await.unwrap();
    let cfg = two_slot_cfg();

    let r = route_for(&db, &cfg, HOST_PEER, &thread.id, "/deep").await;
    let msg = kinai::slash::switch_confirmation(&cfg, &r, None);
    assert!(msg.contains("deep"), "confirmation should name the slot: {msg}");
    assert!(msg.contains("deep-model"), "confirmation should name the model: {msg}");
}

/// A client's thread delete must be PEER-SCOPED on the host: one family
/// member can never delete another's conversation, even knowing its id.
/// (The client-side bug that motivated this — deletes never reaching the
/// host at all — is covered by the mode-aware command; this guards the
/// blast radius of the new host-side handler.)
#[tokio::test]
async fn thread_delete_is_peer_scoped() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(tmp.path().join("t.db")).await.unwrap();

    let mine = db.create_thread("peer-a", Some("mine")).await.unwrap();
    let theirs = db.create_thread("peer-b", Some("theirs")).await.unwrap();

    // peer-a tries to delete peer-b's thread using the right id.
    db.delete_thread("peer-a", &theirs.id).await.unwrap();
    assert_eq!(
        db.list_threads("peer-b").await.unwrap().len(),
        1,
        "another peer's thread must survive"
    );

    // Deleting its own works.
    db.delete_thread("peer-a", &mine.id).await.unwrap();
    assert!(db.list_threads("peer-a").await.unwrap().is_empty());

    // Rename is scoped the same way.
    db.rename_thread("peer-a", &theirs.id, "hijacked").await.unwrap();
    assert_eq!(db.list_threads("peer-b").await.unwrap()[0].title, "theirs");
}
