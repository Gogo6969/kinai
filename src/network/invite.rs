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

use anyhow::{anyhow, Result};
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedInvite {
    pub host_url: String,
    pub token: String,
    pub label: String,
}

pub async fn create(pool: &SqlitePool, cfg: &AppConfig, label: &str, ttl_days: i64) -> Result<Invite> {
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
    let jwt = auth::issue_token(&short_code, &host_url, label, effective_ttl)?;
    let now = Utc::now();
    let exp = now + Duration::days(effective_ttl);

    let join_url = format!(
        "kinai://join?host={}&code={}&token={}",
        urlencode(&host_url),
        urlencode(&short_code),
        urlencode(&jwt)
    );

    sqlx::query(
        "INSERT INTO invites (id, short_code, jwt, host_url, label, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(&id)
    .bind(&short_code)
    .bind(&jwt)
    .bind(&host_url)
    .bind(label)
    .bind(now.to_rfc3339())
    .bind(exp.to_rfc3339())
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
    })
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<Invite>> {
    let rows = sqlx::query(
        "SELECT id, short_code, jwt, host_url, label, created_at, expires_at, revoked
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
    let claims = auth::validate_token(token, expected_host_url)?;
    let row = sqlx::query("SELECT revoked FROM invites WHERE short_code = ?1")
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
    Ok(claims)
}

/// Look up an invite by its 6-character short code. Used by the
/// `/v1/invite/redeem` endpoint so clients on the LAN can type the code
/// and the host resolves it to a full JWT.
pub async fn lookup_by_short_code(pool: &SqlitePool, code: &str) -> Result<ResolvedInvite> {
    use chrono::Utc;
    let row = sqlx::query(
        "SELECT jwt, host_url, label, expires_at, revoked
         FROM invites WHERE short_code = ?1",
    )
    .bind(code)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Err(anyhow!("invite code not found"));
    };
    let revoked: i64 = row.get("revoked");
    if revoked != 0 {
        return Err(anyhow!("invite has been revoked"));
    }
    let expires_at: String = row.get("expires_at");
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&expires_at) {
        if dt < Utc::now() {
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
    let join_url = format!(
        "kinai://join?host={}&code={}&token={}",
        urlencode(&host_url),
        urlencode(&short_code),
        urlencode(&jwt)
    );
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
}
