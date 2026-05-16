//! Host-side update distribution.
//!
//! The deploy script stages signed update bundles under
//! `~/.kinai/updates/<version>/<target>/`, one directory per platform
//! the host has binaries for. The Mac mini host can serve Mac AND
//! Windows clients from a single manifest — Tauri's updater on each
//! client picks the entry matching its own platform.
//!
//! Endpoints:
//!
//!   GET /v1/update/manifest
//!       Multi-platform manifest in Tauri's standard format. Lists
//!       every target the host currently has a bundle for.
//!
//!   GET /v1/update/bundle?target=<id>
//!       Streams the update bundle for that platform. Target is one of
//!       darwin-aarch64, darwin-x86_64, windows-x86_64, linux-x86_64.
//!
//!   GET /v1/update/signature?target=<id>
//!       Streams the Minisign signature file.
//!
//!   GET /v1/update/bundle.tar.gz, /v1/update/bundle.tar.gz.sig
//!       Legacy single-platform endpoints kept for backward compatibility
//!       with clients on v0.2.6 and older. They serve the host's OWN
//!       platform bundle (same as before).
//!
//! All trust LAN locality + the JWT pairing as the transport-layer
//! security boundary; the Tauri updater plugin verifies the Minisign
//! signature against the pubkey baked into every client binary, so even
//! a compromised host can't push an unsigned update.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use super::server::AxumState;

/// Per-target bundle filename conventions. The deploy script names
/// staged files this way; the manifest endpoint surfaces them.
fn bundle_filename(target: &str) -> Option<&'static str> {
    match target {
        "darwin-aarch64" | "darwin-x86_64" => Some("KinAI.app.tar.gz"),
        "windows-x86_64" => Some("KinAI.msi.zip"),
        // Future: linux-x86_64 → "KinAI.AppImage.tar.gz"
        _ => None,
    }
}

/// All Tauri targets we know how to serve. The manifest only includes
/// entries for which a bundle actually exists on disk.
const SUPPORTED_TARGETS: &[&str] = &[
    "darwin-aarch64",
    "darwin-x86_64",
    "windows-x86_64",
    // "linux-x86_64",
];

#[derive(Serialize)]
pub struct UpdateManifest {
    pub version: String,
    pub notes: String,
    pub pub_date: String,
    pub platforms: BTreeMap<String, PlatformBundle>,
}

#[derive(Serialize)]
pub struct PlatformBundle {
    pub url: String,
    pub signature: String,
}

#[derive(Deserialize)]
pub struct TargetQuery {
    pub target: String,
}

fn updates_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kinai")
        .join("updates")
}

fn host_target_id() -> &'static str {
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

/// Find the highest-versioned directory that has a bundle for at least
/// one supported target. The deploy script stages by version, so this
/// is the "latest staged release."
fn latest_version_root() -> Option<(String, PathBuf)> {
    let base = updates_dir();
    let mut versions: Vec<(String, PathBuf)> = std::fs::read_dir(&base)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type()
                .map(|t| t.is_dir())
                .unwrap_or(false)
                && !e.file_name().to_string_lossy().starts_with("latest-")
                && !e.file_name().to_string_lossy().starts_with("shot-backup-")
        })
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            // Only consider dirs whose name parses as a SemVer-ish version.
            if !name.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                return None;
            }
            let dir = e.path();
            // Must contain at least one supported-target subdir with a bundle.
            let has_any = SUPPORTED_TARGETS.iter().any(|t| {
                if let Some(fname) = bundle_filename(t) {
                    dir.join(t).join(fname).exists()
                } else {
                    false
                }
            });
            if has_any {
                Some((name, dir))
            } else {
                None
            }
        })
        .collect();
    versions.sort_by(|a, b| natural_version_cmp(&a.0, &b.0));
    versions.pop()
}

fn natural_version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    parts(a).cmp(&parts(b))
}

