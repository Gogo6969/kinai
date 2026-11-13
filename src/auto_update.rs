//! A family device installs a published update by itself.
//!
//! Until 0.2.151 an update reached a device in two steps: the device
//! noticed it (by itself, within four hours) and then a person had to
//! press "Install & restart" on a banner. A device whose window nobody
//! opened simply stayed on the old version, for hours or for days. A
//! security fix would have waited for that click in exactly the same
//! way.
//!
//! So a device that is connected to a family host now presses the button
//! itself. Four things all have to hold:
//!
//!   1. The HOST gives the go-ahead (`auto_install` in its manifest),
//!      which it only does for a version that is a published release —
//!      never for one that is merely built and staged, because that is
//!      the state a release is in while it is still being tested. An
//!      older host sends no such field, and that reads as "no".
//!   2. The DEVICE can replace itself without asking anyone for a
//!      password (`replaces_itself_unprompted`). An unattended admin
//!      prompt is worse than a banner.
//!   3. NOBODY IS USING IT (`activity.rs`): ten untouched minutes and no
//!      answer on its way.
//!   4. It has not already tried this version in the last day
//!      (`tried_recently`). An install that fails — or one that "works"
//!      and comes back as the same old version — must not turn into a
//!      device that restarts every ten minutes.
//!
//! When any of them does not hold, nothing is lost: the banner is still
//! there and still works.
//!
//! Hosts never install by themselves. A host restarting takes the whole
//! family's KinAI away for a moment; that stays a decision.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Manager, Runtime};

use crate::config::Mode;
use crate::updater::{self, Relaunch};
use crate::SharedState;

/// How often the waiter looks whether the device has gone quiet.
const POLL_SECS: u64 = 30;
/// One attempt per version per day.
const RETRY_AFTER_SECS: i64 = 24 * 60 * 60;

/// One waiter at a time, however often the update is noticed again (it is
/// re-noticed on every connect and whenever the window gets focus).
static WAITING: AtomicBool = AtomicBool::new(false);

/// Did the host release this update for unattended install?
pub(crate) fn host_says_go(manifest: &serde_json::Value) -> bool {
    manifest
        .get("auto_install")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Will THIS device install `version` without being asked?
pub fn will_self_install<R: Runtime>(
    app: &AppHandle<R>,
    manifest: &serde_json::Value,
    version: &str,
) -> bool {
    host_says_go(manifest) && device_may(app) && !tried_recently(version)
}

/// Is this a device that should, and can, update itself at all?
fn device_may<R: Runtime>(app: &AppHandle<R>) -> bool {
    let Some(state) = app.try_state::<SharedState>() else {
        return false;
    };
    let wants = {
        let cfg = state.config.read();
        matches!(cfg.mode, Mode::Client) && cfg.client.auto_update
    };
    wants && replaces_itself_unprompted(app)
}

/// Can the updater swap this install without a password prompt?
///
/// Each platform's updater has one way of quietly turning into a prompt:
/// macOS asks for an administrator when it may not rename the app in its
/// folder; a Linux `.deb`/`.rpm` install goes through `pkexec`; a Windows
/// install outside the user's own folders needs elevation. With a person
/// at the banner that is fine — they asked. Unattended, it is a password
/// dialog appearing on an empty desk, so those installs keep the banner.
fn replaces_itself_unprompted<R: Runtime>(app: &AppHandle<R>) -> bool {
    #[cfg(target_os = "linux")]
    {
        // Only an AppImage replaces itself; it does so in its own folder.
        return app
            .env()
            .appimage
            .as_ref()
            .map(PathBuf::from)
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .is_some_and(|dir| can_write_in(&dir));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        install_folder(&exe).is_some_and(|dir| can_write_in(&dir))
    }
}

/// The folder the updater has to be able to change: on macOS the one the
/// `.app` sits in (`…/KinAI.app/Contents/MacOS/KinAI` → `…`), on Windows
/// the one the `.exe` sits in.
#[cfg(not(target_os = "linux"))]
fn install_folder(exe: &Path) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        let bundle = exe.parent()?.parent()?.parent()?;
        if bundle.extension().and_then(|e| e.to_str()) != Some("app") {
            // Not running from an app bundle (a developer's `cargo run`):
            // there is nothing the updater could sensibly replace.
            return None;
        }
        bundle.parent().map(Path::to_path_buf)
    } else {
        exe.parent().map(Path::to_path_buf)
    }
}

