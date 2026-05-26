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
/// Uses jsonwebtoken v10's `dangerous::insecure_decode` (the v9
/// `Validation::insecure_disable_signature_validation()` flow is
/// deprecated). The new API skips ALL validations (issuer, aud,
/// signature, exp) — we re-check the issuer manually here so a bogus
/// token at URL-paste time fails fast with a clear message rather than
/// getting deferred to the WS-connect step. Signature is still
/// re-verified there with the host's real public key.
pub fn peek_token(token: &str) -> Result<Claims> {
    let data = jsonwebtoken::dangerous::insecure_decode::<Claims>(token)
        .map_err(|e| anyhow!("jwt: {e}"))?;
    if data.claims.iss != "kinai" {
        anyhow::bail!("jwt: unexpected issuer '{}'", data.claims.iss);
    }
    Ok(data.claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test that the JWT subsystem actually works end-to-end —
    /// issue → validate → peek round-trip. Exists specifically to catch
    /// silent breakage like the v0.2.36 → v0.2.39 regression: a
    /// jsonwebtoken-v10 bump compiled cleanly but every encode/decode
    /// panicked at runtime with "Could not automatically determine
    /// the process-level CryptoProvider" because the crate's crypto
    /// backend is now an optional feature. The panic only fired on
    /// tokio worker threads, far from any console the user could see,
    /// and the IPC just hung forever. This test guarantees a CI
    /// failure before any release ships with a broken JWT path.
    ///
    /// Uses a tempdir for the keys so the test doesn't touch the
    /// user's real ~/.kinai/keys/ directory.
    #[test]
    fn jwt_roundtrip_smoketest() {
        // Route keys to a tempdir so we don't read/write the user's
        // real ~/.kinai/keys/ during tests, and we exercise the
        // key-generation path too.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("HOME", tmp.path());
        // ensure_keys caches after first call; reset its cache by
        // using a fresh process if the cache layer changes — for now
        // tests run in the same process and the cache is fine because
        // the tempdir keys persist for the test lifetime.

        let host_url = "ws://192.168.1.1:4847/kin";
        let token = issue_token("ABC123", host_url, "test-label", 30)
            .expect("issue_token must succeed (CryptoProvider feature flag check)");
        assert!(!token.is_empty(), "issued token should be non-empty");

        // The big one: validate_token must succeed against the same
        // host_url with a properly-signed token.
        let claims = validate_token(&token, host_url)
            .expect("validate_token must succeed on a token we just issued");
        assert_eq!(claims.sub, "ABC123");
        assert_eq!(claims.iss, "kinai");
        assert_eq!(claims.aud, host_url);
        assert_eq!(claims.label, "test-label");

        // peek_token (no signature check, used on URL paste client-side)
        // should also succeed.
        let peeked = peek_token(&token).expect("peek_token must succeed");
        assert_eq!(peeked.sub, "ABC123");

        // Wrong audience must be rejected — this checks the validation
        // pipeline isn't being a no-op.
        let bad = validate_token(&token, "ws://wrong-host:4847/kin");
        assert!(bad.is_err(), "wrong audience must fail validation");
    }
}
