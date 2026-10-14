//! The maintenance API: how a script tells the host "this model server is
//! being worked on — don't wait for it".
//!
//! Why it exists. The M5 that serves `fast` and `balanced` is restarted for
//! tests, engine changes and updates. Without a word to the host, KinAI
//! finds out the hard way: a probe that fails, or a request that fails,
//! and the family meanwhile gets a slower first answer or an error. With
//! this call the host switches to the backup model BEFORE the server goes
//! away, says why, and switches back by itself when the window ends.
//!
//! WHAT IT TOUCHES. Only runtime state (`availability`). It never edits
//! `config.toml`, never changes a slot's settings and never needs KinAI to
//! restart; the window ends at its own deadline even if nobody calls "end".
//!
//! WHO MAY CALL IT. A KinAI API key (Manage family → Reminder API key; the
//! same kind of key the reminder API takes). A family chat token is refused:
//! one family member must not be able to put the house on the backup model.
//!
//! WHAT IT NAMES. A server, by any URL that has its host and port — the
//! slot's own base URL is the natural thing to send. Every slot and vision
//! endpoint on that server is then treated as down, so `fast`, `balanced`
//! and the image reader all move together. An address that no slot uses is
//! refused (422) and the answer lists the addresses that are configured, so
//! a typo is loud instead of a no-op.
//!
//!   POST   /v1/maintenance   {"endpoint": "...", "minutes": 45, "reason": "..."}
//!   GET    /v1/maintenance   what is open right now
//!   DELETE /v1/maintenance[?endpoint=...]   close one window, or all

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::availability::{self, Maintenance};
use crate::config::AppConfig;

use super::api::{bearer_claims, err, ApiError, ApiResult};
use super::server::AxumState;

#[derive(Deserialize)]
pub struct BeginBody {
    /// Any URL (or `host:port`) of the server being worked on.
    pub endpoint: String,
    /// How long, from now. Default 30, at most 240.
    #[serde(default)]
    pub minutes: Option<i64>,
    /// The end as an RFC 3339 time, instead of `minutes`.
    #[serde(default)]
    pub until: Option<String>,
    /// Free text for the status call and the log; never shown to the family.
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize)]
pub struct EndQuery {
    #[serde(default)]
    pub endpoint: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct WindowView {
    pub endpoint: String,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub minutes_left: i64,
    pub reason: String,
    /// The things this window takes out of service: slot names
    /// (`fast`, `balanced`, `deep`, `online`) and `vision`.
    pub affects: Vec<String>,
}

#[derive(Serialize, Debug)]
pub struct StatusView {
    pub active: Vec<WindowView>,
}

#[derive(Serialize, Debug)]
pub struct EndView {
    pub closed: usize,
}

/// Everything in the config that points at `authority`.
pub(crate) fn affected_by(cfg: &AppConfig, authority: &str) -> Vec<String> {
    let mut out: Vec<String> = crate::slash::SLOTS
        .iter()
        .filter(|s| {
            let st = crate::slash::slot_settings(cfg, s);
            st.is_active() && availability::authority(&st.base_url) == authority
        })
        .map(|s| s.to_string())
        .collect();
    let v = &cfg.vision;
    if v.enabled
        && [&v.primary.base_url, &v.failover.base_url]
            .iter()
            .any(|u| availability::authority(u) == authority)
    {
        out.push("vision".to_string());
    }
    out
}

/// Every server address the config uses, for the "no such server" answer.
pub(crate) fn configured_authorities(cfg: &AppConfig) -> Vec<String> {
    let mut v: Vec<String> = crate::slash::SLOTS
        .iter()
        .map(|s| availability::authority(&crate::slash::slot_settings(cfg, s).base_url))
        .chain([
            availability::authority(&cfg.vision.primary.base_url),
            availability::authority(&cfg.vision.failover.base_url),
        ])
        .filter(|a| !a.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v
}

fn view(m: &Maintenance, cfg: &AppConfig, now: DateTime<Utc>) -> WindowView {
    WindowView {
        endpoint: m.authority.clone(),
        since: m.since,
        until: m.until,
        minutes_left: ((m.until - now).num_seconds().max(0) + 59) / 60,
        reason: m.reason.clone(),
        affects: affected_by(cfg, &m.authority),
    }
}

/// Resolve the length: `minutes` or `until`, never both, default 30.
pub(crate) fn resolve_minutes(
    minutes: Option<i64>,
    until: Option<&str>,
    now: DateTime<Utc>,
) -> Result<i64, (&'static str, String)> {
    match (minutes, until) {
        (Some(_), Some(_)) => Err(("ambiguous_length", "send minutes or until, not both".into())),
        (Some(m), None) => Ok(m),
        (None, Some(u)) => {
            let end = DateTime::parse_from_rfc3339(u.trim())
                .map_err(|_| ("bad_until", "until must be an RFC 3339 time, e.g. 2031-03-05T17:30:00-04:00".into()))?
                .with_timezone(&Utc);
            let secs = (end - now).num_seconds();
            if secs <= 0 {
                return Err(("until_in_the_past", "until is already over".into()));
            }
            Ok((secs + 59) / 60)
        }
        (None, None) => Ok(availability::DEFAULT_MINUTES),
    }
}

type Refusal = (StatusCode, Json<ApiError>);

/// Only an API key may call this — never a family chat token: one family
/// member must not be able to put the whole house on the backup model.
pub(crate) fn require_api_key(claims: &crate::auth::Claims) -> Result<(), Refusal> {
    if claims.scope != crate::auth::AUTOMATION_SCOPE {
        return Err(err(
            StatusCode::FORBIDDEN,
            "not_an_api_key",
            "this is a chat token; the maintenance call needs an API key \
             (Manage family → Reminder API key → Create API key)",
        ));
    }
    Ok(())
}

/// Open (or renew) a window. Refuses an address no slot or vision endpoint
/// uses — a typo must be loud, not a silent no-op.
pub(crate) fn open_window(
    app: &crate::AppState,
    now: DateTime<Utc>,
    body: &BeginBody,
) -> Result<WindowView, Refusal> {
    let authority = availability::authority(&body.endpoint);
    if authority.is_empty() {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_endpoint",
            "endpoint must be a URL or host:port, e.g. http://example-server:8095",
        ));
    }
    let minutes = resolve_minutes(body.minutes, body.until.as_deref(), now)
        .map_err(|(code, msg)| err(StatusCode::UNPROCESSABLE_ENTITY, code, msg))?;

