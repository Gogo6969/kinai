//! Tauri command surface — every IPC entrypoint the frontend can call.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::config::{
    AppConfig, ClientSettings, HostSettings, LlmSettings, Mode, OverlaySettings, Theme,
    ToolSettings, VisionSettings,
};
use crate::context;
use crate::db;
use crate::llm::detect::DetectedBackend;
use crate::network::invite;
use crate::network::server::PeerSummary;
use crate::tools::loop_pipeline::{PipelineHandlers, ToolEvent};
use crate::tools::registry;
use crate::{network, tools, updater, SharedState};
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Resolve the per-request `max_tokens`:
///   * If user set `max_tokens > 0` in config → cap at that, but never exceed
///     `context_window - prompt_tokens - safety`.
///   * If user set `max_tokens == 0` → use the full remaining budget after the
///     prompt (auto). Reasoning models in particular need this.
fn compute_max_tokens(
    cfg: &AppConfig,
    messages: &[crate::context::ChatMessage],
) -> Option<usize> {
    const SAFETY: usize = 128;
    let prompt = crate::context::token_guard::estimate_messages(messages);
    let budget = cfg
        .llm
        .context_window
        .saturating_sub(prompt + SAFETY)
        .max(256);
    let max_tokens = if cfg.llm.max_tokens == 0 {
        budget
    } else {
        cfg.llm.max_tokens.min(budget)
    };
    Some(max_tokens)
}

// ---- Config ----

#[tauri::command]
pub async fn get_config(state: tauri::State<'_, SharedState>) -> Result<AppConfig> {
    Ok(state.config.read().clone())
}

#[derive(Debug, Deserialize)]
pub struct SetModeArgs {
    pub mode: Mode,
    pub host: Option<HostSettings>,
    pub client: Option<ClientSettings>,
    pub overlay: Option<OverlaySettings>,
    pub tools: Option<ToolSettings>,
}

#[tauri::command]
pub async fn set_mode(
    state: tauri::State<'_, SharedState>,
    args: SetModeArgs,
) -> Result<AppConfig> {
    let new_cfg = {
        let mut cfg = state.config.write();
        cfg.mode = args.mode;
        if let Some(h) = args.host {
            cfg.host = h;
        }
        if let Some(c) = args.client {
            cfg.client = c;
        }
        if let Some(o) = args.overlay {
            cfg.overlay = o;
        }
        if let Some(t) = args.tools {
            cfg.tools = t;
        }
        cfg.save().map_err(err)?;
        cfg.clone()
    };
    Ok(new_cfg)
}

#[tauri::command]
pub async fn set_llm_settings(
    state: tauri::State<'_, SharedState>,
    llm: LlmSettings,
) -> Result<AppConfig> {
    let new_cfg = {
        let mut cfg = state.config.write();
        cfg.llm = llm.clone();
        cfg.save().map_err(err)?;
        cfg.clone()
    };
    let mut client = state.llm.lock().await;
    *client = crate::llm::LlmClient::new(llm);
    state.stats.write().model_loaded = Some(new_cfg.llm.model.clone());
    Ok(new_cfg)
}

#[tauri::command]
pub async fn set_overlay_settings(
    app: AppHandle,
    state: tauri::State<'_, SharedState>,
    overlay: OverlaySettings,
) -> Result<AppConfig> {
    let new_cfg = {
        let mut cfg = state.config.write();
        cfg.overlay = overlay.clone();
        cfg.save().map_err(err)?;
        cfg.clone()
    };
    if let Err(e) = crate::hotkey::replace(&app, &overlay.hotkey) {
        tracing::warn!("hotkey replace failed: {e:?}");
    }
    Ok(new_cfg)
}

#[tauri::command]
pub async fn set_theme(
    state: tauri::State<'_, SharedState>,
    theme: Theme,
) -> Result<AppConfig> {
    let new_cfg = {
        let mut cfg = state.config.write();
        cfg.theme = theme;
        cfg.save().map_err(err)?;
        cfg.clone()
    };
    Ok(new_cfg)
}

#[tauri::command]
pub async fn set_vision_settings(
    state: tauri::State<'_, SharedState>,
    vision: VisionSettings,
) -> Result<AppConfig> {
    let new_cfg = {
        let mut cfg = state.config.write();
        cfg.vision = vision;
        cfg.save().map_err(err)?;
        cfg.clone()
    };
    Ok(new_cfg)
}

#[derive(Debug, Deserialize)]
pub struct TestVisionArgs {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TestVisionResult {
    pub ok: bool,
    pub latency_ms: u64,
    pub error: Option<String>,
    /// Echo of the model's reply on success — usually a 1–2 word description
    /// of the test pixel ("a black square", "white", etc.). Empty on failure.
    pub reply: String,
}

/// Send a 1-pixel PNG with a "what color?" prompt to verify a vision
/// endpoint can be reached, accepts multipart `content`, and returns
/// something sensible. Used by the Settings UI's "Test vision" button.
#[tauri::command]
pub async fn test_vision_endpoint(args: TestVisionArgs) -> Result<TestVisionResult> {
    use crate::context::ChatMessage;
    let started = std::time::Instant::now();
    // A 1×1 transparent PNG, base64-encoded. Big enough to be a valid
    // image payload, small enough that no provider rejects it on size.
    const ONE_PX_PNG: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
    let data_url = format!("data:image/png;base64,{}", ONE_PX_PNG);
    let settings = crate::config::LlmSettings {
        provider: "openai-compat".into(),
        base_url: args.base_url,
        model: args.model,
        api_key: args.api_key,
        context_window: 4096,
        temperature: 0.0,
        max_tokens: 64,
        system_addendum: String::new(),
    };
    let client = crate::llm::LlmClient::new(settings);
    let messages = vec![ChatMessage::User {
        content: "Reply with only the predominant color of this image, one word.".into(),
        name: None,
        image_data_urls: vec![data_url],
    }];
    let result = client.complete(&messages, &[], Some(64)).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(c) => Ok(TestVisionResult {
            ok: true,
            latency_ms,
            error: None,
            reply: c.content.trim().to_string(),
        }),
        Err(e) => Ok(TestVisionResult {
            ok: false,
            latency_ms,
            error: Some(e.to_string()),
            reply: String::new(),
        }),
    }
}

