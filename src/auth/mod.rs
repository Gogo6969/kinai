//! JWT (RS256) issuance and validation.
//!
//! At first launch the Host generates a 2048-bit RSA keypair and writes it to
//! `~/.kinai/keys/`. Every invite is a JWT signed with the private key; the
//! Host validates incoming connections against the public key. Clients store
//! the JWT verbatim and present it on every connection.

mod keys;

/// One sandboxed `$HOME` for the whole test binary, created once and
/// never taken away.
///
/// Every test that signs a token needs the keys to come from somewhere
/// that is not the family's real `~/.kinai/keys/`. `$HOME` is
/// process-global and `ensure_keys` caches its answer in another
/// process-global, so two tests that each set `$HOME` and generate a
/// keypair race: the slower one wins the cache, the faster one then
/// validates a token against the other's key and fails with
/// `InvalidSignature` — intermittently, on the gate that decides
/// whether a release ships. And a `TempDir` dropped at the end of one
/// test leaves every test that runs afterwards pointing at a directory
/// that is gone.
///
/// So: one directory, held in a `static` (never dropped), with the
/// keypair generated inside the initialiser, where `OnceLock` blocks
/// every other caller until it is done.
#[cfg(test)]
pub(crate) fn sandbox_home() {
    use std::sync::OnceLock;
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir for the test HOME");
        std::env::set_var("HOME", dir.path());
        // Fill the key cache from THIS home before anyone else looks.
        keys::ensure_keys().expect("generate a keypair in the sandbox");
        dir
    });
}

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
    /// What this token is allowed to be used for. `""` or `"family"` is a
    /// normal family invite that may open a WebSocket and chat;
    /// `"automation"` may only call the HTTP reminder API.
    ///
    /// `#[serde(default)]` is load-bearing. Every token already in
    /// `invites.jwt` and on every family device was minted before this
    /// field existed, and serde would otherwise refuse them with "missing
    /// field" — locking the whole household out at the Hello frame. The
    /// signature itself is unaffected: jsonwebtoken verifies over the raw
    /// payload bytes before deserializing.
    #[serde(default)]
    pub scope: String,
    /// The peer this token writes as, when that is not its own `sub`.
    /// Only `"host"` is meaningful, and only on an automation token: it
    /// lets a script on the host machine write into the bucket the
    /// Calendar actually reads (`db::HOST_PEER`). `""` means "act as
    /// `sub`", which is what every family token does.
    #[serde(default)]
    pub act_as: String,
}

impl Claims {
    /// May this token open a WebSocket and act as a family member?
    ///
    /// The ONE place the scope rule is written down. Do not compare
    /// `scope` inline anywhere else: an obvious-looking
    /// `claims.scope == "family"` rejects every JWT the household already
    /// holds, because `#[serde(default)]` gives those `""`.
    pub fn is_family(&self) -> bool {
        (self.scope.is_empty() || self.scope == FAMILY_SCOPE) && self.act_as.is_empty()
    }

    /// The peer id this token writes as.
    pub fn writes_as(&self) -> &str {
        if self.act_as.is_empty() {
            &self.sub
        } else {
            &self.act_as
        }
    }
}

/// Scope of a normal family invite. Stored explicitly on new rows; older
/// rows and older tokens carry `""` and mean the same thing.
pub const FAMILY_SCOPE: &str = "family";
/// Scope of a token that may only reach the HTTP reminder API.
pub const AUTOMATION_SCOPE: &str = "automation";

/// Issue a JWT for a new family invite. Returns the encoded token.
///
/// Kept at its original four arguments: five live tests and a unit test
/// call it, and a family invite is overwhelmingly the common case.
pub fn issue_token(short_code: &str, host_url: &str, label: &str, ttl_days: i64) -> Result<String> {
    issue_scoped_token(short_code, host_url, label, ttl_days, FAMILY_SCOPE, "")
}

/// Issue a JWT with an explicit scope, and optionally a peer to act as.
///
/// `act_as` is only honoured for non-family scopes — see
/// `Claims::is_family`, which refuses a WebSocket handshake for anything
/// carrying it.
pub fn issue_scoped_token(
    short_code: &str,
    host_url: &str,
    label: &str,
    ttl_days: i64,
    scope: &str,
    act_as: &str,
) -> Result<String> {
    let (private_pem, _) = ensure_keys()?;
    let now = Utc::now();
    let claims = Claims {
        sub: short_code.into(),
        iss: "kinai".into(),
        aud: host_url.into(),
        iat: now.timestamp(),
        exp: (now + Duration::days(ttl_days)).timestamp(),
        label: label.into(),
        scope: scope.into(),
        act_as: act_as.into(),
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

    /// THE regression net for every token the household already holds.
    ///
    /// Every JWT in `invites.jwt`, and the one saved on every family
    /// device, was minted before `scope` and `act_as` existed. Without
    /// `#[serde(default)]` serde refuses them with "missing field" and
    /// every Windows and Linux client — which auto-update after the host
    /// — is locked out at the Hello frame with "invite rejected". No
    /// crypto needed to prove it: the failure is in deserialization.
    #[test]
    fn a_token_minted_before_scope_existed_is_still_a_family_token() {
        let legacy = r#"{
            "sub":"ab23cd","iss":"kinai","aud":"ws://192.0.2.10:4847/kin",
            "iat":1778880000,"exp":1810416000,"label":"For the iPad"
        }"#;
        let claims: Claims = serde_json::from_str(legacy).expect("legacy claims must decode");
        assert_eq!(claims.scope, "", "absent field defaults to empty, NOT \"family\"");
        assert_eq!(claims.act_as, "");
        assert!(claims.is_family(), "an empty scope is a family invite");
        assert_eq!(claims.writes_as(), "ab23cd", "and it writes as itself");
    }

    #[test]
    fn scope_decides_what_a_token_may_do() {
        let family = Claims {
            sub: "ab23cd".into(),
            iss: "kinai".into(),
            aud: "ws://h/kin".into(),
            iat: 0,
            exp: 0,
            label: "l".into(),
            scope: FAMILY_SCOPE.into(),
            act_as: String::new(),
        };
        assert!(family.is_family());
        assert_eq!(family.writes_as(), "ab23cd");

        let automation = Claims {
            scope: AUTOMATION_SCOPE.into(),
            act_as: "host".into(),
            ..family.clone()
        };
        assert!(!automation.is_family(), "automation may not open a socket");
        assert_eq!(automation.writes_as(), "host");

        // A family scope carrying act_as is still refused — otherwise a
        // family token could nominate itself into the host's bucket.
        let smuggled = Claims { act_as: "host".into(), ..family.clone() };
        assert!(!smuggled.is_family(), "act_as is never allowed on a family token");

        // And an unknown scope is not family either.
        let odd = Claims { scope: "something-new".into(), ..family };
        assert!(!odd.is_family());
    }

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
        // Route keys to the shared sandbox HOME rather than setting
        // $HOME here: this is no longer the only test in the binary
        // that signs a token, and two of them setting it independently
        // race over one process-global key cache.
        super::sandbox_home();

        let host_url = "ws://192.0.2.10:4847/kin";
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