#[cfg(unix)]
fn can_write_in(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // Asks the system; creates nothing.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

#[cfg(not(unix))]
fn can_write_in(dir: &Path) -> bool {
    // Windows has no one-call answer that honours ACLs, so try it: a
    // throwaway file, removed at once.
    let probe = dir.join(format!(".kinai-write-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The host offers a released update and this device may take it: start
/// waiting for a quiet moment, unless something is waiting already.
pub fn offer<R: Runtime>(app: &AppHandle<R>) {
    if WAITING.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        wait_then_install(app).await;
        WAITING.store(false, Ordering::SeqCst);
    });
}

async fn wait_then_install<R: Runtime>(app: AppHandle<R>) {
    tracing::info!(
        "auto-update: an update is waiting — it installs by itself once this device has not been used for {} minutes",
        crate::activity::QUIET_SECS / 60
    );
    loop {
        tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
        if !device_may(&app) {
            tracing::info!("auto-update: switched off on this device — the banner stays");
            return;
        }
        if crate::activity::busy_reason().is_some() {
            continue;
        }

        // Quiet. Ask the host again rather than trusting what it said
        // when the waiting began: that can be hours ago, and in the
        // meantime the release may have been replaced or pulled.
        let update = match updater::check_via_host(&app).await {
            Ok(Some(u)) => u,
            Ok(None) => {
                tracing::info!("auto-update: nothing left to install");
                return;
            }
            Err(e) => {
                // Host away. The next ordinary check — on reconnect, or in
                // four hours — starts a new wait; no need to knock here.
                tracing::info!("auto-update: could not reach the host just now ({e}) — trying again later");
                return;
            }
        };
        let version = update.version.clone();
        if !host_says_go(&update.raw_json) {
            tracing::info!("auto-update: the host no longer releases {version} for unattended install — the banner stays");
            return;
        }
        if tried_recently(&version) {
            tracing::info!("auto-update: already tried {version} within the last day — the banner stays");
            return;
        }
        // The question took a moment; somebody may have sat down.
        if crate::activity::busy_reason().is_some() {
            continue;
        }

        note_attempt(&version);
        let relaunch = if main_window_visible(&app) {
            Relaunch::Show
        } else {
            Relaunch::Quiet
        };
        tracing::info!(
            "auto-update: installing {version} — this device has not been used for {} minutes (comes back {})",
            crate::activity::quiet_minutes(),
            match relaunch {
                Relaunch::Show => "with its window",
                Relaunch::Quiet => "in the tray",
            }
        );
        // On success this never returns: the app restarts.
        if let Err(e) = updater::install(app.clone(), update, relaunch).await {
            tracing::warn!(
                "auto-update: installing {version} failed: {e} — the banner stays, and there is no new attempt for a day"
            );
        }
        return;
    }
}

fn main_window_visible<R: Runtime>(app: &AppHandle<R>) -> bool {
    app.get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false)
}

/// Where the last attempt is written down: the version, then when.
fn attempt_file() -> PathBuf {
    crate::config::AppConfig::config_dir().join(".auto-update-attempt")
}

fn note_attempt(version: &str) {
    note_attempt_in(&attempt_file(), version, chrono::Utc::now());
}

fn note_attempt_in(path: &Path, version: &str, now: chrono::DateTime<chrono::Utc>) {
    if let Err(e) = std::fs::write(path, format!("{version}\n{}\n", now.to_rfc3339())) {
        // Without the note there is no brake. Say so — but go on: a
        // device that cannot write into its own folder has bigger
        // trouble than one extra attempt.
        tracing::warn!("auto-update: could not note the attempt: {e}");
    }
}

fn tried_recently(version: &str) -> bool {
    tried_recently_in(&attempt_file(), version, chrono::Utc::now())
}

fn tried_recently_in(path: &Path, version: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let mut lines = raw.lines();
    let (Some(tried), Some(when)) = (lines.next(), lines.next()) else {
        return false;
    };
    if tried.trim() != version {
        return false;
    }
    chrono::DateTime::parse_from_rfc3339(when.trim())
        .map(|t| {
            let age = (now - t.with_timezone(&chrono::Utc)).num_seconds();
            (0..RETRY_AFTER_SECS).contains(&age)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kinai-auto-update-{name}-{}", std::process::id()))
    }

    /// An older host knows nothing of this field. Silence must read as
    /// "a person decides", or every device would start installing builds
    /// the moment its host staged them.
    #[test]
    fn only_an_explicit_yes_from_the_host_counts() {
        let yes = serde_json::json!({"version": "0.9.4", "auto_install": true});
        let no = serde_json::json!({"version": "0.9.4", "auto_install": false});
        let silent = serde_json::json!({"version": "0.9.4"});
        let odd = serde_json::json!({"version": "0.9.4", "auto_install": "true"});
        assert!(host_says_go(&yes));
        assert!(!host_says_go(&no));
        assert!(!host_says_go(&silent), "an older host");
        assert!(!host_says_go(&odd), "a string is not a yes");
    }

    /// The brake. An installer that fails, or one that reports success and
    /// leaves the old version in place, would otherwise be retried at
    /// every quiet moment — a device restarting itself all night.
    #[test]
    fn a_version_is_tried_once_a_day_not_every_ten_minutes() {
        let path = scratch("brake");
        let _ = std::fs::remove_file(&path);
        let now = chrono::Utc::now();

        assert!(!tried_recently_in(&path, "0.9.4", now), "never tried");

        note_attempt_in(&path, "0.9.4", now);
        assert!(tried_recently_in(&path, "0.9.4", now), "just tried");
        assert!(
            tried_recently_in(&path, "0.9.4", now + chrono::Duration::hours(23)),
            "still the same day"
        );
        assert!(
            !tried_recently_in(&path, "0.9.4", now + chrono::Duration::hours(25)),
            "a day later it may try again"
        );
        // The brake is per version: the fix for a broken release must not
        // be held back by the broken release's failed attempt.
        assert!(!tried_recently_in(&path, "0.9.5", now), "a newer version is a new try");

        let _ = std::fs::remove_file(&path);
    }

    /// A note nobody can read is no brake — but it is also no reason to
    /// refuse for ever.
    #[test]
    fn a_damaged_note_does_not_block_updates() {
        let path = scratch("damaged");
        let now = chrono::Utc::now();
        std::fs::write(&path, "0.9.4\nlast tuesday\n").unwrap();
        assert!(!tried_recently_in(&path, "0.9.4", now));
        std::fs::write(&path, "0.9.4").unwrap();
        assert!(!tried_recently_in(&path, "0.9.4", now));
        // A clock that jumped backwards: the note is "from the future".
        note_attempt_in(&path, "0.9.4", now + chrono::Duration::hours(3));
        assert!(!tried_recently_in(&path, "0.9.4", now));
        let _ = std::fs::remove_file(&path);
    }

    /// The updater replaces the whole `.app`, so what has to be writable
    /// is the folder the app sits in — not the app, and not the binary.
    #[cfg(target_os = "macos")]
    #[test]
    fn on_a_mac_the_folder_that_matters_is_the_one_holding_the_app() {
        assert_eq!(
            install_folder(Path::new("/Applications/KinAI.app/Contents/MacOS/KinAI")),
            Some(PathBuf::from("/Applications"))
        );
        // A bare binary is a developer's build; nothing to replace.
        assert_eq!(install_folder(Path::new("/tmp/target/debug/kinai")), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_we_may_not_write_in_is_recognised_without_touching_it() {
        let dir = std::env::temp_dir();
        assert!(can_write_in(&dir));
        // Root's own folders exist on every Mac and Linux box, and are
        // not ours to change.
        assert!(!can_write_in(Path::new("/usr")) || unsafe { libc::geteuid() } == 0);
        assert!(!can_write_in(Path::new("/no/such/folder/anywhere")));
    }
}