    let cfg = app.config.read().clone();
    let affects = affected_by(&cfg, &authority);
    if affects.is_empty() {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_endpoint",
            format!(
                "no model slot or vision endpoint uses {authority}; the addresses in use are: {}",
                configured_authorities(&cfg).join(", ")
            ),
        ));
    }

    let window = app.availability.lock().begin(&authority, now, minutes, &body.reason);
    // The slot cache would otherwise keep saying "alive" for up to 15 s.
    app.slot_health.lock().clear();
    tracing::info!(
        "maintenance: {} out of service until {} ({} minute(s); {})",
        authority,
        window.until.to_rfc3339(),
        minutes.clamp(1, availability::MAX_MINUTES),
        affects.join(", ")
    );
    Ok(view(&window, &cfg, now))
}

/// Every open window.
pub(crate) fn current(app: &crate::AppState, now: DateTime<Utc>) -> StatusView {
    let cfg = app.config.read().clone();
    let active = app.availability.lock().active(now).iter().map(|m| view(m, &cfg, now)).collect();
    StatusView { active }
}

/// Close one window (`Some`) or all of them (`None`).
pub(crate) fn close_windows(
    app: &crate::AppState,
    now: DateTime<Utc>,
    endpoint: Option<&str>,
) -> Result<EndView, Refusal> {
    let authority = endpoint.map(availability::authority);
    if matches!(authority.as_deref(), Some("")) {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_endpoint",
            "endpoint must be a URL or host:port",
        ));
    }
    let closed = app.availability.lock().end(authority.as_deref(), now);
    app.slot_health.lock().clear();
    tracing::info!("maintenance: {closed} window(s) closed");
    Ok(EndView { closed })
}

pub(super) async fn begin(
    State(s): State<AxumState>,
    headers: HeaderMap,
    Json(body): Json<BeginBody>,
) -> ApiResult<WindowView> {
    require_api_key(&bearer_claims(&s, &headers).await?)?;
    let window = open_window(&s.app, Utc::now(), &body)?;
    Ok((StatusCode::OK, Json(window)))
}

