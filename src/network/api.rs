//! The reminder HTTP API — the one way in that is not a chat turn.
//!
//! Until now the only path that could write a reminder ran through the
//! `set_reminder` tool, which means a model had to decide to call it. That
//! is fine for a person typing a sentence and useless for a cron job: the
//! same input can produce a reminder, a clarifying question, or nothing.
//! These three routes are deterministic.
//!
//! WHAT A TOKEN MAY DO. The peer written to is `claims.writes_as()` and
//! nothing in the request body can change it. An automation key minted
//! with `act_as: "host"` writes the host's own calendar; a family token
//! writes its own. There is deliberately no way to name somebody else:
//! reminders are per member, and one family member scheduling notifications
//! onto another's phone is not a feature this household asked for.
//!
//! WHAT THIS IS NOT. There are no events, no durations, no all-day, no
//! location, no attendees, and no recurrence — the `repeat` column exists
//! and is hardcoded empty, read by nothing. "Calendar" is the name of the
//! page that lists reminders, not a calendar in the iCal sense.
//!
//! Validation is `reminders::spec`, the same code the chat tool uses, so
//! this route cannot be the careless way in. A refusal comes back as 422
//! with a stable `code` — a caller that is a program should not have to
//! parse an apology.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::auth::Claims;
use crate::db::Reminder;
use crate::reminders::spec;

use super::invite;
use super::server::AxumState;

/// An error a program reads: a stable token plus a sentence for a human
/// reading a log.
#[derive(Serialize)]
pub struct ApiError {
    code: &'static str,
    message: String,
}

fn err(status: StatusCode, code: &'static str, message: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (status, Json(ApiError { code, message: message.into() }))
}

type ApiResult<T> = Result<(StatusCode, Json<T>), (StatusCode, Json<ApiError>)>;

/// Resolve `Authorization: Bearer <jwt>` to claims.
///
/// The audience is derived exactly the way the WebSocket handshake does
/// it — the cached `host_url` first, falling back to recomputing it — so a
/// token minted for this host validates on both paths or neither.
async fn bearer_claims(
    s: &AxumState,
    headers: &HeaderMap,
) -> Result<Claims, (StatusCode, Json<ApiError>)> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            err(
                StatusCode::UNAUTHORIZED,
                "no_token",
                "send Authorization: Bearer <your KinAI API key>",
            )
        })?;

    // Both locks are sync (parking_lot); clone out before any await.
    let host_url = {
        let cached = s.app.stats.read().host_url.clone();
        match cached {
            Some(u) => u,
            None => invite::public_host_url(&s.app.config.read()),
        }
    };

    invite::validate_jwt_for_host(&s.app.db.pool, raw, &host_url)
        .await
        .map_err(|e| {
            // Never echo the token back, not even a prefix of it.
            tracing::warn!("reminder api: rejected a bearer token: {e}");
            err(StatusCode::UNAUTHORIZED, "bad_token", "that key is not valid for this host")
        })
}

/// The calendar this token writes to. Never influenced by the request.
fn bucket(claims: &Claims) -> &str {
    claims.writes_as()
}

/// The zone the caller's reminders are rendered in: the member's own if
/// the host has learned it, otherwise the host machine's.
async fn zone_for(s: &AxumState, peer: &str) -> (Option<chrono_tz::Tz>, String) {
    let name = s
        .app
        .db
        .peer_tz(peer)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(crate::tools::datetime::host_tz_name);
    let tz = name.parse::<chrono_tz::Tz>().ok();
    let display = tz.map(|z| z.name().to_string()).unwrap_or(name);
    (tz, display)
}

#[derive(Deserialize)]
pub struct CreateBody {
    text: String,
    /// Minutes from now. Mutually exclusive with `due_local`.
    #[serde(default)]
    in_minutes: Option<i64>,
    /// `YYYY-MM-DDTHH:MM` in the caller's own zone.
    #[serde(default)]
    due_local: Option<String>,
}