/// Write a UTF-8 string to `~/.kinai/prompts/<msg_id>.json` and
/// open it in the user's default editor. Used by the chat UI's "🔍
/// prompt" button. Combines the file-write + native open into one
/// command because the tauri-plugin-shell `open` builtin restricts to
/// URL schemes (https/mailto/tel) — file paths get rejected by its
/// regex-based capability scope.
///
/// Native open on each platform:
///   macOS   → `open <path>`
///   Windows → `cmd /C start "" <path>`  (the empty title is the
///             quirk that lets `start` accept a path arg cleanly)
///   Linux   → `xdg-open <path>`
#[tauri::command]
pub async fn write_prompt_snapshot(msg_id: String, body: String) -> Result<String> {
    let safe = msg_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect::<String>();
    if safe.is_empty() {
        return Err("invalid msg_id".into());
    }
    let dir = dirs::home_dir()
        .ok_or_else(|| "no home directory".to_string())?
        .join(".kinai")
        .join("prompts");
    std::fs::create_dir_all(&dir).map_err(err)?;
    let path = dir.join(format!("{safe}.json"));
    std::fs::write(&path, body).map_err(err)?;

    let path_str = path.to_string_lossy().to_string();
    open_path_in_default_app(&path_str).map_err(|e| format!("file written but couldn't open: {e}"))?;
    Ok(path_str)
}

#[cfg(target_os = "macos")]
fn open_path_in_default_app(path: &str) -> std::io::Result<()> {
    // `open -t` opens with the default PLAIN-TEXT editor (TextEdit by
    // default), independent of the file's extension association. Without
    // -t, .json routes to Xcode for users with Xcode installed, which is
    // overkill for "I just want to read what was sent to the LLM."
    std::process::Command::new("open").args(["-t", path]).spawn()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_path_in_default_app(path: &str) -> std::io::Result<()> {
    std::process::Command::new("cmd")
        .args(["/C", "start", "", path])
        .spawn()?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn open_path_in_default_app(path: &str) -> std::io::Result<()> {
    std::process::Command::new("xdg-open").arg(path).spawn()?;
    Ok(())
}

/// Fetch the bytes at `url` and write them to `dest_path`.
///
/// Used by the chat UI's image Download button. The webview's `fetch()`
/// can't reach `http://192.168.1.x:4847` on macOS because WebKit's ATS
/// blocks plain HTTP requests from JS (even though `<img src>` loads
/// the same URL fine via a different code path). reqwest from Rust has
/// no such restriction.
#[tauri::command]
pub async fn download_url_to_path(url: String, dest_path: String) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(err)?;
    let resp = client.get(&url).send().await.map_err(err)?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} fetching {url}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(err)?;
    std::fs::write(&dest_path, &bytes).map_err(err)?;
    Ok(())
}

#[tauri::command]
pub async fn set_comfy_config(
    state: tauri::State<'_, SharedState>,
    comfyui: crate::config::ComfyConfig,
) -> Result<AppConfig> {
    let new_cfg = {
        let mut cfg = state.config.write();
        cfg.comfyui = comfyui;
        cfg.save().map_err(err)?;
        cfg.clone()
    };
    Ok(new_cfg)
}

// ============================================================
// Telegram
// ============================================================

#[derive(Debug, Deserialize)]
pub struct TelegramTokenArgs {
    pub bot_token: String,
}

#[derive(Debug, Serialize)]
pub struct TelegramTokenResult {
    pub ok: bool,
    pub bot_username: Option<String>,
    pub error: Option<String>,
}

/// Validate a bot token without saving it: probes getMe and returns
/// the bot's @username on success. Used by the Settings "Test" button.
#[tauri::command]
pub async fn test_telegram_token(args: TelegramTokenArgs) -> Result<TelegramTokenResult> {
    let token = args.bot_token.trim().to_string();
    if token.is_empty() {
        return Ok(TelegramTokenResult {
            ok: false,
            bot_username: None,
            error: Some("Token is empty".into()),
        });
    }
    let api = crate::telegram::BotApi::new(token);
    match api.get_me().await {
        Ok(me) => Ok(TelegramTokenResult {
            ok: true,
            bot_username: me.username,
            error: None,
        }),
        Err(e) => Ok(TelegramTokenResult {
            ok: false,
            bot_username: None,
            error: Some(e.to_string()),
        }),
    }
}

/// Save the bot token to config + (re)start the long-poll loop. Empty
/// token disables the feature and stops the loop.
#[tauri::command]
pub async fn set_telegram_token(
    app: AppHandle,
    state: tauri::State<'_, SharedState>,
    args: TelegramTokenArgs,
) -> Result<AppConfig> {
    let cleaned = args.bot_token.trim().to_string();
    {
        let mut cfg = state.config.write();
        cfg.telegram.bot_token = cleaned.clone();
        if cleaned.is_empty() {
            cfg.telegram.bot_username = String::new();
        }
        cfg.save().map_err(err)?;
    }
    // Restart the supervisor with the new token. Runs in background so
    // the IPC call returns quickly; if validation fails the user sees
    // it via the next status poll (or the Test button beforehand).
    let st = (*state).clone();
    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = crate::telegram::start_or_restart(st, app_clone).await {
            tracing::warn!("telegram restart: {e:?}");
        }
    });
    Ok(state.config.read().clone())
}

#[derive(Debug, Serialize)]
pub struct TelegramPairResult {
    /// The deep-link URL: `https://t.me/<bot>?start=<token>`. The
    /// frontend turns this into a QR code.
    pub url: String,
    /// How long (seconds) before the token expires. Currently 600.
    pub expires_in_secs: i64,
}

/// Generate a one-time pairing token and return the deep-link URL the
/// user scans with their phone.
///
/// **Mode-aware:**
///   * Host mode → mint the token directly against the local DB for
///     `HOST_PEER`.
///   * Client mode → send a `RequestTelegramPair` envelope over the WS
///     to the host, park a oneshot on `NetState`, and await the host's
///     `TelegramPair` response (resolves the oneshot in
///     `network::client`). The token is minted on the host with the
///     *client's* `context_peer` (their invite short-code), so the
///     resulting pair binds the client's KinAI account — not the host's.
///
/// Either way, the frontend gets a uniform `{ url, expires_in_secs }`
/// back and renders the same QR card.
#[tauri::command]
pub async fn request_telegram_pair(
    state: tauri::State<'_, SharedState>,
) -> Result<TelegramPairResult> {
    let mode = state.config.read().mode;
    match mode {
        crate::config::Mode::Client => {
            client_request_telegram_pair(&state).await
        }
        _ => {
            let bot_username = state.config.read().telegram.bot_username.clone();
            if bot_username.is_empty() {
                return Err(
                    "Telegram bot isn't set up yet. Paste a bot token from @BotFather first.".into(),
                );
            }
            let token =
                crate::db::telegram::create_pending_pair(&state.db.pool, db::HOST_PEER)
                    .await
                    .map_err(err)?;
            Ok(TelegramPairResult {
                url: format!("https://t.me/{bot_username}?start={token}"),
                expires_in_secs: 600,
            })
        }
    }
}

