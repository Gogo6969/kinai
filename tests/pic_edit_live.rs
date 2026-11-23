//! Live proof for `/pic` with a picture attached: the picture is changed
//! as the message says and comes back as a new picture.
//!
//! Both tests are #[ignore]d. They need `KINAI_PHOTO=<path to a png or
//! jpg>` and the picture server set in this machine's KinAI config.
//!
//! * `the_command_changes_a_sent_picture` calls the command handler
//!   directly — it proves the code in THIS checkout without the app.
//! * `a_family_device_gets_its_picture_changed` speaks the client
//!   WebSocket to the RUNNING host app, the way a family device does.
use futures_util::{SinkExt, StreamExt};
use kinai::config::AppConfig;
use kinai::db::Attachment;
use kinai::network::protocol::Envelope;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

fn photo() -> Attachment {
    let path = std::env::var("KINAI_PHOTO").expect("KINAI_PHOTO=<path to a png or jpg>");
    let bytes = std::fs::read(&path).expect("read the photo");
    let mime = if path.to_lowercase().ends_with(".png") { "image/png" } else { "image/jpeg" };
    use base64::Engine as _;
    Attachment {
        kind: "image".into(),
        mime: Some(mime.into()),
        name: Some("photo".into()),
        data_url: Some(format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        )),
    }
}

/// The reply must be a picture served by the host, and that file must be
/// a real PNG on this machine's disk.
fn assert_is_a_changed_picture(reply: &str) {
    println!("{reply}");
    assert!(reply.starts_with("!["), "the reply should start with the picture: {reply}");
    assert!(reply.contains("changed picture"), "the caption should say it was changed: {reply}");
    assert!(reply.contains("/edit"), "the reply should say how to change it further: {reply}");
    let name = reply
        .split("/v1/pic/")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("a /v1/pic/ link in the reply");
    let saved = kinai::comfyui::pics_dir().join(name);
    let bytes = std::fs::read(&saved).expect("the changed picture is saved on the host");
    assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']), "saved file is a PNG");
    assert!(bytes.len() > 50_000, "a real picture, not a blank: {} bytes", bytes.len());
}

#[tokio::test]
#[ignore = "live: requires the picture server and KINAI_PHOTO"]
async fn the_command_changes_a_sent_picture() {
    let cfg = AppConfig::load_or_default();
    let reply = kinai::slash::handle(
        &cfg,
        "/pic Put a golden paper birthday crown on the man's head. Keep everything else the same.",
        &[photo()],
    )
    .await
    .expect("/pic is a native command");
    assert_is_a_changed_picture(&reply);

    // No instruction: a hint, and nothing is sent to the picture server.
    let hint = kinai::slash::handle(&cfg, "/pic", &[photo()]).await.expect("a reply");
    assert!(hint.contains("Say what to change"), "{hint}");
}

/// `/edit` with no picture attached changes the RESULT of the step before.
/// Runs the handler directly on a throwaway database, so it proves the
/// code in this checkout without the app.
#[tokio::test]
#[ignore = "live: requires the picture server and KINAI_PHOTO"]
async fn edit_changes_the_previous_result() {
    let cfg = AppConfig::load_or_default();
    let dir = tempfile::tempdir().unwrap();
    let db = kinai::db::Db::open(dir.path().join("t.db")).await.unwrap();
    let t = db.create_thread("host", Some("pic edit")).await.unwrap();

    let first_ask = "/pic Put a golden paper birthday crown on the head of the person. Keep everything else the same.";
    db.append_message(&t.id, "user", "Test", first_ask, &[photo()]).await.unwrap();
    let first = kinai::slash::handle_in_thread(&cfg, &db, &t.id, first_ask, &[photo()])
        .await
        .expect("a reply");
    assert_is_a_changed_picture(&first);
    db.append_message(&t.id, "assistant", "KinAI", &first, &[]).await.unwrap();

    let second_ask = "/edit Make the crown much bigger and make the sweater golden.";
    db.append_message(&t.id, "user", "Test", second_ask, &[]).await.unwrap();
    let second = kinai::slash::handle_in_thread(&cfg, &db, &t.id, second_ask, &[])
        .await
        .expect("a reply");
    assert_is_a_changed_picture(&second);
    let name = |r: &str| r.split("/v1/pic/").nth(1).unwrap().split(')').next().unwrap().to_string();
    assert_ne!(name(&first), name(&second), "the second step makes a new picture");
    println!("FIRST={}\nSECOND={}", name(&first), name(&second));
}

