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
    update
        .download_and_install(
            move |chunk_len, total_len| {
                let progress = total_len
                    .map(|t| (chunk_len as f64 / t as f64) * 100.0)
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
    // The plugin's `download_and_install` triggers a restart on its own
    // (macOS replaces the .app and relaunches). The line below is
    // belt-and-suspenders for platforms that don't auto-restart.
    app.restart();
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