async fn client_request_telegram_pair(
    state: &tauri::State<'_, SharedState>,
) -> Result<TelegramPairResult> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let tx = {
        let mut net = state.net.lock().await;
        // Replace any in-flight oneshot — the previous caller's await
        // will drop with a Cancelled error, which their `?` turns into
        // a user-facing "request failed" string. That's fine: the only
        // way to get into this state is double-clicking the QR button.
        net.telegram_pair_pending = Some(sender);
        net.client_tx.clone()
    };
    let Some(tx) = tx else {
        // Clear the stash we just installed so a later reconnect
        // doesn't see a stale sender.
        state.net.lock().await.telegram_pair_pending = None;
        return Err("Not connected to a KinAI host. Reconnect and try again.".into());
    };
    if tx.send(crate::network::protocol::Envelope::RequestTelegramPair).is_err() {
        state.net.lock().await.telegram_pair_pending = None;
        return Err("KinAI host disconnected before we could send the pair request.".into());
    }
    let resp = tokio::time::timeout(std::time::Duration::from_secs(15), receiver)
        .await
        .map_err(|_| "Timed out waiting for the host to mint a pairing code.".to_string())?
        .map_err(|_| "Host dropped the pair request without responding.".to_string())?;
    if resp.url.is_empty() {
        // Sentinel from server.rs — bot isn't configured on the host.
        return Err(
            "The family Telegram bot isn't set up yet. Ask the host owner to configure it in Settings → Telegram on their machine."
                .into(),
        );
    }
    Ok(TelegramPairResult {
        url: resp.url,
        expires_in_secs: resp.expires_in_secs,
    })
}

#[derive(Debug, Serialize)]
pub struct TelegramLinkStatus {
    pub bot_configured: bool,
    pub bot_username: String,
    pub paired: bool,
    pub username: Option<String>,
    pub first_name: Option<String>,
    pub paired_at: Option<String>,
}

/// Current pairing state for the caller's peer (host in Host mode, the
/// connected client peer in Client mode). Used by the Settings UI to
/// render "Paired as @foo · since DATE" + an Unpair button when
/// already paired, or the pair button when not.
#[tauri::command]
pub async fn telegram_link_status(
    state: tauri::State<'_, SharedState>,
) -> Result<TelegramLinkStatus> {
    let mode = state.config.read().mode;
    match mode {
        crate::config::Mode::Client => client_request_telegram_status(&state).await,
        _ => {
            let cfg = state.config.read().clone();
            let link = crate::db::telegram::link_for_peer(&state.db.pool, db::HOST_PEER)
                .await
                .map_err(err)?;
            Ok(TelegramLinkStatus {
                bot_configured: !cfg.telegram.bot_token.trim().is_empty(),
                bot_username: cfg.telegram.bot_username.clone(),
                paired: link.is_some(),
                username: link.as_ref().and_then(|l| l.username.clone()),
                first_name: link.as_ref().and_then(|l| l.first_name.clone()),
                paired_at: link.as_ref().map(|l| l.paired_at.clone()),
            })
        }
    }
}

async fn client_request_telegram_status(
    state: &tauri::State<'_, SharedState>,
) -> Result<TelegramLinkStatus> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let tx = {
        let mut net = state.net.lock().await;
        net.telegram_status_pending = Some(sender);
        net.client_tx.clone()
    };
    let Some(tx) = tx else {
        // Status polling is a passive UI refresh — if the WS isn't up
        // yet (Client mode but not connected), return a neutral
        // "nothing to show" payload instead of an error toast.
        state.net.lock().await.telegram_status_pending = None;
        return Ok(TelegramLinkStatus {
            bot_configured: false,
            bot_username: String::new(),
            paired: false,
            username: None,
            first_name: None,
            paired_at: None,
        });
    };
    if tx.send(crate::network::protocol::Envelope::RequestTelegramStatus).is_err() {
        state.net.lock().await.telegram_status_pending = None;
        return Err("KinAI host disconnected before we could fetch Telegram status.".into());
    }
    let resp = tokio::time::timeout(std::time::Duration::from_secs(10), receiver)
        .await
        .map_err(|_| "Timed out waiting for Telegram status from the host.".to_string())?
        .map_err(|_| "Host dropped the status request.".to_string())?;
    Ok(TelegramLinkStatus {
        bot_configured: resp.bot_configured,
        bot_username: resp.bot_username,
        paired: resp.paired,
        username: resp.username,
        first_name: resp.first_name,
        paired_at: resp.paired_at,
    })
}

#[tauri::command]
pub async fn unpair_telegram(state: tauri::State<'_, SharedState>) -> Result<()> {
    let mode = state.config.read().mode;
    match mode {
        crate::config::Mode::Client => client_request_telegram_unpair(&state).await,
        _ => {
            crate::db::telegram::unpair(&state.db.pool, db::HOST_PEER)
                .await
                .map_err(err)
        }
    }
}

async fn client_request_telegram_unpair(
    state: &tauri::State<'_, SharedState>,
) -> Result<()> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let tx = {
        let mut net = state.net.lock().await;
        net.telegram_unpair_pending = Some(sender);
        net.client_tx.clone()
    };
    let Some(tx) = tx else {
        state.net.lock().await.telegram_unpair_pending = None;
        return Err("Not connected to a KinAI host. Reconnect and try again.".into());
    };
    if tx
        .send(crate::network::protocol::Envelope::RequestTelegramUnpair)
        .is_err()
    {
        state.net.lock().await.telegram_unpair_pending = None;
        return Err("KinAI host disconnected before we could send the unpair request.".into());
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), receiver)
        .await
        .map_err(|_| "Timed out waiting for the host to confirm unpair.".to_string())?
        .map_err(|_| "Host dropped the unpair request.".to_string())?;
    Ok(())
}

// ============================================================
// Changelog modal
// ============================================================

