//! End-to-end proof for the client→host thread operations (0.2.86).
//!
//! Talks to the REAL running host over the real WebSocket the way a
//! family device does: redeem an invite, say Hello, create a thread as
//! that peer, then delete it and confirm the host's own database no
//! longer has it. The bug this guards against looked fine on the device
//! (the row vanished from the sidebar) and only reappeared after a
//! restart, so a UI-level check would not have caught it — only the
//! host's stored state proves the delete landed.
//!
//! #[ignore] — needs the host app running on this machine.
use futures_util::{SinkExt, StreamExt};
use kinai::config::AppConfig;
use kinai::network::protocol::Envelope;
use tokio_tungstenite::tungstenite::Message as WsMessage;

async fn open_db() -> kinai::db::Db {
    kinai::db::Db::open(AppConfig::config_dir().join("kinai.db"))
        .await
        .expect("open the host's live database")
}

/// Second handle on the SAME file for the invite helpers, which take a
/// raw pool. Opening our own avoids widening `Db`'s API just for a test.
async fn open_pool() -> sqlx::SqlitePool {
    let path = AppConfig::config_dir().join("kinai.db");
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("open pool on the host's live database")
}

#[tokio::test]
#[ignore = "live: requires the KinAI host app to be running"]
async fn client_thread_delete_reaches_the_host() {
    let cfg = AppConfig::load_or_default();
    let db = open_db().await;
    let pool = open_pool().await;

    // A short-lived invite, exactly like inviting a family device.
    let invite = kinai::network::invite::create(&pool, &cfg, "thread-op test", 1)
        .await
        .expect("create invite");
    let peer_bucket = invite.short_code.clone();

    // A thread owned by THAT peer (this is what a device's list shows).
    let thread = db
        .create_thread(&peer_bucket, Some("delete me"))
        .await
        .expect("create thread");
    assert_eq!(
        db.list_threads(&peer_bucket).await.unwrap().len(),
        1,
        "precondition: the peer has one thread"
    );

    // Connect as that device and speak the protocol.
    let url = invite.host_url.replace("kinai://", "");
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("host must be running for this test");
    let (mut sink, mut source) = ws.split();
    let hello = Envelope::Hello {
        token: invite.jwt.clone(),
        display_name: "Thread Op Test".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
    };
    sink.send(WsMessage::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .expect("send hello");

    // Welcome must advertise the capability, or clients would refuse.
    let mut advertised = None;
    while let Some(Ok(msg)) = source.next().await {
        if let WsMessage::Text(t) = msg {
            if let Ok(Envelope::Welcome { host_thread_ops, .. }) = serde_json::from_str(&t) {
                advertised = Some(host_thread_ops);
                break;
            }
        }
    }
    assert_eq!(advertised, Some(true), "host must advertise host_thread_ops");

    sink.send(WsMessage::Text(
        serde_json::to_string(&Envelope::DeleteThread { thread_id: thread.id.clone() })
            .unwrap()
            .into(),
    ))
    .await
    .expect("send delete");

    let mut acked = false;
    while let Some(Ok(msg)) = source.next().await {
        if let WsMessage::Text(t) = msg {
            if let Ok(Envelope::ThreadOpAck { thread_id, ok, message }) = serde_json::from_str(&t) {
                assert_eq!(thread_id, thread.id);
                assert!(ok, "host refused the delete: {message}");
                acked = true;
                break;
            }
        }
    }
    assert!(acked, "no ThreadOpAck came back");

    // THE point of the test: the host's own storage no longer has it, so
    // the next launch cannot serve it back.
    let left = db.list_threads(&peer_bucket).await.unwrap();
    assert!(left.is_empty(), "host still holds the thread: {left:?}");

    kinai::network::invite::revoke(&pool, &invite.id).await.ok();
}
