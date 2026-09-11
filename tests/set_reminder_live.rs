//! Live proof that asking for a reminder still works after the 0.2.124
//! extraction, spoken over the real client WebSocket the way a family
//! device does: redeem an invite, say Hello with a zone, ask in plain
//! words, and check what actually landed in the table.
//!
//! `tests/reminders_live.rs` covers the protocol around a reminder — list,
//! snooze, acknowledge, and the scheduler's unprompted push — but it
//! writes its row with `db.create_reminder`, which is exactly the path
//! that skips validation. Nothing exercised `set_reminder` itself against
//! a running host, so moving its guards into `reminders::spec` had no
//! end-to-end net under it. This is that net.
//!
//! What it proves that the unit tests cannot: the model reaches the tool,
//! the tool reaches `spec::plan`, the member's zone survives the trip, and
//! a row appears in the installed binary's own database.
//!
//! #[ignore] — needs the host app running on this machine, with at least
//! one model slot configured.
use futures_util::{SinkExt, StreamExt};
use kinai::config::AppConfig;
use kinai::network::protocol::Envelope;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const TZ: &str = "Europe/Berlin";

async fn open_db() -> kinai::db::Db {
    kinai::db::Db::open(AppConfig::config_dir().join("kinai.db"))
        .await
        .expect("open the host's live database")
}

async fn open_pool() -> sqlx::SqlitePool {
    let path = AppConfig::config_dir().join("kinai.db");
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("open pool on the host's live database")
}

async fn wait_done<S>(source: &mut S, client_msg_id: &str, secs: u64) -> String
where
    S: StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let fut = async {
        while let Some(Ok(msg)) = source.next().await {
            if let WsMessage::Text(t) = msg {
                if let Ok(Envelope::AssistantDone { client_msg_id: id, message, metrics }) =
                    serde_json::from_str::<Envelope>(&t)
                {
                    if id == client_msg_id {
                        println!(
                            "\n[reply] slot={} model={} total={}ms\n{}\n",
                            metrics.slot, metrics.model, metrics.total_ms, message.content
                        );
                        return message.content;
                    }
                }
            }
        }
        panic!("socket closed before AssistantDone for {client_msg_id}");
    };
    tokio::time::timeout(Duration::from_secs(secs), fut)
        .await
        .unwrap_or_else(|_| panic!("no AssistantDone for {client_msg_id} within {secs}s"))
}

