//! KinAI library entrypoint — wires Tauri shell, Axum HTTP/WS server, tray,
//! global hotkey, LLM client, and the never-lose-context pipeline.

pub mod attachments;
pub mod auth;
pub mod changelog;
pub mod commands;
pub mod comfyui;
pub mod config;
pub mod context;
pub mod db;
pub mod discovery;
pub mod factcheck;
pub mod hotkey;
pub mod llm;
pub mod network;
pub mod slash;
pub mod telegram;
pub mod tools;
pub mod tray;
pub mod stt;
pub mod tts;
pub mod update_sync;
pub mod updater;
pub mod vision;

use std::sync::Arc;

use std::collections::HashMap;

use parking_lot::RwLock;
use tauri::{AppHandle, Manager, RunEvent};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::{AppConfig, Mode};
use crate::db::Db;

pub struct AppState {
    pub handle: RwLock<Option<AppHandle>>,
    pub config: RwLock<AppConfig>,
    pub db: Db,
    pub llm: Mutex<llm::LlmClient>,
    pub net: Arc<Mutex<network::NetState>>,
    pub stats: RwLock<RuntimeStats>,
    /// Telegram supervisor handle — lets the runtime stop and restart
    /// the long-poll loop when the bot token changes in Settings.
    pub telegram: Arc<telegram::TelegramSupervisor>,
    /// In-flight LLM turns keyed by the client_msg_id the frontend used
    /// when calling send_message. The CancellationToken propagates into
    /// stream::open and loop_pipeline so the Stop button can actually
    /// abort a hung turn instead of leaving the user with a perpetual
    /// "typing…" indicator. Cleaned up in send_message's finish path.
    pub pending_turns: Arc<parking_lot::Mutex<HashMap<String, CancellationToken>>>,
    /// Peers with a fact check currently in flight — per-peer
    /// single-flight guard for the PAID online checker slot.
    pub fact_checks_running: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// Cached per-slot liveness (label -> (probed_at, alive)) with a
    /// short TTL — feeds the slash menu's offline markers and the
    /// switch-confirmation heads-up without hammering the model servers.
    pub slot_health: parking_lot::Mutex<HashMap<String, (std::time::Instant, bool)>>,
    /// Currently-playing desktop TTS process (the speak button / voice
    /// previews). One at a time: starting a new playback kills the old.
    pub tts_child: parking_lot::Mutex<Option<tokio::process::Child>>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RuntimeStats {
    pub model_loaded: Option<String>,
    pub peers_connected: usize,
    pub host_url: Option<String>,
    pub last_first_token_ms: Option<u64>,
    /// Set by the client networking layer. `true` once the WS handshake +
    /// Hello succeed, `false` while disconnected. Surfaced through
    /// `runtime_stats` so the frontend can hydrate the sidebar's
    /// connection indicator on app launch — the live
    /// `kinai://client-status` event stream alone isn't enough, because
    /// the first event can fire before the UI subscribes, leaving the
    /// indicator stuck on "Connecting…".
    pub client_connected: bool,
    pub client_error: Option<String>,
    /// Last `Envelope::Welcome` payload received from the host — model
    /// name, family name, search engine, host version. Stored here so the
    /// client's Settings page can hydrate its read-only "Host" card on
    /// every visit instead of needing to be present at WS connect time
    /// (the welcome event fires once and is gone).
    pub host_info: Option<HostInfo>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct HostInfo {
    pub family_name: String,
    pub host_version: String,
    pub host_model: String,
    pub host_search_engine: String,
    pub host_vision: String,
    /// `@username` of the host's Telegram bot (or empty if not set up).
    /// Drives the client-side "Family bot: @foo" label + gates the
    /// "Connect Telegram" button on client peers.
    pub host_telegram_bot: String,
    /// Active model slots the host serves (fast/balanced/deep). The
    /// client's slash menu is built from this — its local LLM config is
    /// empty, so without it `/fast`-style switches never appeared.
    pub host_slots: Vec<crate::network::protocol::HostSlotWire>,
    /// Host has the ONLINE fact-check model configured — gates the
    /// per-message "fact check" button on client peers.
    pub host_fact_check: bool,
    /// Host speaks the report protocol (0.2.85+) — gates the report
    /// button so it never hangs against an older host.
    pub host_reports: bool,
    /// Host applies client-initiated thread delete/rename (0.2.86+).
    pub host_thread_ops: bool,
}

pub type SharedState = Arc<AppState>;

/// Keeps the non-blocking file-log writer alive for the whole process —
/// dropping it stops file logging. Set once during startup.
static LOG_GUARD: std::sync::OnceLock<tracing_appender::non_blocking::WorkerGuard> =
    std::sync::OnceLock::new();

/// Cap ~/.kinai/logs at 7 days AND ~50MB total. Oldest files go first (by
/// name — the daily roller suffixes `kinai.log.YYYY-MM-DD`, so lexicographic
/// = chronological). Runs before the tracing subscriber exists, so failures
/// are silently ignored rather than logged.
fn prune_old_logs(dir: &std::path::Path) {
    const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);
    const MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::path::PathBuf, u64, std::time::SystemTime)> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_str()?.to_string();
            if !name.starts_with("kinai.log") {
                return None; // never touch files we didn't create
            }
            let meta = e.metadata().ok()?;
            Some((p, meta.len(), meta.modified().ok()?))
        })
        .collect();
    let now = std::time::SystemTime::now();
    // Age pass.
    files.retain(|(p, _, modified)| {
        let too_old = now.duration_since(*modified).map(|a| a > MAX_AGE).unwrap_or(false);
        if too_old {
            let _ = std::fs::remove_file(p);
        }
        !too_old
    });
    // Size pass: newest first, delete once the running total exceeds the cap.
    files.sort_by(|a, b| b.2.cmp(&a.2));
    let mut total: u64 = 0;
    for (p, len, _) in files {
        total = total.saturating_add(len);
        if total > MAX_TOTAL_BYTES {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// Clamp permissions on everything secret under ~/.kinai: directories to
/// 0700, files to 0600. Covers config.toml (API keys, bot token), the
/// SQLite DB (+wal/shm — the family's entire chat history), and keys/
/// (JWT signing keypair, updater signing key, Apple notary env). Unix-only;
/// on Windows the per-user home ACL already scopes access.
fn harden_kinai_permissions() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fn clamp(path: &std::path::Path, mode: u32) {
            if let Ok(meta) = std::fs::metadata(path) {
                let mut perms = meta.permissions();
                if perms.mode() & 0o777 != mode {
                    perms.set_mode(mode);
                    if let Err(e) = std::fs::set_permissions(path, perms) {
                        tracing::warn!("perm clamp {} failed: {e}", path.display());
                    }
                }
            }
        }
        let root = AppConfig::config_dir();
        clamp(&root, 0o700);
        for sub in ["keys", "logs"] {
            clamp(&root.join(sub), 0o700);
        }
        // Files directly under ~/.kinai (config.toml + backups, kinai.db +
        // -wal/-shm) and everything inside keys/.
        for dir in [root.clone(), root.join("keys")] {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    clamp(&e.path(), 0o600);
                }
            }
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Linux/WebKitGTK: the DMABUF renderer leaves a blank white window on
    // many setups (Nvidia, some Wayland compositors, newer webkit). The
    // standard Tauri workaround is to disable it before the webview
    // initializes. Only set it if the user hasn't chosen otherwise, so a
    // working machine can opt back into hardware accel.
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }

        // WebKitGTK's Skia renderer aborts the web-content process
        // (colrv1_configure_skpaint vector-OOB assertion) the moment it
        // rasterizes a COLRv1 color-emoji glyph — so any color emoji in
        // a reply crashes the page to a blank window. The CSS
        // `font-variant-emoji: text` hint isn't enough: it falls back to
        // the color glyph when no monochrome one exists.
        //
        // Reject ALL color fonts at the font-selection layer via a
        // per-process fontconfig (the FC_COLOR property catches every
        // COLR/CBDT/sbix face by capability, not by name), so Skia never
        // receives a color face and the crashing path is unreachable.
        // Emoji fall back to monochrome (or tofu) — degraded but stable.
        //
        // Guarded: only override when the user hasn't set their own
        // FONTCONFIG_FILE AND the standard system config exists, so a
        // missing include can't leave the process with no fonts at all.
        let system_fc = std::path::Path::new("/etc/fonts/fonts.conf");
        if std::env::var_os("FONTCONFIG_FILE").is_none() && system_fc.exists() {
            let dir = dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".kinai");
            let _ = std::fs::create_dir_all(&dir);
            let conf = dir.join("fonts-no-color.conf");
            let xml = r#"<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig>
  <include ignore_missing="yes">/etc/fonts/fonts.conf</include>
  <selectfont>
    <rejectfont>
      <pattern><patelt name="color"><bool>true</bool></patelt></pattern>
    </rejectfont>
  </selectfont>
