//! Live proof of the reminders protocol against the REAL running host, the
//! way a family device sees it: redeem an invite, say Hello with a time
//! zone, check the host advertises reminders, then round-trip a reminder
//! through list / snooze / acknowledge / delete — and, the part no unit
//! test can cover, wait for the host's scheduler to PUSH a due reminder
//! over the socket unprompted.
//!
//! #[ignore] — needs the host app running on this machine.
use futures_util::{SinkExt, StreamExt};
use kinai::config::AppConfig;
use kinai::network::protocol::Envelope;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

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

/// Wait for the first frame `pick` accepts, ignoring everything else.
async fn wait_for<S, T>(source: &mut S, secs: u64, mut pick: impl FnMut(Envelope) -> Option<T>) -> T
where
    S: StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let fut = async {
        while let Some(Ok(msg)) = source.next().await {
            if let WsMessage::Text(t) = msg {
                if let Ok(env) = serde_json::from_str::<Envelope>(&t) {
                    if let Some(v) = pick(env) {
                        return v;
                    }
                }
            }
        }
        panic!("socket closed while waiting");
    };
    tokio::time::timeout(Duration::from_secs(secs), fut)
        .await
        .unwrap_or_else(|_| panic!("nothing matched within {secs}s"))
}

#[tokio::test]
#[ignore = "live: requires the KinAI host app to be running"]
async fn a_family_device_can_list_act_on_and_receive_reminders() {
    let cfg = AppConfig::load_or_default();
    let db = open_db().await;
    let pool = open_pool().await;

    let invite = kinai::network::invite::create(&pool, &cfg, "reminders test", 1)
        .await
        .expect("create invite");
    let peer = invite.short_code.clone();

    let url = invite.host_url.replace("kinai://", "");
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("host must be running for this test");
    let (mut sink, mut source) = ws.split();
    let hello = Envelope::Hello {
        token: invite.jwt.clone(),
        display_name: "Reminder Test".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        tz: "Europe/Berlin".into(),
    };
    sink.send(WsMessage::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .expect("send hello");
    let advertised = wait_for(&mut source, 10, |e| match e {
        Envelope::Welcome { host_reminders, .. } => Some(host_reminders),
        _ => None,
    })
    .await;
    assert!(advertised, "host must advertise host_reminders");

    // The Hello's zone reached the peers table.
    let stored = db.peer_tz(&peer).await.unwrap();
    assert_eq!(stored.as_deref(), Some("Europe/Berlin"), "Hello.tz persisted");

    // A reminder the scheduler will pick up on its next tick.
    let due = chrono::Utc::now() - chrono::Duration::seconds(5);
    let r = db
        .create_reminder(&peer, None, "test: water the plants", due, "Europe/Berlin", None, "", "")
        .await
        .expect("create reminder");
    println!("\n[created] id={} due_local={} tz={}", &r.id[..8], r.due_local, r.tz);

    // 1. Unprompted push from the scheduler (30 s tick).
    let pushed = wait_for(&mut source, 75, |e| match e {
        Envelope::Reminder { reminder } if reminder.id == r.id => Some(reminder),
        _ => None,
    })
    .await;
    println!("[pushed] status={} fired_at={:?}", pushed.status, pushed.fired_at);
    assert_eq!(pushed.status, "fired");
    assert!(pushed.fired_at.is_some());

    // 2. List shows it as fired.
    sink.send(WsMessage::Text(serde_json::to_string(&Envelope::ListReminders).unwrap().into()))
        .await
        .unwrap();
    let items = wait_for(&mut source, 10, |e| match e {
        Envelope::Reminders { items } => Some(items),
        _ => None,
    })
    .await;
    let mine = items.iter().find(|x| x.id == r.id).expect("listed");
    assert_eq!(mine.status, "fired");

    // 3. Snooze 10 minutes → scheduled, due_local re-rendered.
    let act = |id: &str, action: &str, minutes: u32| Envelope::ReminderAction {
        id: id.to_string(),
        action: action.to_string(),
        snooze_minutes: minutes,
    };
    sink.send(WsMessage::Text(serde_json::to_string(&act(&r.id, "snooze", 10)).unwrap().into()))
        .await
        .unwrap();
    let (ok, msg, snoozed) = wait_for(&mut source, 15, |e| match e {
        Envelope::ReminderActionAck { id, ok, message, reminder } if id == r.id => {
            Some((ok, message, reminder))
        }
        _ => None,
    })
    .await;
    assert!(ok, "snooze refused: {msg}");
    let snoozed = snoozed.expect("snooze returns the row");
    assert_eq!(snoozed.status, "scheduled");
    assert_ne!(snoozed.due_local, r.due_local);
    println!("[snoozed] due_local={}", snoozed.due_local);

    // 4. Acknowledge → done.
    sink.send(WsMessage::Text(serde_json::to_string(&act(&r.id, "ack", 0)).unwrap().into()))
        .await
        .unwrap();
    let (ok, _, done) = wait_for(&mut source, 15, |e| match e {
        Envelope::ReminderActionAck { id, ok, message, reminder } if id == r.id => {
            Some((ok, message, reminder))
        }
        _ => None,
    })
    .await;
    assert!(ok);
    assert_eq!(done.expect("ack returns the row").status, "done");

    // 5. Delete → gone from the host.
    sink.send(WsMessage::Text(serde_json::to_string(&act(&r.id, "delete", 0)).unwrap().into()))
        .await
        .unwrap();
    let (ok, _, gone) = wait_for(&mut source, 15, |e| match e {
        Envelope::ReminderActionAck { id, ok, message, reminder } if id == r.id => {
            Some((ok, message, reminder))
        }
        _ => None,
    })
    .await;
    assert!(ok);
    assert!(gone.is_none());
    assert!(db.get_reminder(&peer, &r.id).await.unwrap().is_none(), "host row deleted");

    kinai::network::invite::revoke(&pool, &invite.id).await.ok();
}