/// `POST /v1/reminders`
pub async fn create(
    State(s): State<AxumState>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> ApiResult<Reminder> {
    let claims = bearer_claims(&s, &headers).await?;
    let peer = bucket(&claims).to_string();
    let (tz, tz_name) = zone_for(&s, &peer).await;

    // "You gave me neither" is worded per surface; `When` makes it
    // unrepresentable inside the spec.
    let when = match (body.in_minutes, body.due_local.as_deref()) {
        (Some(_), Some(_)) => {
            return Err(err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ambiguous_when",
                "send in_minutes or due_local, not both",
            ))
        }
        (Some(m), None) => spec::When::InMinutes(m),
        (None, Some(d)) => spec::When::DueLocal(d),
        (None, None) => {
            return Err(err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "no_when",
                "send in_minutes (minutes from now) or due_local (YYYY-MM-DDTHH:MM)",
            ))
        }
    };

    let planned = spec::plan(&body.text, when, tz, &tz_name, chrono::Utc::now())
        .map_err(|r| err(StatusCode::UNPROCESSABLE_ENTITY, r.code(), r.prose()))?;

    // A runaway guard, and the reason it lives on THIS route rather than
    // in the store: a person adding reminders by hand cannot realistically
    // reach the ceiling, but a cron that misfires can reach it in seconds,
    // and every row it writes is a notification on the member's devices
    // plus a Telegram message. Checked after validation so a malformed
    // request is still told what was actually wrong with it.
    let live = s.app.db.count_live_reminders(&peer).await.unwrap_or(0);
    if live >= spec::MAX_LIVE_PER_PEER {
        tracing::warn!(peer = %peer, live, "reminder api: refused, at the live ceiling");
        return Err(err(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_live",
            format!(
                "there are already {live} unfinished reminders (the limit is {}); \
                 finish or delete some before adding more",
                spec::MAX_LIVE_PER_PEER
            ),
        ));
    }

    let saved = s
        .app
        .db
        .create_reminder(&peer, None, &planned.text, planned.due_at, &planned.tz_name, None)
        .await
        .map_err(|e| {
            tracing::error!("reminder api: create failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "store_failed", "could not store the reminder")
        })?;

    // id and peer only — the text is the member's own words.
    tracing::info!(id = %saved.id, peer = %peer, "reminder api: created");
    Ok((StatusCode::CREATED, Json(saved)))
}

/// `GET /v1/reminders` — this token's own live reminders, soonest first.
pub async fn list(State(s): State<AxumState>, headers: HeaderMap) -> ApiResult<Vec<Reminder>> {
    let claims = bearer_claims(&s, &headers).await?;
    let peer = bucket(&claims);
    let rows = s.app.db.list_reminders(peer).await.map_err(|e| {
        tracing::error!("reminder api: list failed: {e}");
        err(StatusCode::INTERNAL_SERVER_ERROR, "store_failed", "could not read reminders")
    })?;
    Ok((StatusCode::OK, Json(rows)))
}

#[derive(Deserialize)]
pub struct ActionBody {
    /// `ack`, `snooze` or `delete`.
    action: String,
    /// Only meaningful for `snooze`; 0 means the default.
    #[serde(default)]
    snooze_minutes: u32,
}

#[derive(Serialize)]
pub struct ActionResult {
    /// The updated row, or null after a delete.
    reminder: Option<Reminder>,
}

/// `POST /v1/reminders/:id/action`
pub async fn action(
    State(s): State<AxumState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ActionBody>,
) -> ApiResult<ActionResult> {
    let claims = bearer_claims(&s, &headers).await?;
    let peer = bucket(&claims);

    // Reject unknown verbs here rather than letting the store's catch-all
    // surface as a 500. Note "cancel" is NOT one of them, however natural
    // it sounds — the store calls that "delete".
    if !matches!(body.action.as_str(), "ack" | "snooze" | "delete") {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_action",
            "action must be ack, snooze or delete",
        ));
    }

    let updated = s
        .app
        .db
        .apply_reminder_action(peer, &id, &body.action, body.snooze_minutes)
        .await
        .map_err(|e| {
            // The store says "no live reminder …" for a miss; that is a
            // 404, not a fault.
            let text = e.to_string();
            if text.contains("no live reminder") || text.contains("not found") {
                err(StatusCode::NOT_FOUND, "no_such_reminder", text)
            } else {
                tracing::error!("reminder api: action failed: {e}");
                err(StatusCode::INTERNAL_SERVER_ERROR, "store_failed", "could not apply that")
            }
        })?;

    tracing::info!(id = %id, peer = %peer, action = %body.action, "reminder api: action");
    Ok((StatusCode::OK, Json(ActionResult { reminder: updated })))
}
