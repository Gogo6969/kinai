//! Invite issuance & validation.
//!
//! Each invite has:
//!   * a 6-character human-typeable short code (lowercase letters + digits)
//!   * a JWT (RS256) signed by the host's keypair
//!   * a host URL
//!   * a label (e.g. "Mom's iPad")
//!   * an expiry (default 30 days, configurable)
//!
//! The shareable string is `kinai://join?host=<url>&code=<short>&token=<jwt>`.
//! The QR code embeds the same URL.

use anyhow::{anyhow, bail, Result};
use chrono::{Duration, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::auth;
use crate::config::AppConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invite {
    pub id: String,
    pub short_code: String,
    pub jwt: String,
    pub host_url: String,
    pub label: String,
    pub join_url: String,
    pub qr_payload: String,
    pub created_at: String,
    pub expires_at: String,
    pub revoked: bool,
    /// `"family"` (or `""` on rows written before scopes existed) for a
    /// device that may chat; `"automation"` for a reminder API key.
    ///
    /// The Invites page shows both — they share this table so one Revoke
    /// button covers them — and needs to tell them apart: a key is not a
    /// family member's device, its 6-character code is deliberately not
    /// redeemable, and its QR carries a token that acts as the host.
    #[serde(default)]
    pub scope: String,
}

/// Longest invite label accepted, in characters. It is a name for a
/// device ("Mum's iPad"), not a note, and it has to fit on one line of
/// the invite card and beside a member's own name on Manage family.
pub const MAX_LABEL_CHARS: usize = 60;

/// Trim a label and refuse the two shapes that make a list unreadable:
/// nothing at all, and an essay. Shared by create and rename so the two
/// cannot drift apart.
pub fn normalize_label(raw: &str) -> Result<String> {
    // Every place this is shown — the invite card, Manage family, the
    // name on a reported answer — is one line of somebody's identity. A
    // newline would break the line in two and a bidi override would
    // reorder the text around it, so control and formatting characters
    // become spaces rather than being refused: the host typed a name,
    // not an attack, and a paste from a document should just work.
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() || matches!(c, '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') { ' ' } else { c })
        .collect();
    // Collapse the runs those substitutions leave behind.
    let label = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if label.is_empty() {
        bail!("give the device a name so you can tell it apart later");
    }
    if label.chars().count() > MAX_LABEL_CHARS {
        bail!("that name is too long (max {MAX_LABEL_CHARS} characters)");
    }
    Ok(label)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedInvite {
    pub host_url: String,
    pub token: String,
    pub label: String,
}

/// The `kinai://join?…` string a device redeems: host, code, token.
///
/// One definition, because it is built in three places — at creation, when
/// listing invites, and now on Manage family, which shows the code beside
/// the device it belongs to. A copy that drifted would hand somebody a QR
/// that does not pair.
pub fn join_url(host_url: &str, short_code: &str, jwt: &str) -> String {
    format!(
        "kinai://join?host={}&code={}&token={}",
        urlencode(host_url),
        urlencode(short_code),
        urlencode(jwt)
    )
}

/// Create a normal family invite: may open a WebSocket and chat.
///
/// Signature deliberately unchanged — five live tests and the Tauri
/// command call it, and this is overwhelmingly the common case.
pub async fn create(pool: &SqlitePool, cfg: &AppConfig, label: &str, ttl_days: i64) -> Result<Invite> {
    create_scoped(pool, cfg, label, ttl_days, auth::FAMILY_SCOPE, "").await
}

/// Create an invite with an explicit scope.
///
/// `scope = "automation"` with `act_as = "host"` is the API-key case: a
/// token that may call the HTTP reminder API writing into the host's own
/// bucket, and may NOT open a WebSocket. Such a row is also refused by
/// `lookup_by_short_code`, so its 6-character code is never redeemable
/// over the network — the token has to be copied out of the host UI by
/// hand. Without that refusal a guessed code would hand out a credential
/// strictly more powerful than any family invite.
pub async fn create_scoped(
    pool: &SqlitePool,
    cfg: &AppConfig,
    label: &str,
    ttl_days: i64,
    scope: &str,
    act_as: &str,
) -> Result<Invite> {
    // Same gate as rename: a nameless or essay-length invite is as
    // unreadable in the list on the day it is made as a week later.
    let label = normalize_label(label)?;
    let label = label.as_str();
    let id = Uuid::new_v4().to_string();
    let short_code = random_short_code(6);
    let host_url = guess_host_url(cfg);
    // `ttl_days <= 0` is the sentinel for "expires never" — we encode
    // it as a ~100-year TTL so the JWT's `exp` claim stays valid for
    // any realistic family-member lifetime, AND the DB row's
    // `expires_at` column lands far enough in the future that the UI
    // can recognise it as a never-expiring invite (renders as "Never"
    // instead of a date). Picking 100 years (vs i64::MAX) keeps the
    // JWT spec-conformant — `exp` is a Unix second count and some
    // libraries reject extreme values.
    let effective_ttl = if ttl_days <= 0 { 36500 } else { ttl_days };
    let jwt = auth::issue_scoped_token(&short_code, &host_url, label, effective_ttl, scope, act_as)?;
    let now = Utc::now();
    let exp = now + Duration::days(effective_ttl);

    let join_url = join_url(&host_url, &short_code, &jwt);

    sqlx::query(
        "INSERT INTO invites (id, short_code, jwt, host_url, label, created_at, expires_at, scope)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(&id)
    .bind(&short_code)
    .bind(&jwt)
    .bind(&host_url)
    .bind(label)
    .bind(now.to_rfc3339())
    .bind(exp.to_rfc3339())
    .bind(scope)
    .execute(pool)
    .await?;

    Ok(Invite {
        id,
        short_code: short_code.clone(),
        jwt: jwt.clone(),
        host_url: host_url.clone(),
        label: label.into(),
        join_url: join_url.clone(),
        qr_payload: join_url,
        created_at: now.to_rfc3339(),
        expires_at: exp.to_rfc3339(),
        revoked: false,
        scope: scope.into(),
    })
}

/// Rename a device from the Invites page.
///
/// Only the DB column moves. The label is ALSO a claim inside the
/// already-issued JWT, on the family's devices and inside the QR they
/// scanned — re-signing it would mint a second valid token for the same
/// code and silently change a QR someone had printed out. Instead
/// `validate_jwt_for_host` reads the current label from this column at
/// every handshake, so the rename reaches the one place the claim was
/// used (the name on a reported answer) without touching a token.
///
/// `revoked = 0` in the WHERE: renaming something already taken away is
/// not a thing to support, and the page does not offer it.
///
/// Errors when nothing matched. `execute` reports `Ok` with zero rows
/// for an id that does not exist, so without the check a stale row id
/// would look exactly like a successful rename.
pub async fn rename(pool: &SqlitePool, id: &str, label: &str) -> Result<()> {
    let label = normalize_label(label)?;
    let res = sqlx::query("UPDATE invites SET label = ?2 WHERE id = ?1 AND revoked = 0")
        .bind(id)
        .bind(&label)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        bail!("that invite is gone or has been revoked — refresh the page");
    }
    Ok(())
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<Invite>> {
    let rows = sqlx::query(
        "SELECT id, short_code, jwt, host_url, label, created_at, expires_at, revoked, scope
         FROM invites ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_invite).collect())
}

pub async fn revoke(pool: &SqlitePool, id: &str) -> Result<()> {
    // Stamp `revoked_at` so `cleanup_stale` can purge this row a grace
    // period from now rather than keying off `created_at` (which could be
    // months old, making a just-revoked invite vanish immediately).
    sqlx::query("UPDATE invites SET revoked = 1, revoked_at = ?2 WHERE id = ?1")
        .bind(id)
        .bind(Utc::now().to_rfc3339())
        .execute(pool)
        .await?;
    Ok(())
}

/// Revoke by short code rather than row id.
///
/// Manage Family knows a member by their invite's short code (it is also
/// their `peer_id` on every thread, fact and reminder), not by the row id
/// the Invites page uses.
///
/// Errors when nothing matched. `sqlx::execute` reports `Ok` with zero
/// rows affected for a key that does not exist, so a silent typo would
/// otherwise look exactly like a successful revoke — and the caller would
/// tell the host the member was removed while their code kept working.
pub async fn revoke_by_short_code(pool: &SqlitePool, short_code: &str) -> Result<()> {
    let done = sqlx::query("UPDATE invites SET revoked = 1, revoked_at = ?2 WHERE short_code = ?1")
        .bind(short_code)
        .bind(Utc::now().to_rfc3339())
        .execute(pool)
        .await?;
    if done.rows_affected() == 0 {
        return Err(anyhow!("no invite with that code"));
    }
    Ok(())
}

/// Hard-delete invites that have been dead long enough to stop showing.
/// An invite is "stale" once it has been *revoked* or *expired* for more
/// than `grace_days`:
///
///   * revoked  → keyed off `revoked_at` (falling back to `created_at`
///     for rows revoked before that column existed), so a freshly revoked
///     invite still lingers for the grace window as a visible record.
///   * expired  → keyed off the existing `expires_at`. "Never expires"
///     invites sit ~100 years out, so they're never caught here unless
///     they've also been revoked.
///
/// Timestamps are compared as RFC3339 strings — the same lexicographic =
/// chronological assumption `list`'s `ORDER BY created_at` already relies
/// on (all are UTC `+00:00`). Returns the number of rows removed.
pub async fn cleanup_stale(pool: &SqlitePool, grace_days: i64) -> Result<u64> {
    let cutoff = (Utc::now() - Duration::days(grace_days)).to_rfc3339();
    let result = sqlx::query(
        "DELETE FROM invites
         WHERE (revoked = 1 AND COALESCE(revoked_at, created_at) < ?1)
            OR (revoked = 0 AND expires_at < ?1)",
    )
    .bind(&cutoff)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub async fn validate_jwt_for_host(
    pool: &SqlitePool,
    token: &str,
    expected_host_url: &str,
) -> Result<auth::Claims> {
    let mut claims = auth::validate_token(token, expected_host_url)?;
    let row = sqlx::query("SELECT revoked, label FROM invites WHERE short_code = ?1")
        .bind(&claims.sub)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else {
        return Err(anyhow!("invite not on this host"));
    };
    let revoked: i64 = row.get("revoked");
    if revoked != 0 {
        return Err(anyhow!("invite revoked"));
    }
    // The label in the token is whatever it was called the day it was
    // minted; the column is what the host calls it now. The host renames
    // a device precisely so it reads correctly elsewhere — and the one
    // consumer of this claim is the name on a reported answer — so the
    // current name wins. The token is never re-signed for this.
    let label: String = row.get("label");
    if !label.is_empty() {
        claims.label = label;
    }
    Ok(claims)
}

/// Look up an invite by its 6-character short code. Used by the
/// `/v1/invite/redeem` endpoint so clients on the LAN can type the code
/// and the host resolves it to a full JWT.
pub async fn lookup_by_short_code(pool: &SqlitePool, code: &str) -> Result<ResolvedInvite> {
    use chrono::Utc;
    let row = sqlx::query(
        "SELECT jwt, host_url, label, expires_at, revoked, scope
         FROM invites WHERE short_code = ?1",
    )
    .bind(code)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Err(anyhow!("invite code not found"));
    };
    // Only a family invite is redeemable over the network. An automation
    // key lives in the same table so the existing Revoke UI covers it, but
    // its token is strictly more powerful than a family one — it can carry
    // `act_as: "host"` — and it is never typed by a person, so there is no
    // reason for a code to resolve to it. The error is byte-identical to
    // the not-found case on purpose: a distinguishable message would turn
    // this into an oracle for which codes exist.
    let scope: String = row.get("scope");
    if !scope.is_empty() && scope != auth::FAMILY_SCOPE {
        return Err(anyhow!("invite code not found"));
    }
    let revoked: i64 = row.get("revoked");
    if revoked != 0 {
        return Err(anyhow!("invite has been revoked"));
    }
    // Fail CLOSED on a timestamp we cannot read. This used to be a bare
    // `if let Ok(dt) = …` with no else, so a row whose `expires_at` did
    // not parse skipped the expiry check altogether — the one column
    // deciding whether a code still works was also the one whose
    // unreadability made it stop mattering.
    //
    // Safe for existing households: every stored `expires_at` is written
    // by `create_scoped` as RFC3339, including the ~100-year "never"
    // sentinel, so nothing legitimate lands here.
    let expires_at: String = row.get("expires_at");
    match chrono::DateTime::parse_from_rfc3339(&expires_at) {
        Ok(dt) if dt < Utc::now() => return Err(anyhow!("invite has expired")),
        Ok(_) => {}
        Err(e) => {
            tracing::warn!("invite has an unreadable expiry, refusing it: {e}");
            return Err(anyhow!("invite has expired"));
        }
    }
    Ok(ResolvedInvite {
        host_url: row.get("host_url"),
        token: row.get("jwt"),
        label: row.get("label"),
    })
}

pub fn parse_join_url(input: &str) -> Result<ResolvedInvite> {
    let trimmed = input.trim();
    let qs = trimmed
        .strip_prefix("kinai://join?")
        .ok_or_else(|| anyhow!("Not a KinAI join URL. Expected kinai://join?…, or a 6-character invite code."))?;
    let mut host = None;
    let mut token = None;
    let mut label = String::new();
    for pair in qs.split('&') {
        let mut kv = pair.splitn(2, '=');
        match (kv.next(), kv.next()) {
            (Some("host"), Some(v)) => host = Some(urldecode(v)),
            (Some("token"), Some(v)) => token = Some(urldecode(v)),
            (Some("code"), Some(v)) => label = urldecode(v),
            _ => {}
        }
    }
    let host = host.ok_or_else(|| anyhow!("missing host"))?;
    let token = token.ok_or_else(|| anyhow!("missing token"))?;
    let claims = auth::peek_token(&token)?;
    Ok(ResolvedInvite {
        host_url: host,
        token,
        label: if claims.label.is_empty() { label } else { claims.label },
    })
}

fn row_to_invite(r: sqlx::sqlite::SqliteRow) -> Invite {
    let host_url: String = r.get("host_url");
    let jwt: String = r.get("jwt");
    let short_code: String = r.get("short_code");
    let join_url = join_url(&host_url, &short_code, &jwt);
    let revoked: i64 = r.get("revoked");
    Invite {
        id: r.get("id"),
        short_code,
        jwt,
        host_url,
        label: r.get("label"),
        join_url: join_url.clone(),
        qr_payload: join_url,
        created_at: r.get("created_at"),
        expires_at: r.get("expires_at"),
        revoked: revoked != 0,
        // `try_get`, not `get`: a caller that selects the older column
        // set gets `""` instead of a panic. It also swallows a real
        // decode error the same way — acceptable because every consumer
        // treats an unknown scope as "an ordinary family invite", which
        // is what a row without the column always was.
        scope: r.try_get("scope").unwrap_or_default(),
    }
}

fn random_short_code(n: usize) -> String {
    const ALPHA: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..n)
        .map(|_| ALPHA[rng.gen_range(0..ALPHA.len())] as char)
        .collect()
}