#[tokio::test]
#[ignore = "live: requires the KinAI host app to be running and KINAI_PHOTO"]
async fn a_family_device_gets_its_picture_changed() {
    let cfg = AppConfig::load_or_default();
    let db_path = AppConfig::config_dir().join("kinai.db");
    let db = kinai::db::Db::open(db_path.clone()).await.expect("open the host's live database");
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect(&format!("sqlite://{}", db_path.display()))
        .await
        .expect("open pool on the host's live database");

    let invite = kinai::network::invite::create(&pool, &cfg, "pic edit test", 1)
        .await
        .expect("create invite");
    let peer = invite.short_code.clone();
    let thread = db.create_thread(&peer, Some("pic edit")).await.expect("create thread");

    let url = invite.host_url.replace("kinai://", "");
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("host must be running for this test");
    let (mut sink, mut source) = ws.split();
    let hello = Envelope::Hello {
        token: invite.jwt.clone(),
        display_name: "Pic Edit Test".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        tz: String::new(),
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

    let env = Envelope::SendMessage {
        thread_id: thread.id.clone(),
        content: "/pic Put a golden paper birthday crown on the man's head. Keep everything else the same.".into(),
        sender: "Pic Edit Test".into(),
        client_msg_id: "p1".into(),
        attachments: vec![photo()],
    };
    sink.send(WsMessage::Text(serde_json::to_string(&env).unwrap().into()))
        .await
        .expect("send the picture");

    let reply = tokio::time::timeout(Duration::from_secs(300), async {
        while let Some(Ok(msg)) = source.next().await {
            if let WsMessage::Text(t) = msg {
                if let Ok(Envelope::AssistantDone { client_msg_id, message, .. }) =
                    serde_json::from_str::<Envelope>(&t)
                {
                    if client_msg_id == "p1" {
                        return message.content;
                    }
                }
            }
        }
        panic!("socket closed before the reply");
    })
    .await
    .expect("a reply within 5 minutes");

    // Second step, the way a person goes on: no picture attached.
    let env2 = Envelope::SendMessage {
        thread_id: thread.id.clone(),
        content: "/edit Make the crown much bigger and make the sweater golden.".into(),
        sender: "Pic Edit Test".into(),
        client_msg_id: "p2".into(),
        attachments: vec![],
    };
    sink.send(WsMessage::Text(serde_json::to_string(&env2).unwrap().into()))
        .await
        .expect("send the follow-up");
    let reply2 = tokio::time::timeout(Duration::from_secs(300), async {
        while let Some(Ok(msg)) = source.next().await {
            if let WsMessage::Text(t) = msg {
                if let Ok(Envelope::AssistantDone { client_msg_id, message, .. }) =
                    serde_json::from_str::<Envelope>(&t)
                {
                    if client_msg_id == "p2" {
                        return message.content;
                    }
                }
            }
        }
        panic!("socket closed before the second reply");
    })
    .await
    .expect("a second reply within 5 minutes");

    // Clean up before asserting, so a failure leaves nothing behind.
    db.delete_thread(&peer, &thread.id).await.ok();
    kinai::network::invite::revoke(&pool, &invite.id).await.ok();
    assert_is_a_changed_picture(&reply);
    assert_is_a_changed_picture(&reply2);
    assert_ne!(reply, reply2, "the follow-up makes a new picture");
}
