//! KinAI library entrypoint — wires Tauri shell, Axum HTTP/WS server, tray,
//! global hotkey, LLM client, and the never-lose-context pipeline.

pub mod attachments;
pub mod auth;
pub mod commands;
pub mod comfyui;
pub mod config;
pub mod context;
pub mod db;
pub mod discovery;
pub mod hotkey;
pub mod llm;
pub mod network;
pub mod tools;
pub mod tray;
pub mod updater;
pub mod vision;

use std::sync::Arc;

use parking_lot::RwLock;
use tauri::{AppHandle, Manager, RunEvent};
use tokio::sync::Mutex;

use crate::config::{AppConfig, Mode};
use crate::db::Db;

pub struct AppState {
    pub handle: RwLock<Option<AppHandle>>,
    pub config: RwLock<AppConfig>,
    pub db: Db,
    pub llm: Mutex<llm::LlmClient>,
    pub net: Arc<Mutex<network::NetState>>,
    pub stats: RwLock<RuntimeStats>,
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
}

pub type SharedState = Arc<AppState>;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kinai=info,axum=info,tower_http=info".into()),
        )
        .init();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let cfg = AppConfig::load_or_default();
    tracing::info!("KinAI starting in {:?} mode", cfg.mode);

    let db = rt
        .block_on(Db::open(&cfg.db_path()))
        .expect("failed to open KinAI database");

    let llm = llm::LlmClient::new(cfg.llm.clone());
    let net = Arc::new(Mutex::new(network::NetState::default()));

    let state: SharedState = Arc::new(AppState {
        handle: RwLock::new(None),
        config: RwLock::new(cfg.clone()),
        db,
        llm: Mutex::new(llm),
        net: net.clone(),
        stats: RwLock::new(RuntimeStats::default()),
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
            commands::set_overlay_settings,
            commands::set_theme,
            commands::set_vision_settings,
            commands::test_vision_endpoint,
            commands::set_comfy_config,
            commands::test_comfy_endpoint,
            commands::detect_backends,
            commands::scan_local_network,
            commands::rescan_kinai_hosts,
            commands::local_ip,
            commands::query_model_caps,
            commands::list_local_models,
            commands::test_llm_connection,
            commands::list_threads,
            commands::load_thread,
            commands::create_thread,
            commands::delete_thread,
            commands::rename_thread,
            commands::send_message,
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
            commands::check_updates,
            commands::install_update,
            commands::kinai_version,
        ])
        .setup({
            let st = state.clone();
            move |app| {
                *st.handle.write() = Some(app.handle().clone());

                tray::install(app.handle(), st.clone())?;
                hotkey::install(app.handle(), st.clone())?;

                discovery::start_browser(app.handle().clone());
                updater::schedule_periodic_check(app.handle().clone());

                // Always surface the main window when the app starts. Tray +
                // hotkey are extras, not the only entry point.
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();

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
