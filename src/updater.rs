//! Periodic update check — host first, GitHub fallback for off-LAN.
//!
//! When KinAI is in Client mode and connected, we point the Tauri
//! updater plugin at `<host_url>/v1/update/manifest`. The host hands
//! back a Tauri-format JSON describing the latest signed bundle it has
//! staged (see network/updates.rs). The plugin verifies the Minisign
//! signature against the pubkey baked into every install, then triggers
//! the same atomic-replace flow it'd use for a GitHub-served update.
//!
//! If the host has been unreachable for more than 24h we fall back to
//! the GitHub endpoint configured in tauri.conf.json — this covers a
//! laptop that's traveled out of range of the home network.
//!
//! Hosts (Mode::Host) and Unconfigured installs use GitHub as before.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_updater::UpdaterExt;

use crate::config::Mode;
use crate::SharedState;

const CHECK_INTERVAL_SECS: u64 = 4 * 60 * 60;
/// After this many seconds without a successful host check, fall back
/// to GitHub.
const HOST_FALLBACK_AFTER_SECS: i64 = 24 * 60 * 60;

/// Unix-time of the last successful host manifest fetch. `0` = never.
/// Reset to "now" on each successful check.
static LAST_HOST_OK: AtomicI64 = AtomicI64::new(0);

pub fn schedule_periodic_check<R: Runtime>(app: AppHandle<R>) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            check_once(&handle).await;
            tokio::time::sleep(Duration::from_secs(CHECK_INTERVAL_SECS)).await;
        }
    });
}

pub async fn check_once<R: Runtime>(app: &AppHandle<R>) {
    // Branch on mode: a client checks its host first, everyone else uses
    // GitHub directly.
    let mode = {
        let state: Option<tauri::State<'_, SharedState>> = app.try_state();
        match state {
            Some(s) => s.config.read().mode,
            None => Mode::Unconfigured,
        }
    };
    if matches!(mode, Mode::Client) {
        if try_host_check(app).await {
            return;
        }
        if !should_fall_back_to_github() {
            return;
        }
        tracing::info!("updater: host unreachable >24h, falling back to GitHub");
    }
    try_github_check(app).await;
}

async fn try_host_check<R: Runtime>(app: &AppHandle<R>) -> bool {
    let host_url = match host_http_base(app) {
        Some(u) => u,
        None => return false,
    };
    // `{{target}}` is substituted by the updater plugin with this
    // machine's target id, so the host can answer with the newest
    // version staged FOR THIS PLATFORM. Older hosts ignore the query
    // and answer exactly as before.
    let endpoint = format!("{host_url}/v1/update/manifest?target={{{{target}}}}");
    let parsed = match endpoint.parse() {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("updater: bad host endpoint {endpoint}: {e}");
            return false;
        }
    };
    let updater = match app.updater_builder().endpoints(vec![parsed]) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("updater builder: {e}");
            return false;
        }
    };
    let updater = match updater.build() {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("updater build: {e}");
            return false;
        }
    };
    match updater.check().await {
        Ok(Some(update)) => {
            LAST_HOST_OK.store(now_unix(), Ordering::Relaxed);
            let _ = app.emit(
                "kinai://update-available",
                serde_json::json!({
                    "version": update.version,
                    "current": update.current_version,
                    "body": update.body,
                    "source": "host",
                }),
            );
            true
        }
        Ok(None) => {
            LAST_HOST_OK.store(now_unix(), Ordering::Relaxed);
            true
        }
        Err(e) => {
            tracing::debug!("host updater check failed: {e:?}");
            false
        }
    }
}

async fn try_github_check<R: Runtime>(app: &AppHandle<R>) {
    let Ok(updater) = app.updater() else { return };
    match updater.check().await {
        Ok(Some(update)) => {
            let _ = app.emit(
                "kinai://update-available",
                serde_json::json!({
                    "version": update.version,
                    "current": update.current_version,
                    "body": update.body,
                    "source": "github",
                }),
            );
        }
        Ok(None) => {}
        Err(e) => {
            tracing::debug!("github updater check failed: {e:?}");
        }
    }
}

/// Pull the host HTTP base ("http://host:port") from the saved client
/// config. We deliberately don't query the live WS — the updater runs
/// regardless of connection state, and a stale-but-correct URL is
/// better than skipping a check during a brief reconnect.
fn host_http_base<R: Runtime>(app: &AppHandle<R>) -> Option<String> {
    let state = app.try_state::<SharedState>()?;
    let cfg = state.config.read();
    let ws_url = cfg.client.host_url.clone()?;
    ws_to_http_base(&ws_url)
}

fn ws_to_http_base(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    let (scheme, rest) = if let Some(r) = s.strip_prefix("ws://") {
        ("http", r)
    } else if let Some(r) = s.strip_prefix("wss://") {
        ("https", r)
    } else if let Some(r) = s.strip_prefix("http://") {
        ("http", r)
    } else if let Some(r) = s.strip_prefix("https://") {
        ("https", r)
    } else {
        ("http", s)
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        None
    } else {
        Some(format!("{scheme}://{authority}"))
    }
}

