//! Telegram pairing storage.
//!
//! Two tables (see migrate.rs):
//!   - `telegram_links`         — permanent peer ↔ chat_id mapping
//!   - `telegram_pending_pairs` — short-lived (~10 min) tokens used
//!                                during the QR-scan handshake
//!
//! The bot's `/start <token>` handler atomically moves a row from
//! pending_pairs into links (in `redeem_pair`).

use anyhow::Result;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

/// Pending-pair tokens expire after this many minutes. Long enough for
/// the user to scan + open Telegram; short enough that an intercepted
/// token can't be exploited indefinitely.
const PENDING_PAIR_TTL_MINUTES: i64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramLink {
    pub peer_id: String,
    pub chat_id: String,
    pub username: Option<String>,
    pub first_name: Option<String>,
    pub paired_at: String,
}

/// Create a one-shot pairing token bound to a peer. Returns the token
/// so the caller can build the `https://t.me/<bot>?start=<token>` URL
/// and the QR code.
pub async fn create_pending_pair(pool: &SqlitePool, peer_id: &str) -> Result<String> {
    use rand::Rng;
    // 24 random URL-safe chars — Telegram's start_parameter allows
    // up to 64 chars from [A-Za-z0-9_-].
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
    let mut rng = rand::thread_rng();
    let token: String = (0..24)
        .map(|_| {
            let idx = rng.gen_range(0..ALPHA.len());
            ALPHA[idx] as char
        })
        .collect();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO telegram_pending_pairs (token, peer_id, created_at) VALUES (?1, ?2, ?3)",
    )
    .bind(&token)
    .bind(peer_id)
    .bind(&now)
    .execute(pool)
    .await?;
    // Best-effort: prune expired tokens while we're here. Idempotent.
    let cutoff = (Utc::now() - Duration::minutes(PENDING_PAIR_TTL_MINUTES)).to_rfc3339();
    let _ = sqlx::query("DELETE FROM telegram_pending_pairs WHERE created_at < ?1")
        .bind(&cutoff)
        .execute(pool)
        .await;
    Ok(token)
}

/// Bot received `/start <token>`. Look up the token, confirm it's not
/// expired, and atomically swap the pending pair for a permanent link.
/// Returns the linked peer_id on success, or `None` if the token was
/// invalid or expired.
pub async fn redeem_pair(
    pool: &SqlitePool,
    token: &str,
    chat_id: &str,
    username: Option<&str>,
    first_name: Option<&str>,
) -> Result<Option<String>> {
    let cutoff = (Utc::now() - Duration::minutes(PENDING_PAIR_TTL_MINUTES)).to_rfc3339();
    let row = sqlx::query(
        "SELECT peer_id FROM telegram_pending_pairs
         WHERE token = ?1 AND created_at >= ?2",
    )
    .bind(token)
    .bind(&cutoff)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let peer_id: String = row.get("peer_id");

    let now = Utc::now().to_rfc3339();
    // Upsert: a peer may re-pair from a new Telegram (e.g. lost old
    // phone). PRIMARY KEY peer_id ensures we overwrite the previous
    // mapping rather than leaving two.
    sqlx::query(
        "INSERT INTO telegram_links (peer_id, chat_id, username, first_name, paired_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(peer_id) DO UPDATE SET
             chat_id    = excluded.chat_id,
             username   = excluded.username,
             first_name = excluded.first_name,
             paired_at  = excluded.paired_at",
    )
    .bind(&peer_id)
    .bind(chat_id)
    .bind(username)
    .bind(first_name)
    .bind(&now)
    .execute(pool)
    .await?;

    // Token is single-use; remove regardless of outcome.
    sqlx::query("DELETE FROM telegram_pending_pairs WHERE token = ?1")
        .bind(token)
        .execute(pool)
        .await?;
    Ok(Some(peer_id))
}

/// Reverse lookup: given a Telegram chat_id (incoming message), return
/// the paired peer_id, or `None` if no pairing exists.
pub async fn peer_for_chat(pool: &SqlitePool, chat_id: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT peer_id FROM telegram_links WHERE chat_id = ?1")
        .bind(chat_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("peer_id")))
}

/// Forward lookup: given a peer_id (outgoing reply needs a target),
/// return the chat_id to send to, or `None` if the peer hasn't paired.
pub async fn chat_for_peer(pool: &SqlitePool, peer_id: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT chat_id FROM telegram_links WHERE peer_id = ?1")
        .bind(peer_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("chat_id")))
}

/// Return the link row for a peer, or `None`. Used by the Settings UI
/// to show "Paired as @username · paired 3 days ago" + the unpair btn.
pub async fn link_for_peer(pool: &SqlitePool, peer_id: &str) -> Result<Option<TelegramLink>> {
    let row = sqlx::query(
        "SELECT peer_id, chat_id, username, first_name, paired_at
         FROM telegram_links WHERE peer_id = ?1",
    )
    .bind(peer_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| TelegramLink {
        peer_id: r.get("peer_id"),
        chat_id: r.get("chat_id"),
        username: r.get("username"),
        first_name: r.get("first_name"),
        paired_at: r.get("paired_at"),
    }))
}

/// Drop a pairing — used when the user clicks "Disconnect Telegram" or
/// when the bot receives `/stop` from a paired chat.
pub async fn unpair(pool: &SqlitePool, peer_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM telegram_links WHERE peer_id = ?1")
        .bind(peer_id)
        .execute(pool)
        .await?;
    Ok(())
}
