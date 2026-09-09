//! Live proof for the photo follow-up fix (0.2.119), spoken over the real
//! client WebSocket the way a family device does: redeem an invite, say
//! Hello, send a picture with a caption that needs a lookup on a slot that
//! cannot see images, then ask a text follow-up that ONLY the picture can
//! answer. Before the fix the follow-up saw a placeholder named "omitted".
//!
//! #[ignore] — needs the host app running on this machine and
//! `KINAI_PHOTO=<path to a png>` whose text says when a desk opens
//! (the repo's smoke image says "9:30").
use futures_util::{SinkExt, StreamExt};
use kinai::config::AppConfig;
use kinai::db::Attachment;
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

/// Wait for the AssistantDone of one turn, printing the reply and its
/// metrics. Panics after `secs`.
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
                            "\n[{client_msg_id}] slot={} model={} ttft={}ms total={}ms\n{}\n",
                            metrics.slot, metrics.model, metrics.first_token_ms,
                            metrics.total_ms, message.content
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
#[ignore = "live: requires the KinAI host app to be running and KINAI_PHOTO"]
async fn a_follow_up_about_a_photo_still_sees_it() {
    let cfg = AppConfig::load_or_default();
    let db = open_db().await;
    let pool = open_pool().await;
    let png = std::env::var("KINAI_PHOTO").expect("KINAI_PHOTO=<path to a png>");
    let bytes = std::fs::read(&png).expect("read the photo");
    use base64::Engine as _;
    let data_url = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );

    let invite = kinai::network::invite::create(&pool, &cfg, "photo follow-up test", 1)
        .await
        .expect("create invite");
    let peer = invite.short_code.clone();
    let thread = db
        .create_thread(&peer, Some("photo follow-up"))
        .await
        .expect("create thread");

    let url = invite.host_url.replace("kinai://", "");
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("host must be running for this test");
    let (mut sink, mut source) = ws.split();
    let hello = Envelope::Hello {
        token: invite.jwt.clone(),
        display_name: "Photo Test".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
    };
    sink.send(WsMessage::Text(serde_json::to_string(&hello).unwrap().into()))
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

    let send = |id: &str, content: &str, attachments: Vec<Attachment>| {
        let env = Envelope::SendMessage {
            thread_id: thread.id.clone(),
            content: content.into(),
            sender: "Photo Test".into(),
            client_msg_id: id.into(),
            attachments,
        };
        WsMessage::Text(serde_json::to_string(&env).unwrap().into())
    };

    // Turn 1: the picture, on a slot that cannot see, with a caption that
    // needs a lookup — exercises the vision route WITH tools.
    let photo = Attachment {
        kind: "image".into(),
        mime: Some("image/png".into()),
        name: Some("notice.png".into()),
        data_url: Some(data_url),
    };
    let m1 = send(
        "t1",
        "/online Who wrote the book named in this picture, and when was it first published? Check online.",
        vec![photo],
    );
    sink.send(m1).await.expect("send turn 1");
    let r1 = wait_done(&mut source, "t1", 300).await;

    // Turn 2: text only. The desk time is nowhere but in the picture, so a
    // correct answer proves the follow-up still sees it.
    let m2 = send(
        "t2",
        "What time does the return desk open, according to the picture?",
        vec![],
    );
    sink.send(m2).await.expect("send turn 2");
    let r2 = wait_done(&mut source, "t2", 300).await;

    // Turn 3: the exact shape from the field report.
    let m3 = send("t3", "Continue", vec![]);
    sink.send(m3).await.expect("send turn 3");
    let r3 = wait_done(&mut source, "t3", 300).await;

    // Clean up the test peer's thread and invite before asserting.
    db.delete_thread(&peer, &thread.id).await.ok();
    kinai::network::invite::revoke(&pool, &invite.id).await.ok();

    let lc1 = r1.to_ascii_lowercase();
    assert!(
        lc1.contains("hawking") || lc1.contains("1988"),
        "turn 1 should read the title off the picture and look it up: {r1}"
    );
    assert!(
        r2.contains("9:30") || r2.contains("9.30") || r2.to_ascii_lowercase().contains("nine thirty"),
        "the follow-up must still see the picture: {r2}"
    );
    let lc3 = r3.to_ascii_lowercase();
    assert!(
        !lc3.contains("omitted") && !lc3.contains("didn't come through") && !lc3.contains("did not come through"),
        "'Continue' must not report the image as missing: {r3}"
    );
}