fn should_fall_back_to_github() -> bool {
    let last = LAST_HOST_OK.load(Ordering::Relaxed);
    if last == 0 {
        // Never had a successful host check — treat as "host has been
        // unreachable since boot". Falling back to GitHub on fresh
        // installs is safer than waiting 24h before any update check
        // can succeed.
        return true;
    }
    let age = now_unix() - last;
    age > HOST_FALLBACK_AFTER_SECS
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Download and install the latest available update, preferring the
/// host's endpoint and falling back to GitHub. Restarts the app on
/// success. Called from the frontend's update banner Install button.
pub async fn download_and_install<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let mode = {
        let state: Option<tauri::State<'_, SharedState>> = app.try_state();
        match state {
            Some(s) => s.config.read().mode,
            None => Mode::Unconfigured,
        }
    };
    // Try host first when we're a client; otherwise straight to GitHub.
    let update = if matches!(mode, Mode::Client) {
        match check_via_host(&app).await {
            Ok(Some(u)) => Some(u),
            _ => check_via_github(&app).await.map_err(|e| e.to_string())?,
        }
    } else {
        check_via_github(&app).await.map_err(|e| e.to_string())?
    };
    let Some(update) = update else {
        return Err("Already on the latest version.".into());
    };
    let app_for_event = app.clone();
    // The callback reports the size of THIS chunk, not the running total.
    // Dividing a chunk by the whole file gave a percentage that jittered
    // around zero for the entire download and never approached 100 — so
    // the bar looked frozen on every family member's update. Accumulate.
    let downloaded = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    update
        .download_and_install(
            move |chunk_len, total_len| {
                let so_far = downloaded
                    .fetch_add(chunk_len as u64, std::sync::atomic::Ordering::Relaxed)
                    + chunk_len as u64;
                let progress = total_len
                    .filter(|t| *t > 0)
                    .map(|t| ((so_far as f64 / t as f64) * 100.0).min(100.0))
                    .unwrap_or(0.0);
                let _ = app_for_event.emit(
                    "kinai://update-progress",
                    serde_json::json!({"progress": progress}),
                );
            },
            move || {
                tracing::info!("update downloaded; relaunching");
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    // Say, on disk, that the launch about to happen was asked for. See
    // `take_restart_marker` for why the new process cannot work that out
    // for itself.
    mark_restart();
    // The plugin's `download_and_install` triggers a restart on its own
    // (macOS replaces the .app and relaunches). The line below is
    // belt-and-suspenders for platforms that don't auto-restart.
    app.restart();
}

/// Where the "this relaunch was asked for" note lives.
fn restart_marker() -> std::path::PathBuf {
    crate::config::AppConfig::config_dir().join(".restarting")
}

/// Leave that note, just before handing over to the restart.
fn mark_restart() {
    if let Err(e) = std::fs::write(restart_marker(), chrono::Utc::now().to_rfc3339()) {
        // Not fatal: the worst case is the window staying hidden, which
        // is the behaviour this note exists to improve on.
        tracing::warn!("could not mark the restart: {e}");
    }
}

/// Was this launch the one the updater asked for?
///
/// Tauri restarts by re-running the binary with the ORIGINAL argv
/// (`process::restart` → `Command::new(bin).args(env.args_os.iter().skip(1))`),
/// so a copy that macOS started at login comes back still carrying
/// `--autostart`. Startup reads that flag as "this is a background
/// boot" and leaves the window hidden — so the member pressed a button
/// labelled *install and restart*, watched the app disappear, and had
/// nothing come back. It was running the whole time, in the tray.
/// Measured on 0.2.131: a launch carrying `--autostart` opens zero
/// windows, an ordinary one opens a window.
///
/// The note is consumed on read, and ignored when it is old — a restart
/// that never happened must not make every launch from then on pop a
/// window the member did not ask for.
pub fn take_restart_marker() -> bool {
    let path = restart_marker();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return false;
    };
    let _ = std::fs::remove_file(&path);
    chrono::DateTime::parse_from_rfc3339(raw.trim())
        .map(|t| {
            let age = chrono::Utc::now() - t.with_timezone(&chrono::Utc);
            age.num_seconds() >= 0 && age.num_seconds() < 300
        })
        .unwrap_or(false)
}

async fn check_via_host<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<Option<tauri_plugin_updater::Update>, anyhow::Error> {
    let host_url = host_http_base(app)
        .ok_or_else(|| anyhow::anyhow!("no host URL saved"))?;
    // `{{target}}` is substituted by the updater plugin with this
    // machine's target id, so the host can answer with the newest
    // version staged FOR THIS PLATFORM. Older hosts ignore the query
    // and answer exactly as before.
    let endpoint = format!("{host_url}/v1/update/manifest?target={{{{target}}}}");
    let parsed = endpoint.parse()?;
    let updater = app
        .updater_builder()
        .endpoints(vec![parsed])?
        .build()?;
    Ok(updater.check().await?)
}

async fn check_via_github<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<Option<tauri_plugin_updater::Update>, anyhow::Error> {
    let updater = app.updater()?;
    Ok(updater.check().await?)
}

#[cfg(test)]
mod restart_marker_tests {
    use super::{restart_marker, take_restart_marker};

    /// The note has to be consumed, or the first update would make every
    /// launch from then on open a window nobody asked for — including
    /// the login launches this whole flag exists to keep quiet.
    #[test]
    fn the_note_is_read_once_and_only_when_fresh() {
        crate::auth::sandbox_home();
        let path = restart_marker();
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        let _ = std::fs::remove_file(&path);

        // No note at all: an ordinary launch.
        assert!(!take_restart_marker(), "nothing to find");

        // A note written just now: this launch was asked for.
        std::fs::write(&path, chrono::Utc::now().to_rfc3339()).unwrap();
        assert!(take_restart_marker(), "the restart we just asked for");
        assert!(!path.exists(), "and it is gone");
        assert!(!take_restart_marker(), "so the next launch is ordinary again");

        // A note from a restart that never happened — a crash between
        // writing it and relaunching — must not follow the member around.
        std::fs::write(&path, (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339()).unwrap();
        assert!(!take_restart_marker(), "stale");
        assert!(!path.exists(), "cleared anyway");

        // Garbage is not a restart either.
        std::fs::write(&path, "yesterday-ish").unwrap();
        assert!(!take_restart_marker(), "unparseable");
    }
}
