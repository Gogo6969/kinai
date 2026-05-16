//! RSA keypair management — generated once per host, persisted to disk.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rsa::pkcs1::EncodeRsaPublicKey;
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::{RsaPrivateKey, RsaPublicKey};
use std::path::PathBuf;

use crate::config::AppConfig;

static CACHED: Mutex<Option<(String, String)>> = Mutex::new(None);

pub fn ensure_keys() -> Result<(String, String)> {
    if let Some(pair) = CACHED.lock().clone() {
        return Ok(pair);
    }
    let dir = AppConfig::keys_dir();
    let _ = std::fs::create_dir_all(&dir);
    let priv_path = dir.join("invite_private.pem");
    let pub_path = dir.join("invite_public.pem");

    if priv_path.exists() && pub_path.exists() {
        let priv_pem = std::fs::read_to_string(&priv_path)?;
        let pub_pem = std::fs::read_to_string(&pub_path)?;
        let pair = (priv_pem, pub_pem);
        *CACHED.lock() = Some(pair.clone());
        return Ok(pair);
    }

    tracing::info!("generating new RSA keypair for invite JWTs");
    let mut rng = rand::thread_rng();
    let priv_key = RsaPrivateKey::new(&mut rng, 2048).context("rsa keygen")?;
    let pub_key = RsaPublicKey::from(&priv_key);

    let priv_pem = priv_key
        .to_pkcs8_pem(LineEnding::LF)
        .context("encode private pem")?
        .to_string();
    let pub_pem = pub_key
        .to_pkcs1_pem(LineEnding::LF)
        .context("encode public pem")?;

    write_secret(&priv_path, &priv_pem)?;
    std::fs::write(&pub_path, &pub_pem)?;

    let pair = (priv_pem, pub_pem);
    *CACHED.lock() = Some(pair.clone());
    Ok(pair)
}

#[cfg(unix)]
fn write_secret(path: &PathBuf, contents: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents)?;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret(path: &PathBuf, contents: &str) -> Result<()> {
    std::fs::write(path, contents)?;
    Ok(())
}
