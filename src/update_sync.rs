//! Host-side GitHub-release auto-sync.
//!
//! Every hour, a host installation polls GitHub's Releases API for the
//! latest published `vX.Y.Z` tag of this repo, compares it to the
//! highest version currently staged under `~/.kinai/updates/`, and (if
//! GitHub's is newer) downloads each platform asset and stages it
//! locally so the family's connected clients can pick the update up via
//! `/v1/update/manifest`.
//!
//! Effectively makes the host a relay: GitHub → host → family devices.
//! The host owner no longer has to run `scripts/deploy.sh` manually to
//! push new versions to the rest of the family — as long as a public
//! GitHub Release exists, the host pulls it.
//!
//! Only runs in `Mode::Host`. Client installations skip this entirely;
//! they pull from their host, not GitHub.
//!
//! Asset → target mapping (matches `release.yml` + the host's manifest
//! endpoint expectations in `network/updates.rs::bundle_filenames`):
//!
//!   KinAI_*_aarch64.dmg                  — ignored (raw installer, not
//!                                          an auto-update bundle)
//!   KinAI_*_x64.dmg                      — ignored (same)
//!   KinAI_aarch64.app.tar.gz   + .sig    → darwin-aarch64/KinAI.app.tar.gz
//!   KinAI_x64.app.tar.gz       + .sig    → darwin-x86_64/KinAI.app.tar.gz
//!   *_x64-setup.nsis.zip       + .sig    → windows-x86_64/KinAI.nsis.zip
//!   *_x64_en-US.msi.zip        + .sig    → windows-x86_64/KinAI.msi.zip
//!   latest.json                          — ignored (we generate our own)

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tauri::{AppHandle, Manager, Runtime};

use crate::config::Mode;
use crate::SharedState;

const REPO: &str = "Gogo6969/kinai";
/// How often to check for new releases. Keep generous — GitHub's
/// unauthenticated API rate-limit is 60/hour, and this is a background
/// poll, not user-initiated.
const SYNC_INTERVAL_SECS: u64 = 60 * 60;

#[derive(Deserialize, Debug)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize, Debug)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub fn schedule_periodic_sync<R: Runtime>(app: AppHandle<R>) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        // Purely local cleanup — safe to run before the first network sync.
        prune_staged_updates();
        // Initial delay so app startup isn't blocked on a network call.
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            if is_host(&handle) {
                match sync_once().await {
                    Ok(Some(v)) => {
                        tracing::info!("update_sync: staged v{v} for clients");
                        prune_staged_updates();
                    }
                    Ok(None) => tracing::debug!("update_sync: already up to date"),
                    Err(e) => tracing::warn!("update_sync: {e:?}"),
                }
            }
            tokio::time::sleep(Duration::from_secs(SYNC_INTERVAL_SECS)).await;
        }
    });
}

/// Delete old staged versions under `~/.kinai/updates`, keeping:
///   * the newest KEEP_VERSIONS by semver (two — the extra one is deliberate
///     rollback headroom), and
///   * any version a `latest-<target>` symlink still points at (Intel-mac
///     staging is manual and can lag several versions behind — deleting a
///     dir a symlink references would 404 those clients' updates).
///
/// Without this the dir grew ~125MB per release, unbounded (measured 3.0GB
/// on the reference host). Only version-shaped directory names are ever
/// touched; anything else in the dir is left alone.
fn prune_staged_updates() {
    const KEEP_VERSIONS: usize = 2;
    let root = updates_dir();
    let Ok(entries) = std::fs::read_dir(&root) else { return };

    let mut versions: Vec<String> = Vec::new();
    let mut pinned: std::collections::HashSet<String> = std::collections::HashSet::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let path = e.path();
        // Symlink targets are bare version strings ("0.2.75") — collect
        // them as pinned. read_link sees the symlink itself even though
        // file_type() follows on some platforms, so check by name prefix.
        if name.starts_with("latest-") {
            if let Ok(target) = std::fs::read_link(&path) {
                pinned.insert(target.to_string_lossy().to_string());
            }
            continue;
        }
        if path.is_dir() && parse_version(&name).is_some() {
            versions.push(name);
        }
    }

    for name in versions_to_remove(versions, &pinned, KEEP_VERSIONS) {
        let dir = root.join(&name);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => tracing::info!("update_sync: pruned staged v{name}"),
            Err(e) => tracing::warn!("update_sync: prune v{name} failed: {e}"),
        }
    }
}

fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.split('.');
    let v = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    // Exactly three numeric components — don't match e.g. "0.2.75.bak".
    if it.next().is_some() {
        return None;
    }
    Some(v)
}

/// Pure core of the prune decision (unit-tested): given the staged version
/// names and the symlink-pinned set, return the names to delete.
fn versions_to_remove(
    mut versions: Vec<String>,
    pinned: &std::collections::HashSet<String>,
    keep: usize,
) -> Vec<String> {
    versions.sort_by_key(|v| std::cmp::Reverse(parse_version(v)));
    versions
        .into_iter()
        .skip(keep)
        .filter(|v| !pinned.contains(v))
        .collect()
}

#[cfg(test)]
mod prune_tests {
    use super::*;
    use std::collections::HashSet;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn keeps_newest_two_and_pinned() {
        let pinned: HashSet<String> = ["0.2.70".to_string()].into();
        let out = versions_to_remove(
            v(&["0.2.69", "0.2.75", "0.2.70", "0.2.74", "0.2.73"]),
            &pinned,
            2,
        );
        // Newest two (0.2.75, 0.2.74) survive the skip; 0.2.70 is pinned.
        assert_eq!(out, vec!["0.2.73".to_string(), "0.2.69".to_string()]);
    }

    #[test]
    fn numeric_not_lexicographic_ordering() {
        let out = versions_to_remove(v(&["0.2.9", "0.2.75", "0.2.10"]), &HashSet::new(), 2);
        // Lexicographic would wrongly rank "0.2.9" above "0.2.75".
        assert_eq!(out, vec!["0.2.9".to_string()]);
    }

    #[test]
    fn fewer_than_keep_is_noop() {
        assert!(versions_to_remove(v(&["0.2.75"]), &HashSet::new(), 2).is_empty());
    }

    #[test]
    fn version_parse_rejects_junk() {
        assert!(parse_version("0.2.75").is_some());
        assert!(parse_version("latest-darwin-aarch64").is_none());
        assert!(parse_version("0.2").is_none());
        assert!(parse_version("0.2.75.bak").is_none());
    }
}

fn is_host<R: Runtime>(app: &AppHandle<R>) -> bool {
    let state: Option<tauri::State<'_, SharedState>> = app.try_state();
    matches!(
        state.map(|s| s.config.read().mode),
        Some(Mode::Host)
    )
}

/// Check GitHub for the latest release. If newer than what's locally
/// staged, download every platform bundle we know how to serve. Returns
/// `Ok(Some(version))` if something was staged, `Ok(None)` if up to date.
async fn sync_once() -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        // GitHub requires a User-Agent for API calls.
        .user_agent(format!("kinai-update-sync/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()
        .context("http client")?;

    let release: Release = client
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .send()
        .await
        .context("fetch release")?
        .error_for_status()
        .context("release status")?
        .json()
        .await
        .context("parse release")?;

    let version = release.tag_name.trim_start_matches('v').to_string();
    if !version.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        return Err(anyhow!("unexpected tag_name '{}'", release.tag_name));
    }

    // If we already have every bundle for this version staged, nothing to do.
    let stage_root = updates_dir().join(&version);
    if all_known_targets_staged(&stage_root) {
        return Ok(None);
    }

    std::fs::create_dir_all(&stage_root).context("create stage dir")?;

    // Walk the release's assets, route each to its target dir.
    let mut wrote_anything = false;
    for asset in &release.assets {
        let Some((target, dest_name)) = classify_asset(&asset.name) else {
            continue;
        };
        let target_dir = stage_root.join(target);
        std::fs::create_dir_all(&target_dir).context("create target dir")?;
        let dest = target_dir.join(dest_name);
        if dest.exists() {
            continue; // already have this exact file
        }
        let bytes = client
            .get(&asset.browser_download_url)
            .send()
            .await
            .with_context(|| format!("download {}", asset.name))?
            .error_for_status()?
            .bytes()
            .await
            .context("read bytes")?;
        // Atomic-ish write: temp file, then rename.
        let tmp = target_dir.join(format!(".{dest_name}.part"));
        std::fs::write(&tmp, &bytes).context("write tmp")?;
        std::fs::rename(&tmp, &dest).context("rename into place")?;
        wrote_anything = true;
        tracing::info!(
            "update_sync: pulled {} → {target}/{dest_name} ({} bytes)",
            asset.name,
            bytes.len()
        );
    }

    // Refresh the `latest-<target>` symlinks for every target dir that
    // now has a complete bundle+sig pair under this version.
    for &(target, _bundle, _sig) in TARGET_FILENAMES {
        let dir = stage_root.join(target);
        if dir.exists() {
            let _ = update_latest_symlink(target, &version);
        }
    }

    Ok(if wrote_anything { Some(version) } else { None })
}