#[tokio::test]
#[ignore = "live: requires the KinAI host app to be running"]
async fn asking_for_a_reminder_still_puts_one_in_the_table() {
    let cfg = AppConfig::load_or_default();
    let db = open_db().await;
    let pool = open_pool().await;

    let invite = kinai::network::invite::create(&pool, &cfg, "set_reminder live test", 1)
        .await
        .expect("create invite");
    let peer = invite.short_code.clone();
    let thread = db
        .create_thread(&peer, Some("set_reminder live"))
        .await
        .expect("create thread");

    let url = invite.host_url.replace("kinai://", "");
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("host must be running for this test");
    let (mut sink, mut source) = ws.split();
    sink.send(WsMessage::Text(
        serde_json::to_string(&Envelope::Hello {
            token: invite.jwt.clone(),
            display_name: "Reminder Test".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            tz: TZ.into(),
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

    let before = chrono::Utc::now();
    sink.send(WsMessage::Text(
        serde_json::to_string(&Envelope::SendMessage {
            thread_id: thread.id.clone(),
            content: "Remind me in 45 minutes to bring the washing in.".into(),
            sender: "Reminder Test".into(),
            client_msg_id: "rem-live-1".into(),
            attachments: vec![],
        })
        .unwrap()
        .into(),
    ))
    .await
    .expect("send message");

    let reply = wait_done(&mut source, "rem-live-1", 240).await;

    // The row is the proof, not the sentence — a model can claim anything.
    let rows = db.list_reminders(&peer).await.expect("list reminders");
    assert_eq!(rows.len(), 1, "exactly one reminder for this peer; reply was: {reply}");
    let r = &rows[0];
    println!("[row] id={} due_local={} tz={} status={}", &r.id[..8], r.due_local, r.tz, r.status);

    assert_eq!(r.status, "scheduled");
    assert!(!r.text.trim().is_empty(), "the reminder kept some text");
    assert!(
        r.text.chars().count() <= kinai::reminders::spec::MAX_TEXT_CHARS,
        "within the cap the spec enforces"
    );

    // The member's zone made it from Hello.tz all the way onto the row —
    // this is the argument `spec::plan` renders `due_local` with.
    assert_eq!(r.tz, TZ, "the reminder is stamped with the member's zone");

    // "45 minutes" should land roughly 45 minutes out. Generous window: the
    // point is that a future instant was stored and the past/ceiling guards
    // did not reject it, not that the model is a stopwatch.
    let due = chrono::DateTime::parse_from_rfc3339(&r.due_at)
        .expect("due_at is RFC3339")
        .with_timezone(&chrono::Utc);
    let delta = (due - before).num_minutes();
    assert!(
        (5..=180).contains(&delta),
        "due {delta} min from the ask — expected roughly 45; due_local={} reply={reply}",
        r.due_local
    );
    assert!(due > before, "never stored in the past");

    // Clean up: this is the family's real database.
    db.apply_reminder_action(&peer, &r.id, "delete", 0)
        .await
        .expect("delete the test reminder");
    assert!(db.list_reminders(&peer).await.unwrap().is_empty(), "cleaned up");
    db.delete_thread(&peer, &thread.id).await.ok();
    kinai::network::invite::revoke(&pool, &invite.id).await.ok();
}

/// The headline of 0.2.125, end to end: the model has to notice "every
/// weekday", choose the right value from a four-word enum, and the row has
/// to come out repeating. The unit tests prove the date arithmetic; only a
/// running host proves the model can reach it through the tool schema.
#[tokio::test]
#[ignore = "live: requires the KinAI host app to be running"]
async fn asking_for_a_repeating_reminder_stores_a_repeat() {
    let cfg = AppConfig::load_or_default();
    let db = open_db().await;
    let pool = open_pool().await;

    let invite = kinai::network::invite::create(&pool, &cfg, "repeat live test", 1)
        .await
        .expect("create invite");
    let peer = invite.short_code.clone();
    let thread = db.create_thread(&peer, Some("repeat live")).await.expect("thread");

    let url = invite.host_url.replace("kinai://", "");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.expect("host running");
    let (mut sink, mut source) = ws.split();
    sink.send(WsMessage::Text(
        serde_json::to_string(&Envelope::Hello {
            token: invite.jwt.clone(),
            display_name: "Repeat Test".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            tz: TZ.into(),
        })
        .unwrap()
        .into(),
    ))
    .await
    .expect("hello");
    while let Some(Ok(msg)) = source.next().await {
        if let WsMessage::Text(t) = msg {
            if let Ok(Envelope::Welcome { .. }) = serde_json::from_str::<Envelope>(&t) {
                break;
            }
        }
    }

    sink.send(WsMessage::Text(
        serde_json::to_string(&Envelope::SendMessage {
            thread_id: thread.id.clone(),
            content: "Remind me every weekday at 7:45am to check the calendar.".into(),
            sender: "Repeat Test".into(),
            client_msg_id: "rep-live-1".into(),
            attachments: vec![],
        })
        .unwrap()
        .into(),
    ))
    .await
    .expect("send");
    let reply = wait_done(&mut source, "rep-live-1", 240).await;

    let rows = db.list_reminders(&peer).await.expect("list");
    assert_eq!(rows.len(), 1, "one reminder; reply was: {reply}");
    let r = &rows[0];
    println!(
        "[row] repeat={:?} due_local={} occurrence_local={:?}",
        r.repeat, r.due_local, r.occurrence_local
    );

    // Which word the model picks is its judgement — "weekdays" is the
    // right one here, but a defensible "daily" should not fail a release.
    // What must hold is that it repeats at all, and that the value is one
    // the scheduler understands rather than something invented.
    assert!(!r.repeat.is_empty(), "the model must set a repeat; reply was: {reply}");
    assert!(
        matches!(r.repeat.as_str(), "daily" | "weekdays" | "weekly" | "monthly"),
        "unknown repeat {:?} would be dead data to the scheduler",
        r.repeat
    );
    assert_eq!(r.status, "scheduled");

    db.apply_reminder_action(&peer, &r.id, "delete", 0).await.expect("clean up");
    db.delete_thread(&peer, &thread.id).await.ok();
    kinai::network::invite::revoke(&pool, &invite.id).await.ok();
}
