//! Live proof that `/v1/invite/redeem` is throttled, logged, and — the
//! part a unit test cannot reach — that its `ConnectInfo<SocketAddr>`
//! extractor actually resolves against the running server.
//!
//! That last point is the whole reason this file exists. Adding
//! `ConnectInfo` to a handler without also calling
//! `.into_make_service_with_connect_info::<SocketAddr>()` on the router
//! compiles perfectly and then returns 500 on every single redeem — and
//! the family member pasting their code sees the raw error text, because
//! `commands::redeem_invite_code` renders whatever the host said. Nothing
//! in `cargo test --lib` can catch it: constructing `AxumState` needs a
//! Tauri `AppHandle`. So the assertion that matters most below is simply
//! "not 500".
//!
//! Run it inside the release smoke, with the host on the NEW binary:
//!
//!   cargo test --test invite_hardening_live -- --ignored --nocapture
//!
//! The limiter is keyed on source IP, so this burns this machine's own
//! redeem budget for about a minute afterwards. That is intended — it is
//! what the throttle is. No other live test redeems over HTTP (they all
//! call `invite::create` directly), so nothing else is affected.
//!
//! #[ignore] — needs the host app running on this machine.
use kinai::config::AppConfig;

/// A code that cannot exist: `l` and `o` are not in the 31-character
/// alphabet (`abcdefghjkmnpqrstuvwxyz23456789`, src/network/invite.rs),
/// so this can never collide with a real invite no matter how many the
/// household has.
const IMPOSSIBLE_CODE: &str = "lloollo";

fn base_url(cfg: &AppConfig) -> String {
    format!("http://127.0.0.1:{}", cfg.host.port)
}

#[tokio::test]
#[ignore = "live: requires the KinAI host app to be running"]
async fn redeem_is_throttled_per_ip_and_never_500s() {
    let cfg = AppConfig::load_or_default();
    let base = base_url(&cfg);
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("build client");

    // Sanity: the host is up and this is really the KinAI we think it is.
    let health = http
        .get(format!("{base}/healthz"))
        .send()
        .await
        .expect("host must be running for this test");
    assert!(health.status().is_success(), "healthz: {}", health.status());

    // A well-formed but impossible 6-char code. Deliberately the exact
    // shape a guesser would send.
    let code = &IMPOSSIBLE_CODE[..6];
    let mut statuses = Vec::new();
    let mut throttled_body = String::new();
    for _ in 0..15 {
        let res = http
            .get(format!("{base}/v1/invite/redeem"))
            .query(&[("code", code)])
            .send()
            .await
            .expect("request");
        let status = res.status().as_u16();
        if status == 429 && throttled_body.is_empty() {
            throttled_body = res.text().await.unwrap_or_default();
        }
        statuses.push(status);
    }
    println!("[statuses] {statuses:?}");

    // THE trap guard: a missing make-service turns every one of these
    // into a 500 while everything still compiles.
    assert!(
        !statuses.contains(&500),
        "a 500 here means ConnectInfo did not resolve — the router is \
         missing .into_make_service_with_connect_info::<SocketAddr>(). \
         statuses: {statuses:?}"
    );

    // Before the limit: a wrong code is a clean 404, not a hint.
    assert_eq!(statuses[0], 404, "first guess should be a plain not-found");

    // After the limit: 429. REDEEM_RPM is 10, so 15 guesses must cross it.
    assert!(
        statuses.contains(&429),
        "15 rapid guesses from one IP must be throttled; got {statuses:?}"
    );
    assert!(
        throttled_body.contains("too many attempts"),
        "unexpected throttle body: {throttled_body:?}"
    );

    // And the throttle must not leak whether the code was real: the 429
    // has to look identical whatever was guessed.
    assert!(
        !throttled_body.contains(code),
        "the throttle response echoed the guessed code: {throttled_body:?}"
    );

    let first_429 = statuses.iter().position(|s| *s == 429).unwrap();
    println!("[throttled after {first_429} attempts from this IP]");
    assert!(
        first_429 <= 11,
        "throttle kicked in later than REDEEM_RPM would suggest: {first_429}"
    );
}