</fontconfig>
"#;
            if std::fs::write(&conf, xml).is_ok() {
                std::env::set_var("FONTCONFIG_FILE", &conf);
            }
        }
    }

    // Log to stdout AND a daily-rolling file under ~/.kinai/logs. Launched
    // from Finder/`open`, stdout goes nowhere — every past field problem
    // required relaunching the binary from a terminal just to see the error.
    // The file layer makes failures diagnosable after the fact
    // (Settings → About → "Open logs").
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "kinai=info,axum=info,tower_http=info".into());
        let logs_dir = AppConfig::config_dir().join("logs");
        let _ = std::fs::create_dir_all(&logs_dir);
        prune_old_logs(&logs_dir);
        let (file_writer, guard) = tracing_appender::non_blocking(
            tracing_appender::rolling::daily(&logs_dir, "kinai.log"),
        );
        // The guard flushes the background writer on drop — park it for the
        // process lifetime or file logging silently stops.
        let _ = LOG_GUARD.set(guard);
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(file_writer),
            )
            .init();
    }

    // Secrets hygiene: everything under ~/.kinai is per-user private state —
    // config.toml carries API keys and the Telegram bot token, kinai.db the
    // family's chats, keys/ the JWT signing keypair AND the updater signing
    // key (a readable updater.key would let any local process mint update
    // signatures this app trusts). Default file creation left several of
    // these world-readable; clamp them every launch.
    harden_kinai_permissions();

    // Raise the open-file soft limit before anything opens sockets. macOS
    // launches GUI apps with a 256-fd cap, which the LAN scanner (and a
    // busy host serving many family clients) can blow through.
    llm::detect::raise_fd_limit_best_effort();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let cfg = AppConfig::load_or_default();
    tracing::info!("KinAI starting in {:?} mode", cfg.mode);

    let db = rt
        .block_on(Db::open(&cfg.db_path()))
        .expect("failed to open KinAI database");
    // Second clamp pass, AFTER the DB exists: on a FRESH install the first
    // pass ran before SQLite created kinai.db(+wal/shm), so those files
    // would otherwise sit world-readable (umask 022 → 0644) until the next
    // launch. Idempotent and cheap.
    harden_kinai_permissions();

    let llm = llm::LlmClient::new(cfg.llm.clone());
    let net = Arc::new(Mutex::new(network::NetState::default()));

    let state: SharedState = Arc::new(AppState {
        handle: RwLock::new(None),
        config: RwLock::new(cfg.clone()),
        db,
        llm: Mutex::new(llm),
        net: net.clone(),
        stats: RwLock::new(RuntimeStats::default()),
        telegram: Arc::new(telegram::TelegramSupervisor::default()),
        pending_turns: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        slot_health: parking_lot::Mutex::new(HashMap::new()),
        fact_checks_running: parking_lot::Mutex::new(std::collections::HashSet::new()),
        tts_child: parking_lot::Mutex::new(None),
    });

    tauri::Builder::default()
        // Must be the FIRST plugin so a second launch is intercepted before
        // any other initialization (which would otherwise fail with
        // "address already in use" on port 4847).
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tracing::info!("second-instance launch — focusing existing window");
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        // Launch-at-login. The LaunchAgent strategy on macOS uses a
        // per-user ~/Library/LaunchAgents/<bundle-id>.plist — no admin,
        // no signed-helper bundle required. On Windows the plugin writes
        // HKCU\Software\Microsoft\Windows\CurrentVersion\Run\KinAI. We
        // pass `--autostart` so future builds can detect "we were
        // launched at boot" vs. "user double-clicked the app" and
        // potentially skip the dock-bounce / first-run dialog. Not used
        // yet, harmless to pass today.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_mode,
            commands::set_llm_settings,
            commands::set_llm_deep_settings,
            commands::set_llm_balanced_settings,
            commands::set_overlay_settings,
            commands::set_theme,
            commands::set_startup_settings,
            commands::set_vision_settings,
            commands::test_vision_endpoint,
            commands::set_comfy_config,
            commands::test_comfy_endpoint,
            commands::download_url_to_path,
            commands::write_prompt_snapshot,
            commands::set_telegram_token,
            commands::test_telegram_token,
            commands::request_telegram_pair,
            commands::telegram_link_status,
            commands::unpair_telegram,
            commands::detect_backends,
            commands::scan_local_network,
            commands::rescan_kinai_hosts,
            commands::local_ip,
            commands::query_model_caps,
            commands::list_local_models,
            commands::test_llm_connection,
            commands::list_threads,
            commands::search_messages,
            commands::load_thread,
            commands::create_thread,
            commands::delete_thread,
            commands::rename_thread,
            commands::list_user_facts,
            commands::save_user_fact,
            commands::delete_user_fact,
            commands::clear_user_facts,
            commands::set_tts_config,
            commands::tts_supported,
            commands::set_stt_config,
            commands::stt_status,
            commands::download_stt_model,
            commands::list_tts_voices,
            commands::preview_tts_voice,
            commands::speak_text,
            commands::stop_speaking,
            commands::is_speaking,
            commands::send_message,
            commands::regenerate_message,
            commands::edit_and_resend,
            commands::stop_generation,
            commands::start_host,
            commands::stop_host,
            commands::connect_client,
            commands::disconnect_client,
            commands::disconnect_and_forget,
            commands::reconnect_client,
            commands::generate_invite,
            commands::list_invites,
            commands::revoke_invite,
            commands::consume_invite,
            commands::redeem_invite_code,
            commands::list_peers,
            commands::revoke_peer,
            commands::toggle_overlay,
            commands::list_tools,
            commands::test_tool,
            commands::runtime_stats,
            commands::slot_health,
            commands::report_answer,
            commands::list_reports,
            commands::open_report_count,
            commands::set_report_reviewed,
            commands::delete_report,
            commands::delete_reviewed_reports,
            commands::set_llm_factcheck_settings,
            commands::fact_check_message,
            commands::check_updates,
            commands::install_update,
            commands::kinai_version,
            commands::open_logs_dir,
            commands::mark_setup_completed,
            commands::test_llm_endpoint,
            commands::get_changelog_payload,
            commands::mark_changelog_seen,
        ])
        .setup({
            let st = state.clone();
            move |app| {
                *st.handle.write() = Some(app.handle().clone());

                tray::install(app.handle(), st.clone())?;
                hotkey::install(app.handle(), st.clone())?;

                discovery::start_browser(app.handle().clone());
                updater::schedule_periodic_check(app.handle().clone());
                // Host-only: pull new GitHub releases hourly and stage
                // them for the family's connected clients to auto-update
                // to. Becomes the "GitHub → host → family" relay so the
                // host owner doesn't have to run scripts/deploy.sh
                // manually for every release.
                update_sync::schedule_periodic_sync(app.handle().clone());
                // Telegram long-poll loop — no-op when the host hasn't
                // set a bot token. Spawned via the supervisor so a token
                // change in Settings can stop+restart cleanly.
                let telegram_state = st.clone();
                let telegram_app = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = telegram::start_or_restart(telegram_state, telegram_app).await {
                        tracing::warn!("telegram start: {e:?}");
                    }
                });

                // Window-on-launch policy:
                //   * Manual launch (user double-clicked, dock click, etc.) →
                //     always show the window. Tray + hotkey are extras, not
                //     the only entry point.
                //   * OS-driven autostart launch (detected via the
                //     `--autostart` arg the LaunchAgent/Run-key entry passes
                //     in argv — see tauri_plugin_autostart::init in this
                //     file) → consult cfg.startup.autostart_minimized:
                //       - true (default)  → leave hidden; user opens via
                //         tray icon when they're ready.
                //       - false           → show normally, same as manual.
                let is_autostart_launch =
                    std::env::args().any(|a| a == "--autostart");
                let autostart_minimized =
                    st.config.read().startup.autostart_minimized;
                let show_at_startup =
                    !(is_autostart_launch && autostart_minimized);

                if let Some(w) = app.get_webview_window("main") {
                    if show_at_startup {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }

                    // Intercept the close button (X) so the window hides
                    // instead of being destroyed. Without this, on Windows
                    // the user can close the window, the process stays
                    // alive in the tray (which means the global hotkey
                    // still works), but launching KinAI again from the
                    // Start menu trips the single-instance handler which
                    // calls show() on a destroyed window — silent no-op,
                    // user sees nothing.
                    //
                    // Now the window is just hidden; show()/single-instance
                    // can bring it back. Real quit goes via tray → Quit or
                    // Cmd-Q.
                    let win = w.clone();
                    w.on_window_event(move |event| {
                        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                            let _ = win.hide();
                            api.prevent_close();
                        }
                    });
                }

                match st.config.read().mode {
                    Mode::Host => {
                        let s = st.clone();
                        let h = app.handle().clone();
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) = network::server::start(s, h).await {
                                tracing::error!("host server failed: {e:?}");
                            }
                        });
                    }
                    Mode::Client => {
                        let s = st.clone();
                        let h = app.handle().clone();
                        // Must use tauri::async_runtime::spawn here — `setup`
                        // runs on the main thread BEFORE Tauri's Tokio runtime
                        // is up, so calling `tokio::spawn` from this site
                        // panics with "must be called from within a Tokio
                        // runtime". The reconnect-race protection lives in
                        // `connect_client` (which runs inside an async command,
                        // where `tokio::spawn` is valid) — for the initial
                        // auto-connect we just fire-and-forget; if the user
                        // later hits Reconnect, that path stops any prior
                        // task before spawning a new one.
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) = network::client::auto_connect(s, h).await {
                                tracing::warn!("client auto-connect: {e:?}");
                            }
                        });
                    }
                    Mode::Unconfigured => {}
                }
                Ok(())
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building KinAI")
        .run(|app, event| match event {
            RunEvent::ExitRequested { api, .. } => {
                api.prevent_exit();
            }
            // macOS: clicking the dock icon while the app is already running.
            // Bring the main window forward instead of doing nothing.
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { has_visible_windows, .. } => {
                if !has_visible_windows {
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
            }
            _ => {}
        });
}