#[derive(Debug, Serialize)]
pub struct ChangelogPayload {
    /// Current binary version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// Markdown body for the current version's section, parsed out of
    /// the embedded `CHANGELOG.md`. `None` when the binary version
    /// has no matching `## [x.y.z]` heading (dev builds, mid-release).
    pub markdown: Option<String>,
    /// True when the user hasn't acknowledged this version yet —
    /// i.e. `last_seen_changelog_version != current_version` AND we
    /// have a markdown section to show. Frontend uses this to decide
    /// whether to open the modal automatically after launch.
    pub should_show: bool,
}

/// Fetch the changelog entry for the running binary. Always returns a
/// payload — the frontend decides what to do with it based on
/// `should_show`.
#[tauri::command]
pub async fn get_changelog_payload(
    state: tauri::State<'_, SharedState>,
) -> Result<ChangelogPayload> {
    let version = crate::changelog::current_version().to_string();
    let last_seen = state.config.read().last_seen_changelog_version.clone();
    let markdown = crate::changelog::section_for_version(&version);
    let should_show = markdown.is_some() && last_seen != version;
    Ok(ChangelogPayload {
        version,
        markdown,
        should_show,
    })
}

/// Stamp the running binary's version into `last_seen_changelog_version`
/// so the modal doesn't reopen until the next upgrade. Saves config to
/// disk synchronously.
#[tauri::command]
pub async fn mark_changelog_seen(
    state: tauri::State<'_, SharedState>,
) -> Result<()> {
    let mut cfg = state.config.write();
    cfg.last_seen_changelog_version = crate::changelog::current_version().to_string();
    cfg.save().map_err(err)
}

#[derive(Debug, Deserialize)]
pub struct TestComfyArgs {
    pub base_url: String,
}

#[derive(Debug, Serialize)]
pub struct TestComfyResult {
    pub ok: bool,
    pub latency_ms: u64,
    pub error: Option<String>,
}

/// Ping a ComfyUI server's `/system_stats` endpoint. Used by the
/// Settings UI's "Test image-gen" button. No image is generated — just
/// confirms the URL is reachable and returns valid JSON.
#[tauri::command]
pub async fn test_comfy_endpoint(args: TestComfyArgs) -> Result<TestComfyResult> {
    let started = std::time::Instant::now();
    let base = args.base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Ok(TestComfyResult {
            ok: false,
            latency_ms: 0,
            error: Some("URL is empty".into()),
        });
    }
    let url = format!("{base}/system_stats");
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Ok(TestComfyResult {
                ok: false,
                latency_ms: started.elapsed().as_millis() as u64,
                error: Some(format!("http client init: {e}")),
            })
        }
    };
    match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => Ok(TestComfyResult {
            ok: true,
            latency_ms: started.elapsed().as_millis() as u64,
            error: None,
        }),
        Ok(r) => Ok(TestComfyResult {
            ok: false,
            latency_ms: started.elapsed().as_millis() as u64,
            error: Some(format!("HTTP {}", r.status())),
        }),
        Err(e) => Ok(TestComfyResult {
            ok: false,
            latency_ms: started.elapsed().as_millis() as u64,
            error: Some(e.to_string()),
        }),
    }
}

// ---- Backend detection ----

#[tauri::command]
pub async fn detect_backends() -> Result<Vec<DetectedBackend>> {
    Ok(crate::llm::detect::detect_all().await)
}

#[tauri::command]
pub async fn scan_local_network() -> Result<Vec<DetectedBackend>> {
    Ok(crate::llm::detect::scan_local_network().await)
}

/// Re-issue an mDNS query for `_kinai._tcp.local.` so any host that has
/// already announced gets re-resolved and re-emitted to the UI. Used by
/// the Client setup page's "Scan again" button.
#[tauri::command]
pub async fn rescan_kinai_hosts() -> Result<()> {
    crate::discovery::rescan().map_err(err)
}

#[derive(Debug, Serialize)]
pub struct LocalIpInfo {
    pub ip: Option<String>,
    pub hostname: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QueryModelCapsArgs {
    pub provider: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
}

#[tauri::command]
pub async fn query_model_caps(args: QueryModelCapsArgs) -> Result<crate::llm::detect::ModelCaps> {
    crate::llm::detect::query_model_caps(
        &args.provider,
        &args.base_url,
        args.api_key.as_deref(),
        &args.model,
    )
    .await
    .map_err(err)
}

#[derive(Debug, Serialize)]
pub struct VersionInfo {
    pub version: &'static str,
    pub build_time: u64,
    pub git_commit: &'static str,
    pub target: &'static str,
    pub repository: &'static str,
}

#[tauri::command]
pub async fn kinai_version() -> Result<VersionInfo> {
    Ok(VersionInfo {
        version: env!("CARGO_PKG_VERSION"),
        build_time: option_env!("KINAI_BUILD_TIME")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        git_commit: option_env!("KINAI_GIT_COMMIT").unwrap_or("unknown"),
        target: if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
            "macos-aarch64"
        } else if cfg!(target_os = "macos") && cfg!(target_arch = "x86_64") {
            "macos-x86_64"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            "unknown"
        },
        // Single source of truth — set in Cargo.toml [package].repository.
        // Forks update Cargo.toml once and every URL in the app + every
        // outbound User-Agent picks it up automatically.
        repository: env!("CARGO_PKG_REPOSITORY"),
    })
}

#[tauri::command]
pub async fn local_ip() -> Result<LocalIpInfo> {
    let ip = local_ip_address::local_ip().ok().map(|i| i.to_string());
    let hostname = hostname_string();
    Ok(LocalIpInfo { ip, hostname })
}