fn updates_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kinai")
        .join("updates")
}

/// Canonical filenames the host's manifest endpoint expects, paired
/// with how to detect the matching GitHub asset (substring match).
/// Order: (target, canonical bundle name, canonical sig name).
const TARGET_FILENAMES: &[(&str, &str, &str)] = &[
    ("darwin-aarch64", "KinAI.app.tar.gz", "KinAI.app.tar.gz.sig"),
    ("darwin-x86_64", "KinAI.app.tar.gz", "KinAI.app.tar.gz.sig"),
    ("windows-x86_64", "KinAI.nsis.zip", "KinAI.nsis.zip.sig"),
];

fn classify_asset(name: &str) -> Option<(&'static str, &'static str)> {
    // Order matters: more specific patterns first.
    let n = name;

    // macOS arm64 updater bundle
    if n.contains("aarch64.app.tar.gz.sig") {
        return Some(("darwin-aarch64", "KinAI.app.tar.gz.sig"));
    }
    if n.contains("aarch64.app.tar.gz") {
        return Some(("darwin-aarch64", "KinAI.app.tar.gz"));
    }
    // macOS Intel updater bundle
    if n.contains("x64.app.tar.gz.sig") {
        return Some(("darwin-x86_64", "KinAI.app.tar.gz.sig"));
    }
    if n.contains("x64.app.tar.gz") {
        return Some(("darwin-x86_64", "KinAI.app.tar.gz"));
    }
    // Windows: prefer .nsis.zip (matches Tauri's default Windows
    // auto-update format) but accept .msi.zip too.
    if n.contains(".nsis.zip.sig") {
        return Some(("windows-x86_64", "KinAI.nsis.zip.sig"));
    }
    if n.contains(".nsis.zip") {
        return Some(("windows-x86_64", "KinAI.nsis.zip"));
    }
    if n.contains(".msi.zip.sig") {
        return Some(("windows-x86_64", "KinAI.msi.zip.sig"));
    }
    if n.contains(".msi.zip") {
        return Some(("windows-x86_64", "KinAI.msi.zip"));
    }
    // .dmg, .msi, .exe, latest.json — not auto-update bundles, ignore.
    None
}

fn all_known_targets_staged(stage_root: &std::path::Path) -> bool {
    if !stage_root.exists() {
        return false;
    }
    for (target, bundle, sig) in TARGET_FILENAMES {
        let dir = stage_root.join(target);
        if !dir.join(bundle).exists() || !dir.join(sig).exists() {
            return false;
        }
    }
    true
}

fn update_latest_symlink(target: &str, version: &str) -> Result<()> {
    let symlink = updates_dir().join(format!("latest-{target}"));
    let _ = std::fs::remove_file(&symlink);
    #[cfg(unix)]
    std::os::unix::fs::symlink(version, &symlink).context("create symlink")?;
    #[cfg(windows)]
    {
        // Windows symlinks need elevated permissions or developer mode.
        // The manifest endpoint already falls back to scanning the
        // directory tree if the symlink is missing, so on Windows we
        // just write the version into a plain file as a breadcrumb.
        std::fs::write(symlink.with_extension("txt"), version).ok();
    }
    Ok(())
}