pub(crate) async fn manifest(State(s): State<AxumState>) -> Result<Response, (StatusCode, String)> {
    let (version, root) = latest_version_root().ok_or((
        StatusCode::NOT_FOUND,
        "No update bundle has been staged on this host yet. Run `scripts/deploy.sh` to publish one.".into(),
    ))?;
    let host_http_base = http_base_for(&s).unwrap_or_else(|| "http://127.0.0.1".into());

    let mut platforms: BTreeMap<String, PlatformBundle> = BTreeMap::new();
    let mut latest_mtime: Option<std::time::SystemTime> = None;

    for &target in SUPPORTED_TARGETS {
        let Some(fname) = bundle_filename(target) else { continue };
        let bundle_path = root.join(target).join(fname);
        let sig_path = root.join(target).join(format!("{fname}.sig"));
        if !bundle_path.exists() || !sig_path.exists() {
            continue;
        }
        let Ok(sig) = tokio::fs::read_to_string(&sig_path).await else {
            continue;
        };
        if let Ok(meta) = std::fs::metadata(&bundle_path) {
            if let Ok(mt) = meta.modified() {
                latest_mtime = Some(match latest_mtime {
                    Some(prev) if prev > mt => prev,
                    _ => mt,
                });
            }
        }
        platforms.insert(
            target.to_string(),
            PlatformBundle {
                url: format!("{host_http_base}/v1/update/bundle?target={target}"),
                signature: sig,
            },
        );
    }

    if platforms.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            "No platform bundles staged for the latest version.".into(),
        ));
    }

    let pub_date = latest_mtime
        .map(|t| {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

    Ok(Json(UpdateManifest {
        version,
        notes: String::new(),
        pub_date,
        platforms,
    })
    .into_response())
}

pub(crate) async fn bundle(
    State(_s): State<AxumState>,
    Query(q): Query<TargetQuery>,
) -> Result<Response, (StatusCode, String)> {
    serve_target(&q.target, /* signature */ false).await
}

pub(crate) async fn signature_route(
    State(_s): State<AxumState>,
    Query(q): Query<TargetQuery>,
) -> Result<Response, (StatusCode, String)> {
    serve_target(&q.target, /* signature */ true).await
}

async fn serve_target(target: &str, want_signature: bool) -> Result<Response, (StatusCode, String)> {
    if !SUPPORTED_TARGETS.contains(&target) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Unsupported target '{target}'. Expected one of: {}", SUPPORTED_TARGETS.join(", ")),
        ));
    }
    let fname = bundle_filename(target).ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("No filename mapping for target '{target}'"),
        )
    })?;
    let (_, root) = latest_version_root().ok_or((
        StatusCode::NOT_FOUND,
        "No bundle staged".into(),
    ))?;
    let (path, content_type) = if want_signature {
        (
            root.join(target).join(format!("{fname}.sig")),
            "text/plain; charset=utf-8",
        )
    } else if fname.ends_with(".zip") {
        (root.join(target).join(fname), "application/zip")
    } else {
        (root.join(target).join(fname), "application/gzip")
    };
    serve_file(&path, content_type).await
}

/// Legacy single-platform tarball endpoint. Pre-v0.3 clients (Mac on
/// v0.2.5/v0.2.6) hit this URL directly because they were built before
/// the multi-platform manifest existed. We serve the host's OWN
/// platform bundle so those clients can still update.
pub(crate) async fn bundle_legacy(
    State(_s): State<AxumState>,
) -> Result<Response, (StatusCode, String)> {
    serve_target(host_target_id(), /* signature */ false).await
}

pub(crate) async fn signature_legacy(
    State(_s): State<AxumState>,
) -> Result<Response, (StatusCode, String)> {
    serve_target(host_target_id(), /* signature */ true).await
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

fn http_base_for(s: &AxumState) -> Option<String> {
    let cfg = s.app.config.read().clone();
    let host = local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| cfg.host.bind_addr.clone());
    Some(format!("http://{host}:{}", cfg.host.port))
}