/// Canonical reachable host URL for both invite issuance and JWT
/// validation. `bind_addr` is a listen spec (often `0.0.0.0`), not an
/// address a peer can connect to — so for the JWT audience claim and for
/// the reachable URL we publish over mDNS we always use the machine's
/// LAN IP. Exposed (pub) so the server's `start` function uses the exact
/// same string when storing `stats.host_url`, which is then handed to
/// `validate_jwt_for_host`. If these two strings ever diverged, every
/// Hello frame would be rejected with "InvalidAudience".
pub fn public_host_url(cfg: &AppConfig) -> String {
    let host = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| cfg.host.bind_addr.clone());
    format!("ws://{host}:{}/kin", cfg.host.port)
}

fn guess_host_url(cfg: &AppConfig) -> String {
    public_host_url(cfg)
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00"),
                16,
            ) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        crate::db::migrate::run(&pool).await.expect("apply migrations");
        pool
    }

    fn days_ago(n: i64) -> String {
        (Utc::now() - Duration::days(n)).to_rfc3339()
    }
    fn days_ahead(n: i64) -> String {
        (Utc::now() + Duration::days(n)).to_rfc3339()
    }

    /// Insert a bare invite row with full control over the lifecycle
    /// timestamps — bypasses `create()` so the test doesn't need signing
    /// keys or a config, and can place rows arbitrarily in the past.
    async fn seed(
        pool: &SqlitePool,
        id: &str,
        created_at: &str,
        expires_at: &str,
        revoked: i64,
        revoked_at: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO invites
               (id, short_code, jwt, host_url, label, created_at, expires_at, revoked, revoked_at)
             VALUES (?1, ?2, 'jwt', 'ws://h/kin', 'L', ?3, ?4, ?5, ?6)",
        )
        .bind(id)
        .bind(id) // short_code is UNIQUE — reuse the id
        .bind(created_at)
        .bind(expires_at)
        .bind(revoked)
        .bind(revoked_at)
        .execute(pool)
        .await
        .expect("seed invite");
    }

    /// Same, with an explicit scope.
    async fn seed_scoped(pool: &SqlitePool, id: &str, scope: &str) {
        sqlx::query(
            "INSERT INTO invites
               (id, short_code, jwt, host_url, label, created_at, expires_at, revoked, scope)
             VALUES (?1, ?2, 'jwt', 'ws://h/kin', 'L', ?3, ?4, 0, ?5)",
        )
        .bind(id)
        .bind(id)
        .bind(days_ago(1))
        .bind(days_ahead(30))
        .bind(scope)
        .execute(pool)
        .await
        .expect("seed scoped invite");
    }

    /// An automation key lives in `invites` so the Revoke UI covers it,
    /// but its code must never resolve over the network: the token it
    /// holds can carry `act_as: "host"`, which is strictly more than any
    /// family invite grants. A guessed code that returned one would be an
    /// escalation, not merely a breach of one member.
    #[tokio::test]
    async fn a_code_never_resolves_to_an_automation_key() {
        let pool = fresh_pool().await;
        seed_scoped(&pool, "autoxx", crate::auth::AUTOMATION_SCOPE).await;
        seed_scoped(&pool, "family", crate::auth::FAMILY_SCOPE).await;

        // A family invite still works, scope column and all.
        let ok = lookup_by_short_code(&pool, "family").await.expect("family resolves");
        assert_eq!(ok.token, "jwt");

        // The automation one is refused, and refused INDISTINGUISHABLY
        // from a code that does not exist — a different message would
        // turn this into an oracle for which codes are real.
        let refused = lookup_by_short_code(&pool, "autoxx").await.unwrap_err().to_string();
        let absent = lookup_by_short_code(&pool, "zzzzzz").await.unwrap_err().to_string();
        assert_eq!(refused, absent, "must be byte-identical to not-found");
        assert_eq!(refused, "invite code not found");
    }

    /// Revoking by code must report a miss rather than pretending.
    /// `sqlx::execute` returns Ok with zero rows affected for a key that
    /// does not exist, so without the rows_affected check a typo would
    /// look exactly like a successful revoke — and Manage family would
    /// tell the host someone was removed while their code kept working.
    #[tokio::test]
    async fn revoking_a_code_that_is_not_there_is_an_error() {
        let pool = fresh_pool().await;
        seed_scoped(&pool, "realco", crate::auth::FAMILY_SCOPE).await;

        revoke_by_short_code(&pool, "realco").await.expect("the real one revokes");
        assert!(
            lookup_by_short_code(&pool, "realco").await.is_err(),
            "revoked codes stop resolving"
        );
        assert!(
            revoke_by_short_code(&pool, "nosuch").await.is_err(),
            "a code that does not exist must not report success"
        );
    }

    /// Rows written before the scope column existed carry the DEFAULT,
    /// and must keep working.
    #[tokio::test]
    async fn a_row_from_before_the_scope_column_still_redeems() {
        let pool = fresh_pool().await;
        // `seed` does not mention scope at all — exactly like every row
        // already in a household's database.
        seed(&pool, "legacy", &days_ago(2), &days_ahead(30), 0, None).await;
        let r = lookup_by_short_code(&pool, "legacy").await.expect("legacy row resolves");
        assert_eq!(r.token, "jwt");
    }

    async fn ids(pool: &SqlitePool) -> Vec<String> {
        list(pool).await.unwrap().into_iter().map(|i| i.id).collect()
    }

    #[tokio::test]
    async fn cleanup_purges_only_long_dead_invites() {
        let pool = fresh_pool().await;
        // Survivors:
        seed(&pool, "active", &days_ago(1), &days_ahead(30), 0, None).await;
        seed(&pool, "expired_recent", &days_ago(10), &days_ago(2), 0, None).await;
        seed(&pool, "revoked_recent", &days_ago(40), &days_ahead(30), 1, Some(&days_ago(2))).await;
        seed(&pool, "never", &days_ago(1), &days_ahead(36500), 0, None).await;
        // Should be purged (dead > 7d):
        seed(&pool, "expired_old", &days_ago(40), &days_ago(30), 0, None).await;
        seed(&pool, "revoked_old", &days_ago(40), &days_ahead(30), 1, Some(&days_ago(30))).await;
        // Legacy revoked row (pre-migration): revoked_at NULL, falls back
        // to created_at, which is old → purged.
        seed(&pool, "revoked_legacy", &days_ago(40), &days_ahead(30), 1, None).await;

        let removed = cleanup_stale(&pool, 7).await.unwrap();
        assert_eq!(removed, 3, "exactly the three long-dead invites go");

        let mut remaining = ids(&pool).await;
        remaining.sort();
        assert_eq!(
            remaining,
            vec!["active", "expired_recent", "never", "revoked_recent"],
        );
    }

    #[tokio::test]
    async fn revoke_stamps_revoked_at() {
        let pool = fresh_pool().await;
        seed(&pool, "x", &days_ago(40), &days_ahead(30), 0, None).await;
        revoke(&pool, "x").await.unwrap();
        // Just-revoked → revoked_at is "now", so a 7-day sweep must NOT
        // delete it even though created_at is 40 days old.
        let removed = cleanup_stale(&pool, 7).await.unwrap();
        assert_eq!(removed, 0, "a freshly revoked invite survives the grace window");
        assert_eq!(ids(&pool).await, vec!["x"]);
    }

    async fn label_of(pool: &SqlitePool, id: &str) -> String {
        sqlx::query("SELECT label FROM invites WHERE id = ?1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("row")
            .get("label")
    }

    #[tokio::test]
    async fn renaming_a_device_changes_only_its_name() {
        let pool = fresh_pool().await;
        seed(&pool, "ipad", &days_ago(1), &days_ahead(30), 0, None).await;
        let before = list(&pool).await.unwrap().into_iter().next().unwrap();

        rename(&pool, "ipad", "  Mum's iPad  ").await.unwrap();

        assert_eq!(label_of(&pool, "ipad").await, "Mum's iPad", "trimmed");
        let after = list(&pool).await.unwrap().into_iter().next().unwrap();
        assert_eq!(after.label, "Mum's iPad");
        // The credential itself is untouched: the token is not re-signed
        // and the QR someone already scanned still reads the same.
        assert_eq!(after.jwt, before.jwt, "no new token");
        assert_eq!(after.short_code, before.short_code);
        assert_eq!(after.join_url, before.join_url, "a printed QR keeps working");
        assert_eq!(after.expires_at, before.expires_at);
        assert!(!after.revoked);
    }

    #[tokio::test]
    async fn a_rename_that_cannot_land_says_so() {
        let pool = fresh_pool().await;
        seed(&pool, "live", &days_ago(1), &days_ahead(30), 0, None).await;
        seed(&pool, "gone", &days_ago(1), &days_ahead(30), 1, Some(&days_ago(1))).await;

        // Empty and over-long are refused before any SQL runs.
        assert!(rename(&pool, "live", "   ").await.is_err(), "nameless");
        assert!(rename(&pool, "live", &"x".repeat(MAX_LABEL_CHARS + 1)).await.is_err(), "essay");
        assert!(rename(&pool, "live", &"x".repeat(MAX_LABEL_CHARS)).await.is_ok(), "at the cap");

        // A revoked row and a row that never existed are the SAME
        // refusal, word for word: a different message for each would
        // tell whoever is asking which invite ids exist.
        let revoked = rename(&pool, "gone", "Nice try").await.unwrap_err().to_string();
        let missing = rename(&pool, "no-such-id", "Nice try").await.unwrap_err().to_string();
        assert_eq!(revoked, missing, "must not say which of the two it was");
        assert_eq!(revoked, "that invite is gone or has been revoked — refresh the page");
        assert_eq!(label_of(&pool, "gone").await, "L", "and untouched");
    }

    #[test]
    fn a_name_is_tidied_or_refused() {
        assert_eq!(normalize_label("  Mum's iPad  ").unwrap(), "Mum's iPad");
        assert!(normalize_label("").is_err(), "nothing");
        assert!(normalize_label(" \t ").is_err(), "whitespace only");
        assert_eq!(normalize_label(&"x".repeat(MAX_LABEL_CHARS)).unwrap().chars().count(), MAX_LABEL_CHARS);
        assert!(normalize_label(&"x".repeat(MAX_LABEL_CHARS + 1)).is_err(), "one over");
        // Counted in characters, not bytes: an emoji is one name's worth
        // of one character, not four.
        assert!(normalize_label(&"🎈".repeat(MAX_LABEL_CHARS)).is_ok());

        // A name is one line. A newline would break the invite card in
        // two and a bidi override would reorder the text around it.
        assert_eq!(normalize_label("Kitchen\niPad").unwrap(), "Kitchen iPad");
        assert_eq!(normalize_label("Tab\there").unwrap(), "Tab here");
        let bidi = normalize_label("Sofa\u{202e}drapi").unwrap();
        assert!(!bidi.contains('\u{202e}'), "no reordering marks: {bidi:?}");
        assert!(normalize_label("\u{202e}\u{202e}").is_err(), "nothing but marks is nothing");
    }

    /// `list` is what the Invites page reads, and the page tells a
    /// reminder API key apart from somebody's device on this one field.
    /// Drop `scope` from the SELECT and `try_get` quietly reports `""`
    /// for every row — so without this, a key would look like a device.
    #[tokio::test]
    async fn the_list_says_which_rows_are_api_keys() {
        let pool = fresh_pool().await;
        seed_scoped(&pool, "device", auth::FAMILY_SCOPE).await;
        seed_scoped(&pool, "key", auth::AUTOMATION_SCOPE).await;

        let rows = list(&pool).await.unwrap();
        let scope_of = |id: &str| {
            rows.iter().find(|r| r.id == id).map(|r| r.scope.clone()).unwrap_or_default()
        };
        assert_eq!(scope_of("device"), auth::FAMILY_SCOPE);
        assert_eq!(scope_of("key"), auth::AUTOMATION_SCOPE);
    }

    /// Rows created before labels were ever trimmed can hold `''`. The
    /// handshake must leave the token's own label alone for those —
    /// overwriting it with nothing would take away the only name a
    /// reported answer had.
    #[tokio::test]
    async fn a_blank_stored_name_does_not_erase_the_token_s_own() {
        let pool = fresh_pool().await;
        auth::sandbox_home();
        let host = "ws://192.0.2.10:4847/kin";
        let token = auth::issue_token("OLD999", host, "From the token", 30).unwrap();
        sqlx::query(
            "INSERT INTO invites
               (id, short_code, jwt, host_url, label, created_at, expires_at, revoked)
             VALUES ('legacy', 'OLD999', ?1, ?2, '', ?3, ?4, 0)",
        )
        .bind(&token)
        .bind(host)
        .bind(days_ago(1))
        .bind(days_ahead(30))
        .execute(&pool)
        .await
        .unwrap();

        let claims = validate_jwt_for_host(&pool, &token, host).await.unwrap();
        assert_eq!(claims.label, "From the token", "a blank column takes nothing away");
    }

    /// The label is minted twice — this column and a claim inside the
    /// device's JWT — and only the column can change. The handshake
    /// therefore reads the column, so the one place the claim was used
    /// (the name on a reported answer) follows a rename instead of
    /// repeating whatever the device was called on the day it joined.
    #[tokio::test]
    async fn the_handshake_reports_the_current_name_not_the_minted_one() {
        let pool = fresh_pool().await;
        auth::sandbox_home();
        let host = "ws://192.0.2.10:4847/kin";

        let token = auth::issue_token("ABC123", host, "Old name", 30).unwrap();
        sqlx::query(
            "INSERT INTO invites
               (id, short_code, jwt, host_url, label, created_at, expires_at, revoked)
             VALUES ('i1', 'ABC123', ?1, ?2, 'Old name', ?3, ?4, 0)",
        )
        .bind(&token)
        .bind(host)
        .bind(days_ago(1))
        .bind(days_ahead(30))
        .execute(&pool)
        .await
        .unwrap();

        let claims = validate_jwt_for_host(&pool, &token, host).await.unwrap();
        assert_eq!(claims.label, "Old name", "before the rename");

        rename(&pool, "i1", "New name").await.unwrap();
        let claims = validate_jwt_for_host(&pool, &token, host).await.unwrap();
        assert_eq!(claims.label, "New name", "the same unchanged token, renamed");
        assert_eq!(claims.sub, "ABC123", "still the same member");
    }
}
