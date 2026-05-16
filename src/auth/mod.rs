//! JWT (RS256) issuance and validation.
//!
//! At first launch the Host generates a 2048-bit RSA keypair and writes it to
//! `~/.kinai/keys/`. Every invite is a JWT signed with the private key; the
//! Host validates incoming connections against the public key. Clients store
//! the JWT verbatim and present it on every connection.

mod keys;

use anyhow::{anyhow, Result};
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

pub use keys::ensure_keys;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject — short opaque id for the invite (= the short_code).
    pub sub: String,
    /// Issuer — fixed to "kinai".
    pub iss: String,
    /// Audience — host URL.
    pub aud: String,
    /// Issued-at (unix seconds).
    pub iat: i64,
    /// Expires (unix seconds).
    pub exp: i64,
    /// Human label for this invite ("For Mom's iPad").
    pub label: String,
}

/// Issue a JWT for a new invite. Returns the encoded token.
pub fn issue_token(short_code: &str, host_url: &str, label: &str, ttl_days: i64) -> Result<String> {
    let (private_pem, _) = ensure_keys()?;
    let now = Utc::now();
    let claims = Claims {
        sub: short_code.into(),
        iss: "kinai".into(),
        aud: host_url.into(),
        iat: now.timestamp(),
        exp: (now + Duration::days(ttl_days)).timestamp(),
        label: label.into(),
    };
    let header = Header::new(Algorithm::RS256);
    let key = EncodingKey::from_rsa_pem(private_pem.as_bytes())?;
    Ok(encode(&header, &claims, &key)?)
}

/// Validate a JWT and return its claims. Verifies signature, expiry, and audience.
pub fn validate_token(token: &str, expected_host_url: &str) -> Result<Claims> {
    let (_, public_pem) = ensure_keys()?;
    let key = DecodingKey::from_rsa_pem(public_pem.as_bytes())?;
    let mut v = Validation::new(Algorithm::RS256);
    v.set_audience(&[expected_host_url]);
    v.set_issuer(&["kinai"]);
    let data = decode::<Claims>(token, &key, &v).map_err(|e| anyhow!("jwt: {e}"))?;
    Ok(data.claims)
}

/// Decode the JWT claims WITHOUT verifying the signature.
///
/// This is called client-side on an invite-URL paste, on a token signed by
/// a DIFFERENT machine's private key (the host's). The client doesn't have
/// the host's public key yet — that comes later, on the WebSocket connect,
/// where the host itself verifies its own signature.
///
/// Previously this used `ensure_keys()` and validated with the LOCAL public
/// key, which always failed cross-machine ("Invalid signature") and broke
/// the URL-paste pairing flow for any device joining a different host.
pub fn peek_token(token: &str) -> Result<Claims> {
    let mut v = Validation::new(Algorithm::RS256);
    v.set_issuer(&["kinai"]);
    v.validate_aud = false;
    v.insecure_disable_signature_validation();
    // DecodingKey contents are ignored when signature validation is off,
    // but the API still requires a key — use an empty one.
    let key = DecodingKey::from_secret(&[]);
    let data = decode::<Claims>(token, &key, &v).map_err(|e| anyhow!("jwt: {e}"))?;
    Ok(data.claims)
}
