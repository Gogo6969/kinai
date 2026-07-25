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

/// Candidate bundle filenames per target. The manifest endpoint serves
/// whichever one is actually on disk.
///
/// Windows: we stage the raw `.msi` (and fall back to `.exe` NSIS),
/// NOT the `.msi.zip` wrapper. The Tauri 2.x updater's zip-extraction
/// path declares `zip = { default-features = false }` with no
/// compression backends, so it can't decompress DEFLATE-compressed
/// zips — every `.msi.zip` we used to ship tripped "Unsupported Zip
/// Archive: Compression method not supported". Serving the raw .msi
/// avoids the zip path entirely; `extract_exe` → `infer::is_msi`
/// detects it as MSI and installs directly.
///
/// `.msi.zip` is kept as a fallback for legacy hosts that still stage
/// the wrapper format. It won't work on Tauri 2.10.x clients but
/// won't break anything either; deploy.sh now writes the raw .msi
/// so new hosts auto-fix on first redeploy.
///
/// PRIORITY: the NSIS `KinAI.exe` is preferred over the `.msi` because
/// an MSI consults its upgrade table and no-ops when the ProductVersion
/// already matches what's installed (the "stuck on the update banner"
/// bug); the NSIS installer always installs over the existing version.
/// So if both are ever staged in one dir, serve the NSIS one.
fn bundle_filenames(target: &str) -> &'static [&'static str] {
    match target {
        "darwin-aarch64" | "darwin-x86_64" => &["KinAI.app.tar.gz"],
        "windows-x86_64" => &["KinAI.exe", "KinAI.nsis.zip", "KinAI.msi", "KinAI.msi.zip"],
        // Tauri's Linux updater downloads the raw AppImage (signed via
        // its sidecar .sig) and swaps the running one — there's no
        // .tar.gz wrapper like macOS uses for the .app.
        "linux-x86_64" => &["KinAI.AppImage"],
        _ => &[],
    }
}

/// Resolve the actual on-disk bundle file (and its `.sig` sibling) for
/// the given target inside a per-version dir. Returns None if no
/// candidate exists.
fn resolve_bundle(root: &std::path::Path, target: &str) -> Option<(std::path::PathBuf, std::path::PathBuf, &'static str)> {
    for &name in bundle_filenames(target) {
        let bundle = root.join(target).join(name);
        let sig = root.join(target).join(format!("{name}.sig"));
        if bundle.exists() && sig.exists() {
            return Some((bundle, sig, name));
        }
    }
    None
}

/// All Tauri targets we know how to serve. The manifest only includes
/// entries for which a bundle actually exists on disk.
const SUPPORTED_TARGETS: &[&str] = &[
    "darwin-aarch64",
    "darwin-x86_64",
    "windows-x86_64",
    "linux-x86_64",
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
    latest_version_root_in(&updates_dir())
}

/// Newest staged version that has a bundle for THIS target.
///
/// The manifest carries a single version, so without knowing the asking
/// client's platform the host has to pick one — and it used to pick the
/// newest version staged for ANY platform. Between `deploy.sh` (which
/// stages only the host's own arch) and `stage-windows`/`stage-linux`
/// (which run after the release publishes), that version has no entry
/// for the other platforms. A client whose target is missing from
/// `platforms` doesn't just skip the update: tauri-plugin-updater
/// resolves the download URL BEFORE comparing versions, so `check()`
/// fails outright and the client can't update to ANY version until
/// staging finishes.
///
/// Selecting per target removes that window entirely, and can only ever
/// move a target forward — never withhold an update it could install.
fn newest_version_for_target(base: &Path, target: &str) -> Option<(String, PathBuf)> {
    let mut hits: Vec<(String, PathBuf)> = staged_version_dirs(base)
        .into_iter()
        .filter(|(_, dir)| resolve_bundle(dir, target).is_some())
        .collect();
    hits.sort_by(|a, b| natural_version_cmp(&a.0, &b.0));
    hits.pop()
}

/// Every `<base>/<version>/` directory that holds at least one bundle.
fn staged_version_dirs(base: &Path) -> Vec<(String, PathBuf)> {
    std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && !e.file_name().to_string_lossy().starts_with("latest-")
                && !e.file_name().to_string_lossy().starts_with("shot-backup-")
        })
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                return None;
            }
            let dir = e.path();
            SUPPORTED_TARGETS
                .iter()
                .any(|t| resolve_bundle(&dir, t).is_some())
                .then_some((name, dir))
        })
        .collect()
}

fn latest_version_root_in(base: &Path) -> Option<(String, PathBuf)> {
    let mut versions = staged_version_dirs(base);
    versions.sort_by(|a, b| natural_version_cmp(&a.0, &b.0));
    versions.pop()
}

