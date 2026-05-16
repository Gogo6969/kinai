//! Host-side update distribution.
//!
//! The deploy script stages signed update tarballs under
//! `~/.kinai/updates/<version>/<target>/`. This module exposes them
//! over HTTP so clients can use the host as their update source instead
//! of (or in addition to) GitHub Releases. Three endpoints:
//!
//!   GET /v1/update/manifest             -> Tauri-format JSON manifest
//!   GET /v1/update/bundle.tar.gz        -> streams the latest bundle
//!   GET /v1/update/bundle.tar.gz.sig    -> Minisign signature
//!
//! All three trust LAN locality + the JWT pairing as the security
//! boundary at the transport layer; the Tauri updater plugin verifies
//! the Minisign signature against the pubkey baked into every client
//! binary, so even a compromised host can't push an unsigned update.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use tokio::io::AsyncReadExt;

use super::server::AxumState;

const TARBALL_NAME: &str = "KinAI.app.tar.gz";
const SIGNATURE_NAME: &str = "KinAI.app.tar.gz.sig";

#[derive(Serialize)]
pub struct UpdateManifest {
    /// Version of the staged bundle. Tauri's updater compares this to
    /// the running binary's CARGO_PKG_VERSION; updates only trigger
    /// when this is strictly newer.
    pub version: String,
    /// URL the client should GET to download the bundle. Returned as an
    /// absolute URL so we don't have to coordinate base-path assumptions
    /// with the client side.
    pub url: String,
    /// Minisign signature of the tarball, as plain text (matches what
    /// Tauri signer emits). Embedding in the manifest avoids an extra
    /// round-trip — Tauri's updater accepts it either way.
    pub signature: String,
    /// RFC3339 timestamp of when this bundle was staged. Useful for
    /// "Last seen" debugging UIs and for skipping a same-version re-poll.
    pub pub_date: String,
    /// Free-form release notes. Empty for now.
    pub notes: String,
}

fn updates_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kinai")
        .join("updates")
}

fn target_id() -> &'static str {
    // Tauri's updater uses these strings for the platforms key. The host
    // only serves bundles built for its own architecture today — a Mac
    // mini host won't have x86_64 binaries for a hypothetical Intel
    // client. Cross-arch shipping is a Phase 2 problem.
    if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        "darwin-aarch64"
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "x86_64") {
        "darwin-x86_64"
    } else if cfg!(target_os = "windows") {
        "windows-x86_64"
    } else {
        "linux-x86_64"
    }
}

/// Resolve `~/.kinai/updates/latest-<target>/` via the symlink the
/// deploy script keeps current, falling back to scanning the version
/// directories alphabetically (matches SemVer for our usage).
fn latest_version_dir() -> Option<PathBuf> {
    let base = updates_dir();
    let target = target_id();
    let symlink = base.join(format!("latest-{}", target));
    if symlink.exists() {
        // The symlink points at a relative path like "0.1.36"; resolve.
        if let Ok(resolved) = std::fs::read_link(&symlink) {
            let full = if resolved.is_absolute() {
                resolved
            } else {
                base.join(resolved)
            };
            let candidate = full.join(target);
            if candidate.join(TARBALL_NAME).exists() {
                return Some(candidate);
            }
        }
    }
    // Fallback: highest-named version directory that has a bundle for us.
    let mut versions: Vec<(String, PathBuf)> = std::fs::read_dir(&base)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type()
                .map(|t| t.is_dir())
                .unwrap_or(false)
        })
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let p = e.path().join(target).join(TARBALL_NAME);
            if p.exists() {
                Some((name, e.path().join(target)))
            } else {
                None
            }
        })
        .collect();
    versions.sort_by(|a, b| natural_version_cmp(&a.0, &b.0));
    versions.pop().map(|(_, p)| p)
}

/// Comparator that orders `0.1.10` after `0.1.9` without us needing
/// `semver` as a dependency.
fn natural_version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    parts(a).cmp(&parts(b))
}

pub(crate) async fn manifest(State(s): State<AxumState>) -> Result<Response, (StatusCode, String)> {
    let dir = latest_version_dir().ok_or((
        StatusCode::NOT_FOUND,
        "No update bundle has been staged on this host yet. Run `scripts/deploy.sh` to publish one.".into(),
    ))?;
    let version = dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("0.0.0")
        .to_string();
    let sig = tokio::fs::read_to_string(dir.join(SIGNATURE_NAME))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("read .sig failed: {e}")))?;
    let pub_date = std::fs::metadata(dir.join(TARBALL_NAME))
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|t| {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    // Build an absolute URL the client can hand to Tauri's updater
    // verbatim — pulling the host_url out of stats matches the same form
    // we already stamp into invite JWT audiences.
    let host_http_base = http_base_for(&s).unwrap_or_else(|| "http://127.0.0.1".into());
    let url = format!("{host_http_base}/v1/update/bundle.tar.gz");
    let body = UpdateManifest {
        version,
        url,
        signature: sig,
        pub_date,
        notes: String::new(),
    };
    Ok(Json(body).into_response())
}

pub(crate) async fn bundle(State(s): State<AxumState>) -> Result<Response, (StatusCode, String)> {
    let _ = s;
    let dir = latest_version_dir().ok_or((StatusCode::NOT_FOUND, "no bundle staged".into()))?;
    serve_file(&dir.join(TARBALL_NAME), "application/gzip").await
}

pub(crate) async fn signature(State(s): State<AxumState>) -> Result<Response, (StatusCode, String)> {
    let _ = s;
    let dir = latest_version_dir().ok_or((StatusCode::NOT_FOUND, "no signature staged".into()))?;
    serve_file(&dir.join(SIGNATURE_NAME), "text/plain; charset=utf-8").await
}

async fn serve_file(path: &Path, content_type: &str) -> Result<Response, (StatusCode, String)> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("missing: {}: {e}", path.display())))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("read failed: {e}")))?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, content_type.to_string())],
        buf,
    )
        .into_response())
}

/// HTTP origin we'd return to a client — same derivation logic as the
/// invite audience, but with `http://` rather than `ws://`.
fn http_base_for(s: &AxumState) -> Option<String> {
    let cfg = s.app.config.read().clone();
    let host = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| cfg.host.bind_addr.clone());
    Some(format!("http://{host}:{}", cfg.host.port))
}