pub(super) async fn status(State(s): State<AxumState>, headers: HeaderMap) -> ApiResult<StatusView> {
    require_api_key(&bearer_claims(&s, &headers).await?)?;
    Ok((StatusCode::OK, Json(current(&s.app, Utc::now()))))
}

pub(super) async fn end(
    State(s): State<AxumState>,
    headers: HeaderMap,
    Query(q): Query<EndQuery>,
) -> ApiResult<EndView> {
    require_api_key(&bearer_claims(&s, &headers).await?)?;
    let closed = close_windows(&s.app, Utc::now(), q.endpoint.as_deref())?;
    Ok((StatusCode::OK, Json(closed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn cfg() -> AppConfig {
        let mut c = AppConfig::default();
        c.llm.base_url = "http://127.0.0.1:18095".into();
        c.llm.model = "glm-fast".into();
        c.llm.enabled = true;
        c.llm_balanced.base_url = "http://127.0.0.1:18095".into();
        c.llm_balanced.model = "glm-balanced".into();
        c.llm_balanced.enabled = true;
        c.llm_deep.base_url = "http://127.0.0.1:18087".into();
        c.llm_deep.model = "qwen".into();
        c.llm_deep.enabled = true;
        c.vision.enabled = true;
        c.vision.primary.base_url = "http://127.0.0.1:18095".into();
        c.vision.primary.model = "glm-fast".into();
        c.vision.failover.base_url = "http://127.0.0.1:18095".into();
        c.vision.failover.model = "glm-fast".into();
        c
    }

    #[test]
    fn one_address_takes_every_slot_and_the_image_reader_with_it() {
        let c = cfg();
        assert_eq!(
            affected_by(&c, "127.0.0.1:18095"),
            vec!["fast".to_string(), "balanced".to_string(), "vision".to_string()]
        );
        assert_eq!(affected_by(&c, "127.0.0.1:18087"), vec!["deep".to_string()]);
        assert!(affected_by(&c, "nowhere:1").is_empty());
    }

    #[test]
    fn a_paused_slot_is_not_listed_as_affected() {
        let mut c = cfg();
        c.llm_balanced.enabled = false;
        assert_eq!(
            affected_by(&c, "127.0.0.1:18095"),
            vec!["fast".to_string(), "vision".to_string()]
        );
    }

    #[test]
    fn the_configured_addresses_are_listed_once_each() {
        let c = cfg();
        let v = configured_authorities(&c);
        assert!(v.contains(&"127.0.0.1:18095".to_string()));
        assert!(v.contains(&"127.0.0.1:18087".to_string()));
        assert_eq!(v.iter().filter(|a| *a == "127.0.0.1:18095").count(), 1);
    }

    #[test]
    fn the_length_is_minutes_or_a_deadline_never_both() {
        let now = Utc::now();
        assert_eq!(resolve_minutes(None, None, now), Ok(availability::DEFAULT_MINUTES));
        assert_eq!(resolve_minutes(Some(45), None, now), Ok(45));
        assert_eq!(resolve_minutes(Some(45), Some("2030-01-01T00:00:00Z"), now).unwrap_err().0, "ambiguous_length");
        let soon = (now + Duration::minutes(20) + Duration::seconds(10)).to_rfc3339();
        assert_eq!(resolve_minutes(None, Some(&soon), now), Ok(21), "rounded UP so the window never ends early");
        let past = (now - Duration::minutes(1)).to_rfc3339();
        assert_eq!(resolve_minutes(None, Some(&past), now).unwrap_err().0, "until_in_the_past");
        assert_eq!(resolve_minutes(None, Some("tomorrow"), now).unwrap_err().0, "bad_until");
    }

    #[test]
    fn minutes_left_rounds_up_and_never_goes_negative() {
        let c = cfg();
        let now = Utc::now();
        let mut a = availability::Availability::default();
        let w = a.begin("127.0.0.1:18095", now, 10, "test");
        assert_eq!(view(&w, &c, now).minutes_left, 10);
        assert_eq!(view(&w, &c, now + Duration::seconds(30)).minutes_left, 10);
        assert_eq!(view(&w, &c, now + Duration::minutes(30)).minutes_left, 0);
    }

    // ---- the real functions, over a real AppState (not on Windows: see lib.rs::test_state) ----

    #[cfg(not(target_os = "windows"))]
    fn body(endpoint: &str, minutes: Option<i64>) -> BeginBody {
        BeginBody { endpoint: endpoint.into(), minutes, until: None, reason: "native check".into() }
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn opening_a_window_takes_the_whole_server_out_of_service() {
        let st = crate::test_state(&cfg()).await;
        let now = Utc::now();
        let w = open_window(&st, now, &body("http://127.0.0.1:18095/v1", Some(45))).unwrap();
        assert_eq!(w.endpoint, "127.0.0.1:18095");
        assert_eq!(w.minutes_left, 45);
        assert_eq!(w.affects, vec!["fast", "balanced", "vision"]);
        // The slots on that server are down at once, with no probe; the backup is not.
        assert!(matches!(
            crate::slash::slot_unavailable(&st, "fast").await,
            Some(crate::slash::Unavailable::Maintenance { .. })
        ));
        assert!(matches!(
            crate::slash::slot_unavailable(&st, "balanced").await,
            Some(crate::slash::Unavailable::Maintenance { .. })
        ));
        assert_eq!(current(&st, now).active.len(), 1);
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn an_address_no_slot_uses_is_refused_and_the_answer_lists_the_real_ones() {
        let st = crate::test_state(&cfg()).await;
        let (status, Json(e)) = open_window(&st, Utc::now(), &body("http://127.0.0.1:19999", None)).unwrap_err();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(e.code, "unknown_endpoint");
        assert!(e.message.contains("127.0.0.1:18095") && e.message.contains("127.0.0.1:18087"), "{}", e.message);
        assert!(current(&st, Utc::now()).active.is_empty(), "a refused call opens nothing");
        let (_, Json(e)) = open_window(&st, Utc::now(), &body("", None)).unwrap_err();
        assert_eq!(e.code, "bad_endpoint");
        let mut b = body("http://127.0.0.1:18095", Some(5));
        b.until = Some("2030-01-01T00:00:00Z".into());
        let (_, Json(e)) = open_window(&st, Utc::now(), &b).unwrap_err();
        assert_eq!(e.code, "ambiguous_length");
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn ending_a_window_gives_the_server_back_at_once() {
        let st = crate::test_state(&cfg()).await;
        let now = Utc::now();
        open_window(&st, now, &body("127.0.0.1:18095", Some(30))).unwrap();
        open_window(&st, now, &body("http://127.0.0.1:18087", Some(30))).unwrap();
        // One by address (any spelling), then the rest.
        assert_eq!(close_windows(&st, now, Some("HTTP://127.0.0.1:18095/")).unwrap().closed, 1);
        // The closed window no longer reads as maintenance (the address is a dead
        // loopback port, so the slot is "down" — but not "being serviced").
        assert!(!matches!(
            crate::slash::slot_unavailable(&st, "balanced").await,
            Some(crate::slash::Unavailable::Maintenance { .. })
        ));
        assert_eq!(current(&st, now).active.len(), 1);
        assert_eq!(close_windows(&st, now, None).unwrap().closed, 1);
        assert!(current(&st, now).active.is_empty());
        assert_eq!(close_windows(&st, now, None).unwrap().closed, 0, "nothing left to close");
        let (_, Json(e)) = close_windows(&st, now, Some("")).unwrap_err();
        assert_eq!(e.code, "bad_endpoint");
    }

    /// A chat token must never be able to switch the house to the backup model.
    #[test]
    fn a_chat_token_is_refused_and_an_api_key_is_not() {
        let claims = |scope: &str| crate::auth::Claims {
            sub: "abc123".into(),
            iss: "kinai".into(),
            aud: "x".into(),
            iat: 0,
            exp: i64::MAX,
            label: "t".into(),
            scope: scope.into(),
            act_as: String::new(),
        };
        for chat in ["", "family"] {
            let (status, Json(e)) = require_api_key(&claims(chat)).unwrap_err();
            assert_eq!(status, StatusCode::FORBIDDEN, "scope {chat:?}");
            assert_eq!(e.code, "not_an_api_key");
        }
        assert!(require_api_key(&claims(crate::auth::AUTOMATION_SCOPE)).is_ok());
    }
}