#[allow(dead_code)]
fn legacy_latest_version_root() -> Option<(String, PathBuf)> {
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
            if !name.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                return None;
            }
            let dir = e.path();
            let has_any = SUPPORTED_TARGETS
                .iter()
                .any(|t| resolve_bundle(&dir, t).is_some());
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

#[derive(Deserialize)]
pub struct ManifestQuery {
    /// Sent by KinAI clients from 0.2.87 on (`?target={{target}}`).
    /// Absent for older clients — they get the previous whole-host
    /// behavior, so this can never make an existing client worse.
    pub target: Option<String>,
}

pub(crate) async fn manifest(
    State(s): State<AxumState>,
    Query(q): Query<ManifestQuery>,
) -> Result<Response, (StatusCode, String)> {
    let asked = q
        .target
        .as_deref()
        .filter(|t| SUPPORTED_TARGETS.contains(t));
    let (version, root) = match asked {
        Some(t) => newest_version_for_target(&updates_dir(), t).ok_or((
            StatusCode::NOT_FOUND,
            format!("No update bundle staged for {t} on this host yet."),
        ))?,
        None => latest_version_root().ok_or((
            StatusCode::NOT_FOUND,
            "No update bundle has been staged on this host yet. Run `scripts/deploy.sh` to publish one.".into(),
        ))?,
    };
    let host_http_base = http_base_for(&s).unwrap_or_else(|| "http://127.0.0.1".into());

    let mut platforms: BTreeMap<String, PlatformBundle> = BTreeMap::new();
    let mut latest_mtime: Option<std::time::SystemTime> = None;

    for &target in SUPPORTED_TARGETS {
        let Some((bundle_path, sig_path, _)) = resolve_bundle(&root, target) else {
            continue;
        };
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
    let (_, root) = latest_version_root().ok_or((
        StatusCode::NOT_FOUND,
        "No bundle staged".into(),
    ))?;
    let (bundle_path, sig_path, fname) = resolve_bundle(&root, target).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("No bundle staged for target '{target}'"),
        )
    })?;
    let (path, content_type) = if want_signature {
        (sig_path, "text/plain; charset=utf-8")
    } else if fname.ends_with(".zip") {
        (bundle_path, "application/zip")
    } else if fname.ends_with(".msi") {
        (bundle_path, "application/x-msi")
    } else if fname.ends_with(".exe") {
        (bundle_path, "application/vnd.microsoft.portable-executable")
    } else {
        (bundle_path, "application/gzip")
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

#[cfg(test)]
mod target_selection_tests {
    use super::*;

    fn stage(base: &Path, version: &str, target: &str) {
        let dir = base.join(version).join(target);
        std::fs::create_dir_all(&dir).unwrap();
        let name = bundle_filenames(target)[0];
        std::fs::write(dir.join(name), b"bundle").unwrap();
        std::fs::write(dir.join(format!("{name}.sig")), b"sig").unwrap();
    }

    /// The exact shape that broke Windows/Linux clients: a newer version
    /// staged for the host's own arch only, while the previous version
    /// has everything. Each target must be offered the newest bundle IT
    /// can actually install — never a version with no entry for it.
    #[test]
    fn each_target_gets_its_own_newest_version() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        for t in SUPPORTED_TARGETS {
            stage(base, "0.2.85", t);
        }
        stage(base, "0.2.86", "darwin-aarch64");

        assert_eq!(
            newest_version_for_target(base, "darwin-aarch64").unwrap().0,
            "0.2.86",
            "the platform that IS staged gets the newest"
        );
        for t in ["windows-x86_64", "linux-x86_64", "darwin-x86_64"] {
            assert_eq!(
                newest_version_for_target(base, t).unwrap().0,
                "0.2.85",
                "{t} must still be offered the version it can install"
            );
        }
        // Whole-host view (what pre-0.2.87 clients get) is unchanged.
        assert_eq!(latest_version_root_in(base).unwrap().0, "0.2.86");
    }

    #[test]
    fn version_order_is_numeric_and_gaps_are_fine() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        // 0.2.9 must not beat 0.2.10 (string order would).
        stage(base, "0.2.9", "linux-x86_64");
        stage(base, "0.2.10", "linux-x86_64");
        assert_eq!(newest_version_for_target(base, "linux-x86_64").unwrap().0, "0.2.10");
        // A target never staged has no answer — the client then falls
        // back to GitHub rather than being handed something broken.
        assert!(newest_version_for_target(base, "windows-x86_64").is_none());
        // Directories with no bundle at all are ignored entirely.
        std::fs::create_dir_all(base.join("0.2.99")).unwrap();
        assert_eq!(newest_version_for_target(base, "linux-x86_64").unwrap().0, "0.2.10");
    }
}
