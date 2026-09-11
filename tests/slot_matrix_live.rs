//! The release smoke matrix: one control question and one live-data
//! question on EVERY configured slot, against the running host.
//!
//! `docs/RELEASE-CHECKLIST.md` step 3 requires this before a release
//! reaches family devices, and the reason is 0.2.98: `/online` shipped
//! broken because the forced-search round sends `tool_choice: "required"`,
//! which llama.cpp and vLLM honour and DeepSeek's thinking mode rejects
//! with a 400. Every live-data question on that slot failed, and testing
//! `fast` alone could never have found it — the slots run against
//! different providers, so one slot answering proves nothing about
//! another. That is why the live-data question is asked separately: it
//! forces a web search, and forcing is the part providers disagree about.
//!
//! This replaces the out-of-tree `halluctest` harness, which lived in a
//! scratch directory under /tmp and was lost with its source when the
//! machine rebooted. Being a repo test, this one cannot evaporate.
//!
//! Run it against the installed app after `deploy.sh`, inside the single
//! restart the checklist budgets:
//!
//!   cargo test --test slot_matrix_live -- --ignored --nocapture
//!
//! ONE THREAD PER SLOT. Sending all eight questions into a single
//! conversation makes every later turn re-process every earlier question
//! and answer — including the long search results the live-data question
//! produces — with no cache. On a slot that is merely slow per token that
//! turns a one-word reply into minutes: measured here at 342 SECONDS for
//! "what colour is a ripe lime?" on `deep`, against 0.73s when the same
//! server is asked directly. The matrix then reports a silence it cannot
//! tell apart from a fault. A fresh thread per slot keeps every prompt
//! small and measures the slot rather than the transcript.
//!
//! It PRINTS the matrix and asserts only that every configured slot
//! answered something. Judge the answers yourself — in particular do not
//! read `online` saying a ripe lime is "Yellow" as a regression. A fully
//! ripe lime does turn yellow, and DeepSeek alternates between the two.
//!
//! #[ignore] — needs the host app running on this machine.
use futures_util::{SinkExt, StreamExt};
use kinai::config::AppConfig;
use kinai::network::protocol::Envelope;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const SLOTS: [&str; 4] = ["fast", "balanced", "deep", "online"];
const CONTROL: &str = "Answer with one word: what colour is a ripe lime?";
const LIVE_DATA: &str = "Which stock is the largest holding of the SPY fund today?";
/// Balanced on a cold Minisforum server has taken ~3 minutes for one
/// forced-search round, and deep has taken twelve. Slow is not broken;
/// give it room. Override with KINAI_TURN_TIMEOUT when a slot is known to
/// be crawling and you want the matrix to wait it out rather than report
/// a silence it cannot distinguish from a fault.
fn turn_timeout_secs() -> u64 {
    std::env::var("KINAI_TURN_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

struct Answer {
    slot: String,
    model: String,
    total_ms: u64,
    content: String,
}

async fn wait_done<S>(source: &mut S, client_msg_id: &str, secs: u64) -> Option<Answer>
where
    S: StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let fut = async {
        while let Some(Ok(msg)) = source.next().await {
            if let WsMessage::Text(t) = msg {
                match serde_json::from_str::<Envelope>(&t) {
                    Ok(Envelope::AssistantDone { client_msg_id: id, message, metrics })
                        if id == client_msg_id =>
                    {
                        return Some(Answer {
                            slot: metrics.slot.clone(),
                            model: metrics.model.clone(),
                            total_ms: metrics.total_ms as u64,
                            content: message.content,
                        });
                    }
                    // A slot with no model server answers with an Error
                    // frame rather than a turn; report it, do not hang.
                    Ok(Envelope::Error { message }) => {
                        println!("  [error frame] {message}");
                        return None;
                    }
                    _ => {}
                }
            }
        }
        None
    };
    tokio::time::timeout(Duration::from_secs(secs), fut).await.unwrap_or(None)
}

#[tokio::test]
#[ignore = "live: requires the KinAI host app to be running"]
async fn every_configured_slot_answers_a_control_and_a_live_data_question() {
    let cfg = AppConfig::load_or_default();
    let path = AppConfig::config_dir().join("kinai.db");
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("open pool on the host's live database");
    let db = kinai::db::Db::open(path).await.expect("open the host's live database");

    let invite = kinai::network::invite::create(&pool, &cfg, "slot matrix smoke", 1)
        .await
        .expect("create invite");
    let peer = invite.short_code.clone();

    let url = invite.host_url.replace("kinai://", "");
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("host must be running for this test");
    let (mut sink, mut source) = ws.split();
    sink.send(WsMessage::Text(
        serde_json::to_string(&Envelope::Hello {
            token: invite.jwt.clone(),
            display_name: "Slot Matrix".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            tz: String::new(),
        })
        .unwrap()
        .into(),
    ))
    .await
    .expect("send hello");
    let mut welcomed = false;
    while let Some(Ok(msg)) = source.next().await {
        if let WsMessage::Text(t) = msg {
            if let Ok(Envelope::Welcome { .. }) = serde_json::from_str::<Envelope>(&t) {
                welcomed = true;
                break;
            }
        }
    }
    assert!(welcomed, "no Welcome");

    let mut rows: Vec<(String, String, String, String, u64)> = Vec::new();
    let mut silent: Vec<String> = Vec::new();

    let mut threads: Vec<String> = Vec::new();
    for slot in SLOTS {
        // Fresh conversation per slot — see the note at the top of this
        // file. This is what keeps the prompt small enough that the
        // number below measures the slot and not the backlog.
        let thread = db
            .create_thread(&peer, Some(&format!("slot matrix {slot}")))
            .await
            .expect("create thread");
        threads.push(thread.id.clone());
        for (kind, question) in [("control", CONTROL), ("live-data", LIVE_DATA)] {
            let id = format!("matrix-{slot}-{kind}");
            let content = format!("/{slot} {question}");
            println!("\n--- /{slot} [{kind}] ---");
            sink.send(WsMessage::Text(
                serde_json::to_string(&Envelope::SendMessage {
                    thread_id: thread.id.clone(),
                    content,
                    sender: "Slot Matrix".into(),
                    client_msg_id: id.clone(),
                    attachments: vec![],
                })
                .unwrap()
                .into(),
            ))
            .await
            .expect("send message");

            match wait_done(&mut source, &id, turn_timeout_secs()).await {
                Some(a) => {
                    let one_line: String =
                        a.content.trim().replace('\n', " ").chars().take(110).collect();
                    println!("  slot={} model={} {}ms\n  {}", a.slot, a.model, a.total_ms, one_line);
                    assert!(
                        !a.content.trim().is_empty(),
                        "/{slot} [{kind}] answered with an empty message"
                    );
                    rows.push((
                        slot.to_string(),
                        kind.to_string(),
                        a.model,
                        one_line,
                        a.total_ms,
                    ));
                }
                None => {
                    println!("  NO ANSWER within {}s", turn_timeout_secs());
                    silent.push(format!("/{slot} [{kind}]"));
                }
            }
        }
    }

    println!("\n================ SLOT MATRIX ================");
    for (slot, kind, model, line, ms) in &rows {
        println!("/{slot:<9} {kind:<10} {ms:>7}ms  {model}\n              {line}");
    }
    if !silent.is_empty() {
        println!("\nSILENT: {}", silent.join(", "));
    }
    println!("============================================\n");

    // Clean up before asserting, so a failure still leaves the family's
    // database tidy.
    for t in &threads {
        db.delete_thread(&peer, t).await.ok();
    }
    kinai::network::invite::revoke(&pool, &invite.id).await.ok();

    assert!(
        silent.is_empty(),
        "these slot/question pairs never answered: {} — a slot that is genuinely \
         unconfigured should be removed from SLOTS, not left silent",
        silent.join(", ")
    );
}