fn hostname_string() -> Option<String> {
    std::env::var("HOST")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

#[tauri::command]
pub async fn list_local_models(state: tauri::State<'_, SharedState>) -> Result<Vec<String>> {
    let llm = state.llm.lock().await.clone();
    llm.list_models().await.map_err(err)
}

#[derive(Debug, Deserialize)]
pub struct TestConnectionArgs {
    pub provider: String,
    pub base_url: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TestConnectionResult {
    pub ok: bool,
    pub models: Vec<String>,
    pub error: Option<String>,
    pub latency_ms: u64,
}

#[tauri::command]
pub async fn test_llm_connection(args: TestConnectionArgs) -> Result<TestConnectionResult> {
    let probe_settings = LlmSettings {
        provider: args.provider,
        base_url: args.base_url,
        api_key: args.api_key,
        ..LlmSettings::default()
    };
    let start = std::time::Instant::now();
    match crate::llm::detect::list_models(&probe_settings).await {
        Ok(models) => Ok(TestConnectionResult {
            ok: true,
            models,
            error: None,
            latency_ms: start.elapsed().as_millis() as u64,
        }),
        Err(e) => Ok(TestConnectionResult {
            ok: false,
            models: Vec::new(),
            error: Some(e.to_string()),
            latency_ms: start.elapsed().as_millis() as u64,
        }),
    }
}

// ---- Threads ----
//
// Every thread/message Tauri command operates on the LOCAL DB, which on the
// host machine holds many peers' conversations side by side. The UI that
// calls these commands is always either the host's own UI or a client's
// own UI looking at its own local cache — neither one should ever see
// another peer's threads. We hardcode `HOST_PEER` ("host") here because:
//   * On a host machine the UI in front of you IS the host user
//   * On a client machine the local DB only has that client's own data,
//     so peer_id is effectively a no-op (all rows are "host" by default)

#[tauri::command]
pub async fn list_threads(state: tauri::State<'_, SharedState>) -> Result<Vec<db::ThreadMeta>> {
    // Mode-aware. Host reads its local DB (HOST_PEER bucket); client
    // peers ask the host over the WebSocket for the threads belonging
    // to them (their invite_id bucket). Without this WS round-trip,
    // client UIs couldn't see threads that exist only in the host's DB
    // — most importantly Telegram-originated threads, which are created
    // server-side when the bot first sees a paired chat.
    if matches!(state.config.read().mode, Mode::Client) {
        return client_list_threads(&state).await;
    }
    state.db.list_threads(db::HOST_PEER).await.map_err(err)
}

async fn client_list_threads(
    state: &tauri::State<'_, SharedState>,
) -> Result<Vec<db::ThreadMeta>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let tx = {
        let mut net = state.net.lock().await;
        net.threads_pending = Some(sender);
        net.client_tx.clone()
    };
    let Some(tx) = tx else {
        // No live WS — fall back to local DB. Better than an error
        // toast on app boot when the client is still reconnecting.
        state.net.lock().await.threads_pending = None;
        return state.db.list_threads(db::HOST_PEER).await.map_err(err);
    };
    if tx
        .send(crate::network::protocol::Envelope::ListThreads)
        .is_err()
    {
        state.net.lock().await.threads_pending = None;
        return state.db.list_threads(db::HOST_PEER).await.map_err(err);
    }
    match tokio::time::timeout(std::time::Duration::from_secs(10), receiver).await {
        Ok(Ok(threads)) => Ok(threads),
        _ => {
            // Timeout or sender dropped — fall back to local DB rather
            // than failing the UI's startup load.
            state.net.lock().await.threads_pending = None;
            state.db.list_threads(db::HOST_PEER).await.map_err(err)
        }
    }
}

#[tauri::command]
pub async fn load_thread(
    state: tauri::State<'_, SharedState>,
    thread_id: String,
) -> Result<Vec<db::Message>> {
    if matches!(state.config.read().mode, Mode::Client) {
        return client_load_thread(&state, &thread_id).await;
    }
    state
        .db
        .load_messages(db::HOST_PEER, &thread_id, 500)
        .await
        .map_err(err)
}

async fn client_load_thread(
    state: &tauri::State<'_, SharedState>,
    thread_id: &str,
) -> Result<Vec<db::Message>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let tx = {
        let mut net = state.net.lock().await;
        net.thread_messages_pending = Some(sender);
        net.client_tx.clone()
    };
    let Some(tx) = tx else {
        state.net.lock().await.thread_messages_pending = None;
        return state
            .db
            .load_messages(db::HOST_PEER, thread_id, 500)
            .await
            .map_err(err);
    };
    if tx
        .send(crate::network::protocol::Envelope::LoadThread {
            thread_id: thread_id.to_string(),
        })
        .is_err()
    {
        state.net.lock().await.thread_messages_pending = None;
        return state
            .db
            .load_messages(db::HOST_PEER, thread_id, 500)
            .await
            .map_err(err);
    }
    match tokio::time::timeout(std::time::Duration::from_secs(10), receiver).await {
        Ok(Ok(messages)) => Ok(messages),
        _ => {
            state.net.lock().await.thread_messages_pending = None;
            state
                .db
                .load_messages(db::HOST_PEER, thread_id, 500)
                .await
                .map_err(err)
        }
    }
}

