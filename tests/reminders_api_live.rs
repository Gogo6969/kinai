//! Live proof of the reminder HTTP API against the running host — the
//! whole point of which is that a program, not a model, decides.
//!
//! It mints a real automation key, drives the three routes with `reqwest`
//! the way a cron job would, and checks the rows that actually landed. It
//! also asserts the two refusals that make the key safe to hand out: the
//! key's own short code must NOT be redeemable over the network, and a
//! request with no key at all must be turned away.
//!
//! #[ignore] — needs the host app running on this machine.
use kinai::config::AppConfig;
use kinai::db::HOST_PEER;

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

#[tokio::test]
#[ignore = "live: requires the KinAI host app to be running"]
async fn a_script_can_write_a_reminder_without_talking_to_a_model() {
    let cfg = AppConfig::load_or_default();
    let db = open_db().await;
    let pool = open_pool().await;
    let base = format!("http://127.0.0.1:{}", cfg.host.port);
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();

    let key = kinai::network::invite::create_scoped(
        &pool,
        &cfg,
        "reminders api live test",
        1,
        kinai::auth::AUTOMATION_SCOPE,
        HOST_PEER,
    )
    .await
    .expect("mint an automation key");

    // How many the host bucket already holds — this is the family's real
    // database, so assert on deltas, never on absolute counts.
    let before: Vec<String> =
        db.list_reminders(HOST_PEER).await.unwrap().into_iter().map(|r| r.id).collect();

    // ---- 1. No key at all is turned away. ----
    let anon = http.get(format!("{base}/v1/reminders")).send().await.expect("GET");
    assert_eq!(anon.status().as_u16(), 401, "an unauthenticated read must be refused");

    // ---- 2. The key's short code is NOT redeemable over the network. ----
    // An automation token can act as the host, so a guessed code that
    // resolved to one would be an escalation past any family invite.
    let redeem = http
        .get(format!("{base}/v1/invite/redeem"))
        .query(&[("code", key.short_code.as_str())])
        .send()
        .await
        .expect("redeem");
    assert_eq!(
        redeem.status().as_u16(),
        404,
        "an automation key's code must look exactly like a code that does not exist"
    );

    // ---- 3. A refusal is a 422 with a stable machine code. ----
    let bad = http
        .post(format!("{base}/v1/reminders"))
        .bearer_auth(&key.jwt)
        .json(&serde_json::json!({ "text": "too soon", "in_minutes": 0 }))
        .send()
        .await
        .expect("POST");
    assert_eq!(bad.status().as_u16(), 422);
    let body: serde_json::Value = bad.json().await.expect("json body");
    assert_eq!(body["code"], "too_soon", "body was {body}");

    // A caller that names no time at all gets its own code.
    let nowhen = http
        .post(format!("{base}/v1/reminders"))
        .bearer_auth(&key.jwt)
        .json(&serde_json::json!({ "text": "when?" }))
        .send()
        .await
        .expect("POST");
    assert_eq!(nowhen.status().as_u16(), 422);
    let body: serde_json::Value = nowhen.json().await.unwrap();
    assert_eq!(body["code"], "no_when", "body was {body}");

    // ---- 4. The happy path. ----
    let created = http
        .post(format!("{base}/v1/reminders"))
        .bearer_auth(&key.jwt)
        .json(&serde_json::json!({
            "text": "test: take the bins out",
            "in_minutes": 90
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(created.status().as_u16(), 201, "created");
    let row: serde_json::Value = created.json().await.expect("json body");
    let id = row["id"].as_str().expect("an id").to_string();
    println!("[created] id={} due_local={} tz={}", &id[..8], row["due_local"], row["tz"]);

    // It landed in the HOST bucket — the one the Calendar reads. A cron
    // writing into its own invite's bucket instead would fire into nothing.
    assert_eq!(row["peer_id"], HOST_PEER, "an act_as:host key writes the host's calendar");
    assert_eq!(row["status"], "scheduled");

    // ---- 5. It is listed. ----
    let listed = http
        .get(format!("{base}/v1/reminders"))
        .bearer_auth(&key.jwt)
        .send()
        .await
        .expect("GET");
    assert_eq!(listed.status().as_u16(), 200);
    let rows: Vec<serde_json::Value> = listed.json().await.expect("json list");
    assert!(
        rows.iter().any(|r| r["id"] == id.as_str()),
        "the new reminder should be in the caller's own list"
    );

    // ---- 6. An unknown verb is a 422, not a 500. ----
    let oops = http
        .post(format!("{base}/v1/reminders/{id}/action"))
        .bearer_auth(&key.jwt)
        .json(&serde_json::json!({ "action": "cancel" }))
        .send()
        .await
        .expect("POST action");
    assert_eq!(oops.status().as_u16(), 422, "\"cancel\" is not one of the verbs");

    // ---- 7. Delete it again, and leave the database as we found it. ----
    let gone = http
        .post(format!("{base}/v1/reminders/{id}/action"))
        .bearer_auth(&key.jwt)
        .json(&serde_json::json!({ "action": "delete" }))
        .send()
        .await
        .expect("POST action");
    assert_eq!(gone.status().as_u16(), 200);

    let after: Vec<String> =
        db.list_reminders(HOST_PEER).await.unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(after, before, "the host's calendar is exactly as we found it");

    kinai::network::invite::revoke(&pool, &key.id).await.ok();
}