#[tauri::command]
pub async fn create_thread(
    state: tauri::State<'_, SharedState>,
    title: Option<String>,
) -> Result<db::ThreadMeta> {
    state
        .db
        .create_thread(db::HOST_PEER, title.as_deref())
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn delete_thread(
    state: tauri::State<'_, SharedState>,
    thread_id: String,
) -> Result<()> {
    state
        .db
        .delete_thread(db::HOST_PEER, &thread_id)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn rename_thread(
    state: tauri::State<'_, SharedState>,
    thread_id: String,
    title: String,
) -> Result<()> {
    state
        .db
        .rename_thread(db::HOST_PEER, &thread_id, &title)
        .await
        .map_err(err)
}

// ---- Chat ----

#[derive(Debug, Deserialize)]
pub struct SendMessageArgs {
    pub thread_id: String,
    pub content: String,
    pub client_msg_id: String,
    #[serde(default)]
    pub sender: Option<String>,
    /// Files attached to this turn — images, PDFs, etc. Empty for plain
    /// text chats. The server side decides whether to extract text from
    /// them (PDF) or route the call to a vision endpoint (future).
    #[serde(default)]
    pub attachments: Vec<db::Attachment>,
}

#[derive(Debug, Serialize, Clone)]
pub struct TurnMetrics {
    pub first_token_ms: u64,
    pub total_ms: u64,
    pub output_tokens: u64,
    pub tps: f64,
}

#[derive(Debug, Serialize)]
pub struct SendMessageResult {
    pub user_message: db::Message,
    pub assistant_message: db::Message,
    pub metrics: TurnMetrics,
}

#[tauri::command]
pub async fn send_message(
    app: AppHandle,
    state: tauri::State<'_, SharedState>,
    args: SendMessageArgs,
) -> Result<SendMessageResult> {
    let sender = args
        .sender
        .unwrap_or_else(|| state.config.read().client.display_name.clone());

    // -- Client mode: forward to host over the live WebSocket. The host is
    // the source of truth for messages — it persists them and broadcasts
    // `Envelope::Message` back to every connected peer (including the
    // sender). The client task in `network::client::connect` re-emits each
    // inbound envelope as a Tauri event so the existing UI listeners pick
    // them up unchanged. We deliberately DON'T append locally here, because
    // then the host's echo would arrive with a different id and the UI
    // store (which dedups by id) would display the user's message twice.
    if matches!(state.config.read().mode, Mode::Client) {
        let tx = {
            let net = state.net.lock().await;
            net.client_tx.clone()
        };
        let Some(tx) = tx else {
            return Err(
                "Not connected to a KinAI host. Open Settings → Client to reconnect."
                    .into(),
            );
        };
        let envelope = network::protocol::Envelope::SendMessage {
            thread_id: args.thread_id.clone(),
            content: args.content.clone(),
            sender: sender.clone(),
            client_msg_id: args.client_msg_id.clone(),
            attachments: args.attachments.clone(),
        };
        tx.send(envelope).map_err(|e| {
            format!("KinAI host channel closed mid-send: {e}")
        })?;

        // The Tauri command contract returns the persisted messages, but in
        // client mode they don't exist locally — they arrive asynchronously
        // via the kinai:// event stream. Return placeholder shells so the
        // command resolves; the UI's frontend store already drives itself
        // off the event stream, not this return value, so the placeholders
        // are never actually displayed.
        let now = chrono::Utc::now().to_rfc3339();
        let user_placeholder = db::Message {
            id: format!("pending-user-{}", args.client_msg_id),
            thread_id: args.thread_id.clone(),
            role: "user".into(),
            sender: sender.clone(),
            content: args.content.clone(),
            attachments: args.attachments.clone(),
            created_at: now.clone(),
            summarized_into: None,
            metrics: None,
        };
        let assistant_placeholder = db::Message {
            id: format!("pending-{}", args.client_msg_id),
            thread_id: args.thread_id.clone(),
            role: "assistant".into(),
            sender: "KinAI".into(),
            content: String::new(),
            attachments: vec![],
            created_at: now,
            summarized_into: None,
            metrics: None,
        };
        return Ok(SendMessageResult {
            user_message: user_placeholder,
            assistant_message: assistant_placeholder,
            metrics: TurnMetrics {
                first_token_ms: 0,
                total_ms: 0,
                output_tokens: 0,
                tps: 0.0,
            },
        });
    }

    // Persist the user's typed prose verbatim — attachment text is
    // extracted at context-build time so the chat history shows what
    // the user actually wrote, not a wall of PDF body text.
    let user_msg = state
        .db
        .append_message(
            &args.thread_id,
            "user",
            &sender,
            &args.content,
            &args.attachments,
        )
        .await
        .map_err(err)?;
    let _ = app.emit("kinai://message", &user_msg);

    let cfg = state.config.read().clone();

    // Slash commands (/pic, /picHQ, /help, ?) are intercepted BEFORE the
    // LLM pipeline. Same handler the WebSocket dispatcher uses for client
    // peers — keeps the two chat paths identical.
    if let Some(reply) = crate::slash::handle(&cfg, &args.content).await {
        let started_at = std::time::Instant::now();
        let mut assistant_msg = state
            .db
            .append_message(&args.thread_id, "assistant", "KinAI", &reply, &[])
            .await
            .map_err(err)?;
        let total_ms = started_at.elapsed().as_millis() as u64;
        let metrics = TurnMetrics {
            first_token_ms: 0,
            total_ms,
            output_tokens: 0,
            tps: 0.0,
        };
        let metrics_json = serde_json::to_value(&metrics).unwrap_or(serde_json::Value::Null);
        let _ = state
            .db
            .set_message_metrics(&assistant_msg.id, &metrics_json)
            .await;
        assistant_msg.metrics = Some(metrics_json);
        let _ = app.emit("kinai://message", &assistant_msg);
        // Same final emit shape the LLM path uses below, so the UI's
        // assistant-done listener treats this turn identically.
        let _ = app.emit(
            "kinai://assistant-done",
            serde_json::json!({
                "client_msg_id": &args.client_msg_id,
                "message": &assistant_msg,
                "metrics": &metrics,
            }),
        );
        // Bidirectional Telegram sync: if the host owner is chatting
        // in their own Telegram thread, mirror the slash-command reply
        // back to Telegram. No-op on regular threads.
        crate::telegram::echo::maybe_echo_assistant(
            &state,
            db::HOST_PEER,
            &args.thread_id,
            &assistant_msg.content,
        )
        .await;
        return Ok(SendMessageResult {
            user_message: user_msg,
            assistant_message: assistant_msg,
            metrics,
        });
    }

    let messages =
        context::builder::build_context(&state.db, &cfg, db::HOST_PEER, &args.thread_id, &user_msg)
            .await
            .map_err(err)?;
    // Snapshot for the 🔍 debug panel — emitted alongside the
    // assistant message after it lands. Same shape as the client-peer
    // path in network/server.rs. Inline image data URLs are stripped
    // out so a single attached PNG doesn't bloat the JSON to 7-8 MB.
    let prompt_debug = serde_json::to_string_pretty(
        &messages
            .iter()
            .map(|m| m.redacted_for_debug())
            .collect::<Vec<_>>(),
    )
    .ok();
    let tool_defs = registry::enabled(&cfg.tools);
    let tool_runtime = registry::ToolRuntime::from_tool_settings(&cfg.tools);

    let max_tokens = compute_max_tokens(&cfg, &messages);

    let llm = state.llm.lock().await.clone();
    let cancel = CancellationToken::new();

    let app_for_token = app.clone();
    let client_id_token = args.client_msg_id.clone();
    let app_for_reasoning = app.clone();
    let client_id_reasoning = args.client_msg_id.clone();
    let app_for_tool = app.clone();
    let client_id_tool = args.client_msg_id.clone();

    let started_at = std::time::Instant::now();
    let first_token_seen = Arc::new(parking_lot::Mutex::new(None::<u64>));
    let first_token_clone = first_token_seen.clone();

    let handlers = PipelineHandlers {
        on_token: Arc::new(move |t| {
            if first_token_clone.lock().is_none() {
                *first_token_clone.lock() = Some(started_at.elapsed().as_millis() as u64);
            }
            let _ = app_for_token.emit(
                "kinai://token",
                serde_json::json!({"client_msg_id": client_id_token, "delta": t}),
            );
        }),
        on_reasoning: Arc::new(move |r| {
            let _ = app_for_reasoning.emit(
                "kinai://reasoning",
                serde_json::json!({"client_msg_id": client_id_reasoning, "delta": r}),
            );
        }),
        on_tool: Arc::new(move |event: ToolEvent| {
            let _ = app_for_tool.emit(
                "kinai://tool",
                serde_json::json!({"client_msg_id": client_id_tool, "event": event}),
            );
        }),
    };

    let route = crate::vision::decide(&cfg.llm.model, &args.attachments, &cfg.vision)
        .map_err(err)?;
    let result = crate::vision::run_with_route(
        route,
        llm,
        &cfg.llm,
        messages,
        tool_defs,
        tool_runtime,
        max_tokens,
        handlers,
        cancel,
    )
    .await
    .map_err(err)?;
    let total_ms = started_at.elapsed().as_millis() as u64;
    let first_token_ms = first_token_seen.lock().unwrap_or(0);
    let output_tokens = crate::context::token_guard::count_tokens(&result.final_content) as u64;
    let gen_ms = total_ms.saturating_sub(first_token_ms);
    // Suppress TPS when generation duration is too short to be meaningful
    // (e.g. only the empty-answer diagnostic was emitted after a hung
    // tool loop — first_token_ms == total_ms gives nonsense values like
    // 41000 tok/s). Sub-200 ms generation phases are reported as 0.
    let tps = if gen_ms < 200 || output_tokens == 0 {
        0.0
    } else {
        (output_tokens as f64) * 1000.0 / (gen_ms as f64)
    };
    state.stats.write().last_first_token_ms = Some(first_token_ms);

    let metrics = TurnMetrics {
        first_token_ms,
        total_ms,
        output_tokens,
        tps,
    };

    let mut assistant_msg = state
        .db
        .append_message(
            &args.thread_id,
            "assistant",
            "KinAI",
            &result.final_content,
            &[],
        )
        .await
        .map_err(err)?;
    let metrics_json = serde_json::to_value(&metrics).unwrap_or(serde_json::Value::Null);
    let _ = state
        .db
        .set_message_metrics(&assistant_msg.id, &metrics_json)
        .await;
    assistant_msg.metrics = Some(metrics_json.clone());
    let _ = app.emit("kinai://message", &assistant_msg);

    if let Some(p) = prompt_debug {
        let _ = app.emit(
            "kinai://prompt-debug",
            serde_json::json!({
                "assistant_msg_id": assistant_msg.id,
                "prompt": p,
            }),
        );
    }

    if let Err(e) =
        context::memory::maybe_summarize(&state.db, db::HOST_PEER, &args.thread_id).await
    {
        tracing::warn!("summarizer: {e:?}");
    }

    let _ = app.emit(
        "kinai://assistant-done",
        serde_json::json!({
            "client_msg_id": args.client_msg_id,
            "message": assistant_msg,
            "metrics": metrics,
        }),
    );

    // Bidirectional Telegram sync — if this assistant reply landed on
    // the host owner's Telegram thread, mirror it to their Telegram
    // chat. No-op for any non-Telegram thread.
    crate::telegram::echo::maybe_echo_assistant(
        &state,
        db::HOST_PEER,
        &args.thread_id,
        &assistant_msg.content,
    )
    .await;

    Ok(SendMessageResult {
        user_message: user_msg,
        assistant_message: assistant_msg,
        metrics,
    })
}

#[tauri::command]
pub async fn stop_generation(_state: tauri::State<'_, SharedState>) -> Result<()> {
    // For MVP: simplified — cancellation tokens belong to in-flight tasks
    // and the host UI just stops listening. v1.0 ties this into peer-tagged
    // cancellation.
    Ok(())
}

// ---- Host ----

#[tauri::command]
pub async fn start_host(
    state: tauri::State<'_, SharedState>,
    app: AppHandle,
) -> Result<()> {
    let s: SharedState = (*state).clone();
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = network::server::start(s, h).await {
            tracing::error!("host server failed: {e:?}");
        }
    });
    Ok(())
}

#[tauri::command]
pub async fn stop_host(state: tauri::State<'_, SharedState>) -> Result<()> {
    let s: SharedState = (*state).clone();
    network::server::stop(s).await.map_err(err)
}

// ---- Client ----

#[derive(Debug, Deserialize)]
pub struct ConnectArgs {
    pub host_url: String,
    pub token: String,
    pub display_name: Option<String>,
    pub label: Option<String>,
}

#[tauri::command]
pub async fn connect_client(
    state: tauri::State<'_, SharedState>,
    app: AppHandle,
    args: ConnectArgs,
) -> Result<()> {
    {
        let mut cfg = state.config.write();
        cfg.client.host_url = Some(args.host_url.clone());
        cfg.client.host_token = Some(args.token.clone());
        cfg.client.host_label = args.label.clone();
        if let Some(name) = args.display_name.clone() {
            cfg.client.display_name = name;
        }
        cfg.mode = Mode::Client;
        cfg.save().map_err(err)?;
    }

    // Stop any pre-existing client task before spawning a new one — without
    // this, reconnect attempts race and both install themselves into
    // `state.net.client_tx`, causing outbound messages to land on whichever
    // task got there last.
    {
        let mut net = state.net.lock().await;
        if let Some(handle) = net.client.take() {
            handle.abort();
        }
        net.client_tx = None;
    }

    let s: SharedState = (*state).clone();
    let h = app.clone();
    let handle = tokio::spawn(async move {
        network::client::supervise(s, h, args.host_url, args.token).await;
    });
    state.net.lock().await.client = Some(handle);
    Ok(())
}

/// Wake the client supervisor's backoff sleeper for an immediate retry.
/// If no supervisor is running (e.g. user disconnected and forgot the
/// host, then changed their mind), spawn one using the saved credentials.
#[tauri::command]
pub async fn reconnect_client(
    state: tauri::State<'_, SharedState>,
    app: AppHandle,
) -> Result<()> {
    let has_creds = {
        let cfg = state.config.read();
        cfg.client.host_url.is_some() && cfg.client.host_token.is_some()
    };
    if !has_creds {
        return Err(
            "No saved host. Open the Client page and enter an invite code first.".into(),
        );
    }
    // If a supervisor is already running, wake it for an immediate retry
    // — otherwise we'd be racing the next backoff tick.
    state.net.lock().await.client_wake.notify_waiters();

    // If somehow there's no supervisor running (task crashed, etc.),
    // spawn one. Idempotent: the supervisor itself bails if a duplicate
    // happens to overlap, because each instance reloads credentials each
    // iteration and `connect` exits cleanly when the WS dies.
    let supervisor_alive = {
        let net = state.net.lock().await;
        net.client.as_ref().map(|h| !h.is_finished()).unwrap_or(false)
    };
    if !supervisor_alive {
        let s: SharedState = (*state).clone();
        let h = app.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = network::client::auto_connect(s, h).await {
                tracing::warn!("reconnect_client: {e:?}");
            }
        });
        state.net.lock().await.client = Some(handle);
    }
    Ok(())
}

#[tauri::command]
pub async fn disconnect_client(state: tauri::State<'_, SharedState>) -> Result<()> {
    let s: SharedState = (*state).clone();
    network::client::disconnect(s).await.map_err(err)
}

/// Aggressive variant: stop the WebSocket, clear the saved host URL +
/// token, and drop the mode back to Unconfigured. Used when an invite has
/// been revoked or the user wants to start over with a different host —
/// without this, auto-connect on the next launch keeps trying the dead
/// credentials.
#[tauri::command]
pub async fn disconnect_and_forget(state: tauri::State<'_, SharedState>) -> Result<()> {
    let s: SharedState = (*state).clone();
    let _ = network::client::disconnect(s).await;
    {
        let mut cfg = state.config.write();
        cfg.client.host_url = None;
        cfg.client.host_token = None;
        cfg.client.host_label = None;
        cfg.mode = Mode::Unconfigured;
        cfg.save().map_err(err)?;
    }
    Ok(())
}

// ---- Invites & peers ----

#[derive(Debug, Deserialize)]
pub struct GenerateInviteArgs {
    pub label: String,
    #[serde(default = "default_ttl_days")]
    pub ttl_days: i64,
}

fn default_ttl_days() -> i64 {
    30
}

#[tauri::command]
pub async fn generate_invite(
    state: tauri::State<'_, SharedState>,
    args: GenerateInviteArgs,
) -> Result<invite::Invite> {
    let cfg = state.config.read().clone();
    invite::create(&state.db.pool, &cfg, &args.label, args.ttl_days)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn list_invites(state: tauri::State<'_, SharedState>) -> Result<Vec<invite::Invite>> {
    invite::list(&state.db.pool).await.map_err(err)
}

#[tauri::command]
pub async fn revoke_invite(
    state: tauri::State<'_, SharedState>,
    invite_id: String,
) -> Result<()> {
    invite::revoke(&state.db.pool, &invite_id).await.map_err(err)
}

#[tauri::command]
pub async fn consume_invite(code: String) -> Result<invite::ResolvedInvite> {
    invite::parse_join_url(&code).map_err(err)
}

/// Redeem a 6-character invite code against a specific host's HTTP API.
///
/// `host_url` is whatever the discovery layer or the user provided — we
/// accept either `ws://1.2.3.4:8080/kin` (mDNS form) or a plain
/// `http://1.2.3.4:8080` and normalize it before issuing the GET.
#[tauri::command]
pub async fn redeem_invite_code(
    host_url: String,
    code: String,
) -> Result<invite::ResolvedInvite> {
    let trimmed_code = code.trim().to_lowercase();
    if trimmed_code.len() != 6 {
        return Err("Invite code must be exactly 6 characters".into());
    }
    let base = host_to_http_base(&host_url)
        .ok_or_else(|| "Couldn't understand host URL".to_string())?;
    let url = format!("{base}/v1/invite/redeem?code={trimmed_code}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(err)?;
    let resp = client.get(&url).send().await.map_err(|e| {
        format!("Couldn't reach KinAI host at {base}: {e}")
    })?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Host rejected the code ({status}): {body}"));
    }
    #[derive(serde::Deserialize)]
    struct Wire {
        host_url: String,
        token: String,
        label: String,
    }
    let parsed: Wire = resp.json().await.map_err(err)?;
    Ok(invite::ResolvedInvite {
        host_url: parsed.host_url,
        token: parsed.token,
        label: parsed.label,
    })
}

/// Convert any mDNS / config form of a host URL into the HTTP origin that
/// serves the REST API. Accepts `ws://host:port/path`, `wss://...`,
/// `http://...`, `https://...`, or bare `host:port`.
fn host_to_http_base(input: &str) -> Option<String> {
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
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

#[tauri::command]
pub async fn list_peers(state: tauri::State<'_, SharedState>) -> Result<Vec<PeerSummary>> {
    Ok(network::server::list_peers(&*state).await)
}

#[tauri::command]
pub async fn revoke_peer(
    state: tauri::State<'_, SharedState>,
    peer_id: String,
) -> Result<()> {
    network::server::revoke_peer(&*state, &peer_id).await.map_err(err)
}

// ---- Overlay ----

#[tauri::command]
pub async fn toggle_overlay(app: AppHandle) -> Result<()> {
    if let Some(w) = app.get_webview_window("overlay") {
        let visible = w.is_visible().unwrap_or(false);
        if visible {
            let _ = w.hide();
        } else {
            let _ = w.show();
            let _ = w.set_focus();
            let _ = app.emit("kinai://overlay-focus", ());
        }
    }
    Ok(())
}

// ---- Tools ----

#[derive(Debug, Serialize)]
pub struct ToolListEntry {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

#[tauri::command]
pub async fn list_tools(state: tauri::State<'_, SharedState>) -> Result<Vec<ToolListEntry>> {
    let cfg = state.config.read().clone();
    Ok(registry::enabled(&cfg.tools)
        .into_iter()
        .map(|t| ToolListEntry {
            name: t.name,
            description: t.description,
            schema: t.schema,
        })
        .collect())
}

#[derive(Debug, Deserialize)]
pub struct TestToolArgs {
    pub name: String,
    pub args_json: String,
}

#[tauri::command]
pub async fn test_tool(
    state: tauri::State<'_, SharedState>,
    args: TestToolArgs,
) -> Result<String> {
    let runtime = {
        let cfg = state.config.read();
        tools::registry::ToolRuntime::from_tool_settings(&cfg.tools)
    };
    tools::registry::execute(&args.name, &args.args_json, &runtime)
        .await
        .map_err(err)
}

// ---- Stats / Updates ----

#[tauri::command]
pub async fn runtime_stats(state: tauri::State<'_, SharedState>) -> Result<crate::RuntimeStats> {
    Ok(state.stats.read().clone())
}

#[tauri::command]
pub async fn check_updates(app: AppHandle) -> Result<()> {
    updater::check_once(&app).await;
    Ok(())
}

/// Frontend "Install update" button → trigger the host-or-GitHub
/// download + signature verify + atomic install flow. Restarts on
/// success.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<()> {
    updater::download_and_install(app).await
}
