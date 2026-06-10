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

/// Persist the secondary "deep" LLM slot. Same shape as
/// `set_llm_settings`, just targets `cfg.llm_deep`. Doesn't touch the
/// state.llm client — message routing re-instantiates the client per
/// turn from the chosen slot's settings, so a config change here
/// shows up immediately on the next chat turn.
#[tauri::command]
pub async fn set_llm_deep_settings(
    state: tauri::State<'_, SharedState>,
    llm: LlmSettings,
) -> Result<AppConfig> {
    let new_cfg = {
        let mut cfg = state.config.write();
        cfg.llm_deep = llm;
        cfg.save().map_err(err)?;
        cfg.clone()
    };
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

/// Persist the "when autostarted, should the window open or stay
/// hidden" preference. Only consulted on launches that arrive with
/// `--autostart` in argv (i.e. the OS auto-launched us); manual
/// double-clicks always show the window regardless.
#[tauri::command]
pub async fn set_startup_settings(
    state: tauri::State<'_, SharedState>,
    startup: crate::config::StartupSettings,
) -> Result<AppConfig> {
    let new_cfg = {
        let mut cfg = state.config.write();
        cfg.startup = startup;
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
        enabled: true,
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
// Voice replies (TTS)
// ============================================================

#[tauri::command]
pub async fn set_tts_config(
    state: tauri::State<'_, SharedState>,
    tts: crate::config::TtsConfig,
) -> Result<AppConfig> {
    let new_cfg = {
        let mut cfg = state.config.write();
        cfg.tts = tts;
        cfg.save().map_err(err)?;
        cfg.clone()
    };
    Ok(new_cfg)
}

/// Installed system voices for the Settings dropdowns. Empty on
/// non-macOS / client machines — the UI hides the card then.
#[tauri::command]
pub async fn list_tts_voices() -> Result<Vec<crate::tts::TtsVoice>> {
    crate::tts::list_voices().await.map_err(err)
}

#[derive(Debug, Deserialize)]
pub struct PreviewVoiceArgs {
    pub voice: String,
    /// "de" → German sample sentence, anything else → English.
    pub lang: String,
}

/// Play a short sample sentence in `voice` on the host's speakers so
/// the user can audition voices from Settings. Replaces any playback
/// already running.
#[tauri::command]
pub async fn preview_tts_voice(
    state: tauri::State<'_, SharedState>,
    args: PreviewVoiceArgs,
) -> Result<()> {
    let sample = if args.lang == "de" {
        "Hallo! Ich bin KinAI. Morgen wird es sonnig bei zweiundzwanzig Grad."
    } else {
        "Hi! I'm KinAI. Tomorrow looks sunny, twenty two degrees."
    };
    start_playback(&state, sample, &args.voice).await
}

#[derive(Debug, Deserialize)]
pub struct SpeakTextArgs {
    pub text: String,
}

/// The desktop speak button: read a reply aloud on the host. Markdown
/// is stripped server-side; the voice follows the configured
/// per-language choice. Host-only — clients run on other machines, so
/// playing on the host's speakers would be nonsense there.
#[tauri::command]
pub async fn speak_text(
    state: tauri::State<'_, SharedState>,
    args: SpeakTextArgs,
) -> Result<()> {
    if matches!(state.config.read().mode, Mode::Client) {
        return Err("Voice playback runs on the host only.".into());
    }
    let tts_cfg = state.config.read().tts.clone();
    let text = crate::tts::speakable_text(&args.text);
    if text.is_empty() {
        return Err("Nothing speakable in this message.".into());
    }
    let voice = crate::tts::voice_for_text(&tts_cfg, &text);
    start_playback(&state, &text, &voice).await
}

/// Stop any in-progress desktop playback. Safe to call when idle.
#[tauri::command]
pub async fn stop_speaking(state: tauri::State<'_, SharedState>) -> Result<()> {
    if let Some(mut child) = state.tts_child.lock().take() {
        let _ = child.start_kill();
    }
    Ok(())
}

/// Whether desktop playback is still running — the frontend polls this
/// to flip the speak button back from ⏹ to ▶ when the voice finishes.
#[tauri::command]
pub async fn is_speaking(state: tauri::State<'_, SharedState>) -> Result<bool> {
    let mut guard = state.tts_child.lock();
    match guard.as_mut() {
        None => Ok(false),
        Some(child) => match child.try_wait() {
            Ok(Some(_)) => {
                *guard = None; // finished — clear the slot
                Ok(false)
            }
            Ok(None) => Ok(true),
            Err(_) => {
                *guard = None;
                Ok(false)
            }
        },
    }
}

/// Kill any current playback, then start a new one and park the child
/// in the shared slot.
async fn start_playback(state: &SharedState, text: &str, voice: &str) -> Result<()> {
    if let Some(mut old) = state.tts_child.lock().take() {
        let _ = old.start_kill();
    }
    let child = crate::tts::speak_live(text, voice).await.map_err(err)?;
    *state.tts_child.lock() = Some(child);
    Ok(())
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

/// Cross-thread full-text search over the current peer's messages.
/// Host-only for now: a client has no authoritative local message DB
/// (its chat is a WS pass-through), so it returns empty rather than
/// searching a partial local store — the UI gates the search box to host
/// mode. The client WS round-trip (Envelope::SearchMessages) is a planned
/// follow-up, mirroring list_threads.
#[tauri::command]
pub async fn search_messages(
    state: tauri::State<'_, SharedState>,
    query: String,
) -> Result<Vec<db::SearchHit>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    if matches!(state.config.read().mode, Mode::Client) {
        return Ok(Vec::new());
    }
    state
        .db
        .search_messages(db::HOST_PEER, &query, 50)
        .await
        .map_err(err)
}

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

// ---- Persistent memory (user facts) ----
//
// User-facing surface for the long-term per-peer memory layer. The
// Settings → Memory page reads via list_user_facts, mutates via
// save/delete/clear.
//
// Mode-aware routing:
//   * HOST mode → query the local DB scoped to HOST_PEER (the host
//     owner's own memory).
//   * CLIENT mode → request the same operations from the host over
//     the WebSocket; host scopes by the connecting peer's id so each
//     family member sees ONLY their own facts. Privacy invariant is
//     enforced on the host side, not the client (a malicious client
//     can't ask for another peer's facts because the host doesn't
//     accept a peer_id from the wire — it derives one from the JWT).
//
// Why route through WS instead of duplicating the data to the client's
// local DB: the host is the single source of truth — the LLM that
// produces the facts runs there, and there's no sync-conflict surface.
// Client storage would just be a stale cache.

/// Run a user-facts request through the WS client connection and wait
/// for the host's `UserFacts` response. Used by all four mode-aware
/// commands below. 10s timeout matches load_thread; the operation is
/// a single DB read on the host so it should complete in milliseconds.
async fn client_user_facts_request(
    state: &tauri::State<'_, SharedState>,
    envelope: crate::network::protocol::Envelope,
) -> Result<Vec<db::UserFact>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let tx = {
        let mut net = state.net.lock().await;
        // Defensive: if a previous request hasn't completed yet, fail
        // fast with a clear message instead of silently overwriting
        // its parked sender. Silent overwrite caused the v0.2.42
        // "deleting on a client makes the screen flicker with
        // 'timed out waiting for host's user_facts response'" bug:
        // a stray `kinai://user-facts-updated` emit fired a second
        // load() while the first was in flight, displacing it.
        // We've also removed that emit; this guard belt-and-braces
        // any future rapid-fire race.
        if net.user_facts_pending.is_some() {
            return Err("another memory request is already in flight; try again in a moment".to_string());
        }
        net.user_facts_pending = Some(sender);
        net.client_tx.clone()
    };
    let Some(tx) = tx else {
        state.net.lock().await.user_facts_pending = None;
        return Err("not connected to a host".to_string());
    };
    if tx.send(envelope).is_err() {
        state.net.lock().await.user_facts_pending = None;
        return Err("WS send channel closed".to_string());
    }
    match tokio::time::timeout(std::time::Duration::from_secs(10), receiver).await {
        Ok(Ok(facts)) => Ok(facts),
        _ => {
            state.net.lock().await.user_facts_pending = None;
            Err("timed out waiting for host's user_facts response".to_string())
        }
    }
}

#[tauri::command]
pub async fn list_user_facts(
    state: tauri::State<'_, SharedState>,
) -> Result<Vec<db::UserFact>> {
    if matches!(state.config.read().mode, Mode::Client) {
        return client_user_facts_request(
            &state,
            crate::network::protocol::Envelope::ListUserFacts,
        )
        .await;
    }
    state
        .db
        .list_user_facts(db::HOST_PEER)
        .await
        .map_err(err)
}

#[derive(Debug, Deserialize)]
pub struct SaveUserFactArgs {
    pub key: String,
    pub value: String,
}

#[tauri::command]
pub async fn save_user_fact(
    state: tauri::State<'_, SharedState>,
    args: SaveUserFactArgs,
) -> Result<db::UserFact> {
    if matches!(state.config.read().mode, Mode::Client) {
        let facts = client_user_facts_request(
            &state,
            crate::network::protocol::Envelope::SaveUserFact {
                key: args.key.clone(),
                value: args.value.clone(),
            },
        )
        .await?;
        // The host's response is the full updated list. Pick the row
        // we just saved so the call's return type stays UserFact.
        // Match by key (peer scope is implicit on the host side).
        return facts
            .into_iter()
            .find(|f| f.key == args.key.trim())
            .ok_or_else(|| "host accepted save but the new fact isn't in the response list".to_string());
    }
    state
        .db
        .save_user_fact(db::HOST_PEER, &args.key, &args.value, "manual", None)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn delete_user_fact(
    state: tauri::State<'_, SharedState>,
    id: String,
) -> Result<()> {
    if matches!(state.config.read().mode, Mode::Client) {
        let _ = client_user_facts_request(
            &state,
            crate::network::protocol::Envelope::DeleteUserFact { id },
        )
        .await?;
        return Ok(());
    }
    state
        .db
        .delete_user_fact(db::HOST_PEER, &id)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn clear_user_facts(
    state: tauri::State<'_, SharedState>,
) -> Result<u64> {
    if matches!(state.config.read().mode, Mode::Client) {
        // After clear, the host responds with the (now empty) list.
        // Return the count we just wiped, not Vec.len() which is 0.
        // The pre-clear count would have required two round-trips; not
        // worth it for what's just a UI-toast number. Return 0 — the
        // UI then re-renders an empty list which is the truth.
        let _ = client_user_facts_request(
            &state,
            crate::network::protocol::Envelope::ClearUserFacts,
        )
        .await?;
        return Ok(0);
    }
    state.db.clear_user_facts(db::HOST_PEER).await.map_err(err)
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
    /// LLM model id that produced this reply. Empty for slash commands
    /// (no model involved). See `TurnMetricsWire` for the WS twin.
    #[serde(default)]
    pub model: String,
    /// "fast", "deep", or "" — kept separate so the UI can render a
    /// small slot indicator alongside the (long-ish) model name.
    #[serde(default)]
    pub slot: String,
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
                model: String::new(),
                slot: String::new(),
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

    run_assistant_turn(
        &app,
        &state,
        &args.thread_id,
        user_msg,
        &args.client_msg_id,
        &sender,
        false,
    )
    .await
}

/// The LLM half of a chat turn, factored out of `send_message` so
/// `regenerate_message` / `edit_and_resend` can re-run it against an
/// existing (or edited) user message without duplicating the streaming /
/// metrics / Telegram-echo plumbing. The user row is assumed to already be
/// persisted — this fn never appends it. `suppress_echo` skips the Telegram
/// typing indicator + Q&A mirror so a regenerate/edit doesn't bounce a
/// duplicate message back into a Telegram-linked thread.
#[allow(clippy::too_many_arguments)]
async fn run_assistant_turn(
    app: &AppHandle,
    state: &tauri::State<'_, SharedState>,
    thread_id: &str,
    user_msg: db::Message,
    client_msg_id: &str,
    sender: &str,
    suppress_echo: bool,
) -> Result<SendMessageResult> {
    // Bidirectional Telegram sync (host side): if the host owner is
    // typing inside their own Telegram thread on KinAI, show the
    // "typing…" indicator on Telegram while the LLM thinks. The full
    // Q&A echo (combined into one bot message) happens after the LLM
    // finishes — see `maybe_echo_qa` calls below.
    if !suppress_echo {
        crate::telegram::echo::maybe_show_typing(state, db::HOST_PEER, thread_id).await;
    }

    let cfg = state.config.read().clone();

    // Resolve which LLM slot ("fast" vs "deep") this turn targets +
    // strip the `/fast `/`/deep ` prefix. The persisted user message
    // keeps the original text (so the slash shows in chat history);
    // everything from here on uses the stripped content so neither
    // the in-message slash handler (/pic, /help) nor the LLM context
    // builder sees the routing token. `route_for` is now async +
    // thread-aware so /deep sticks across follow-up turns until the
    // user switches back with /fast.
    let route_pick =
        crate::slash::route_for(&state.db, &cfg, db::HOST_PEER, thread_id, &user_msg.content)
            .await;
    let llm_route_content = route_pick.stripped_content.clone();

    // Bare `/fast` / `/deep` (no prompt) → a pure mode switch. Reply with
    // a confirmation and skip the LLM, instead of sending an empty user
    // turn to the model (which produced junk/nothing). Falls through to
    // the normal slash handler (/pic, /picHQ, /help, ?) otherwise — same
    // handler the WebSocket dispatcher uses, so both chat paths match.
    let slash_reply = if route_pick.bare_switch {
        Some(crate::slash::switch_confirmation(&route_pick))
    } else {
        crate::slash::handle(&cfg, &llm_route_content).await
    };
    if let Some(reply) = slash_reply {
        let started_at = std::time::Instant::now();
        let mut assistant_msg = state
            .db
            .append_message(thread_id, "assistant", "KinAI", &reply, &[])
            .await
            .map_err(err)?;
        let total_ms = started_at.elapsed().as_millis() as u64;
        // Slash commands don't go through the LLM — leave model/slot
        // empty so the UI just hides the model badge instead of
        // pretending some model produced the reply.
        let metrics = TurnMetrics {
            first_token_ms: 0,
            total_ms,
            output_tokens: 0,
            tps: 0.0,
            model: String::new(),
            slot: String::new(),
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
                "client_msg_id": client_msg_id,
                "message": &assistant_msg,
                "metrics": &metrics,
            }),
        );
        // Bidirectional Telegram sync: mirror the full Q&A turn (the
        // user's slash-command input + the resolved reply) to Telegram
        // as one combined bot message. No-op on non-Telegram threads.
        if !suppress_echo {
            crate::telegram::echo::maybe_echo_qa(
                state,
                db::HOST_PEER,
                thread_id,
                sender,
                &user_msg.content,
                &assistant_msg.content,
            )
            .await;
        }
        return Ok(SendMessageResult {
            user_message: user_msg,
            assistant_message: assistant_msg,
            metrics,
        });
    }

    // Build prompt with the slash-stripped user content — DB still
    // has the original for chat history, but the model shouldn't see
    // its own routing prefix in the prompt.
    let user_msg_for_llm = db::Message {
        content: llm_route_content.clone(),
        ..user_msg.clone()
    };
    let messages =
        context::builder::build_context(&state.db, &cfg, db::HOST_PEER, thread_id, &user_msg_for_llm)
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
    let tool_runtime = registry::ToolRuntime::from_tool_settings(&cfg.tools)
        .with_memory(state.db.clone(), db::HOST_PEER)
        .with_source_msg(client_msg_id.to_string());

    let max_tokens = compute_max_tokens(&cfg, &messages);

    // Pick the LLM client from the routed slot (fast vs deep) rather
    // than the cached state.llm (which is always the fast slot).
    let active_llm_settings = route_pick.settings.clone();
    let llm = crate::llm::LlmClient::new(active_llm_settings.clone());
    let cancel = CancellationToken::new();
    // Register the token under the frontend-supplied client_msg_id so
    // stop_generation can cancel THIS turn specifically (not whatever
    // happens to be running). Deregistered in the guard below regardless
    // of how the turn ends — success, error, or panic.
    state
        .pending_turns
        .lock()
        .insert(client_msg_id.to_string(), cancel.clone());
    // Drop-guard removes the token when this function returns, so a
    // panic mid-pipeline doesn't leak entries that would later get
    // cancelled by accident on a future turn with the same id.
    struct TurnGuard {
        id: String,
        pending: Arc<parking_lot::Mutex<std::collections::HashMap<String, CancellationToken>>>,
    }
    impl Drop for TurnGuard {
        fn drop(&mut self) {
            self.pending.lock().remove(&self.id);
        }
    }
    let _turn_guard = TurnGuard {
        id: client_msg_id.to_string(),
        pending: state.pending_turns.clone(),
    };

    let app_for_token = app.clone();
    let client_id_token = client_msg_id.to_string();
    let app_for_reasoning = app.clone();
    let client_id_reasoning = client_msg_id.to_string();
    let app_for_tool = app.clone();
    let client_id_tool = client_msg_id.to_string();

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

    let route = crate::vision::decide(&active_llm_settings.model, &user_msg.attachments, &cfg.vision)
        .map_err(err)?;
    let result = crate::vision::run_with_route(
        route,
        llm,
        &active_llm_settings,
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
        model: active_llm_settings.model.clone(),
        slot: route_pick.slot_label.to_string(),
    };

    let mut assistant_msg = state
        .db
        .append_message(
            thread_id,
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
        context::memory::maybe_summarize(&state.db, db::HOST_PEER, thread_id).await
    {
        tracing::warn!("summarizer: {e:?}");
    }

    // Passive fact extraction — fire-and-forget so a slow extractor
    // call can never delay the user's reply (which has already been
    // emitted at this point). Runs the fast slot regardless of which
    // slot served this turn; the extractor's job is cheap structured
    // output and the fast model is reliably good at JSON.
    let extractor_db = state.db.clone();
    let extractor_settings = cfg.llm.clone();
    let extractor_msg_id = user_msg.id.clone();
    let extractor_content = user_msg.content.clone();
    let extractor_app = app.clone();
    tokio::spawn(async move {
        match context::extractor::extract_and_save(
            &extractor_db,
            &extractor_settings,
            db::HOST_PEER,
            &extractor_msg_id,
            &extractor_content,
        )
        .await
        {
            Ok(n) if n > 0 => {
                tracing::info!("extractor stored {n} new fact(s)");
                // Nudge the Settings page so the new chips appear without
                // a manual refresh.
                let _ = extractor_app.emit("kinai://user-facts-updated", serde_json::json!({}));
            }
            Ok(_) => { /* nothing fact-shaped; quiet */ }
            Err(e) => tracing::warn!("extractor: {e:?}"),
        }
    });

    let _ = app.emit(
        "kinai://assistant-done",
        serde_json::json!({
            "client_msg_id": client_msg_id,
            "message": assistant_msg,
            "metrics": metrics,
        }),
    );

    // Bidirectional Telegram sync — mirror the full Q&A turn to
    // Telegram as one combined bot message. No-op on non-Telegram
    // threads; `maybe_echo_qa` also skips the question quote when
    // sender == "Telegram" so we don't bounce inbound messages.
    if !suppress_echo {
        crate::telegram::echo::maybe_echo_qa(
            state,
            db::HOST_PEER,
            thread_id,
            sender,
            &user_msg.content,
            &assistant_msg.content,
        )
        .await;
    }

    Ok(SendMessageResult {
        user_message: user_msg,
        assistant_message: assistant_msg,
        metrics,
    })
}

#[derive(Debug, Deserialize)]
pub struct RegenerateArgs {
    pub thread_id: String,
    /// id of the ASSISTANT message to re-run.
    pub assistant_msg_id: String,
    /// Fresh uuid from the frontend; drives the streaming events for the
    /// new reply (same role as `client_msg_id` in `send_message`).
    pub client_msg_id: String,
}

/// Re-run the assistant reply for the user turn that produced
/// `assistant_msg_id`. Deletes that assistant message (and anything after
/// it — the cut is inclusive for safety; the UI only offers regenerate on
/// the last reply, so normally there's nothing after), then re-runs the
/// pipeline against the unchanged user turn. Host-only for now.
#[tauri::command]
pub async fn regenerate_message(
    app: AppHandle,
    state: tauri::State<'_, SharedState>,
    args: RegenerateArgs,
) -> Result<SendMessageResult> {
    if matches!(state.config.read().mode, Mode::Client) {
        return Err("Regenerate isn't available in client mode yet.".into());
    }
    let assistant = state
        .db
        .get_message(db::HOST_PEER, &args.assistant_msg_id)
        .await
        .map_err(err)?
        .ok_or_else(|| "message not found".to_string())?;
    if assistant.role != "assistant" {
        return Err("can only regenerate an assistant reply".into());
    }
    // The user message that produced it = the newest user message in the
    // thread strictly older than the assistant reply. `load_messages`
    // returns oldest-first, so `.last()` of the filtered set is the newest.
    let history = state
        .db
        .load_messages(db::HOST_PEER, &args.thread_id, 500)
        .await
        .map_err(err)?;
    let user_msg = history
        .iter()
        .filter(|m| m.role == "user" && m.created_at < assistant.created_at)
        .next_back()
        .cloned()
        .ok_or_else(|| "no user message to regenerate from".to_string())?;
    // Drop the old assistant reply (inclusive cut at its created_at).
    state
        .db
        .delete_messages_from(db::HOST_PEER, &args.thread_id, &assistant.created_at)
        .await
        .map_err(err)?;
    let sender = user_msg.sender.clone();
    // suppress_echo = true: a desktop-initiated regenerate of a
    // Telegram-linked thread shouldn't bounce a duplicate Q&A to Telegram.
    run_assistant_turn(
        &app,
        &state,
        &args.thread_id,
        user_msg,
        &args.client_msg_id,
        &sender,
        true,
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct EditResendArgs {
    pub thread_id: String,
    /// id of the user's own message being edited.
    pub user_msg_id: String,
    pub new_content: String,
    /// Fresh uuid from the frontend; drives the streaming events.
    pub client_msg_id: String,
}

/// Edit a previous user message in place (same id + created_at so ordering
/// is stable), delete everything strictly after it, then re-run the
/// pipeline against the edited turn. Host-only for now.
#[tauri::command]
pub async fn edit_and_resend(
    app: AppHandle,
    state: tauri::State<'_, SharedState>,
    args: EditResendArgs,
) -> Result<SendMessageResult> {
    if matches!(state.config.read().mode, Mode::Client) {
        return Err("Edit & resend isn't available in client mode yet.".into());
    }
    let trimmed = args.new_content.trim().to_string();
    if trimmed.is_empty() {
        return Err("Edited message can't be empty.".into());
    }
    let original = state
        .db
        .get_message(db::HOST_PEER, &args.user_msg_id)
        .await
        .map_err(err)?
        .ok_or_else(|| "message not found".to_string())?;
    if original.role != "user" {
        return Err("can only edit your own messages".into());
    }
    // Update content in place, then delete the first message after it
    // (the old assistant reply) and everything beyond.
    state
        .db
        .update_message(&original.id, &trimmed)
        .await
        .map_err(err)?;
    let history = state
        .db
        .load_messages(db::HOST_PEER, &args.thread_id, 500)
        .await
        .map_err(err)?;
    if let Some(next) = history.iter().find(|m| m.created_at > original.created_at) {
        let cut = next.created_at.clone();
        state
            .db
            .delete_messages_from(db::HOST_PEER, &args.thread_id, &cut)
            .await
            .map_err(err)?;
    }
    let sender = original.sender.clone();
    let edited = db::Message {
        content: trimmed,
        ..original
    };
    run_assistant_turn(
        &app,
        &state,
        &args.thread_id,
        edited,
        &args.client_msg_id,
        &sender,
        true,
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct StopGenerationArgs {
    /// The client-supplied id the frontend used when calling send_message.
    /// When set, only that turn is cancelled; this is the normal case
    /// (the Stop button knows which turn it's looking at). When None,
    /// every in-flight turn is cancelled — useful as a "panic button"
    /// from a context where the id is unavailable.
    #[serde(default)]
    pub client_msg_id: Option<String>,
}

/// Cancel an in-flight chat turn. The token's `.cancel()` propagates
/// through `crate::llm::stream::pump` (which awaits `cancel.cancelled()`
/// in `tokio::select!`) and through `crate::tools::loop_pipeline`
/// (which checks `cancel.is_cancelled()` between iterations), so the
/// LLM HTTP body stops being read and the next tool iteration is
/// skipped. The pending-turns guard in `send_message` removes the
/// entry on completion regardless of how the turn ends, so we
/// silently no-op when the id isn't found (turn already finished).
#[tauri::command]
pub async fn stop_generation(
    state: tauri::State<'_, SharedState>,
    args: Option<StopGenerationArgs>,
) -> Result<()> {
    let target = args.and_then(|a| a.client_msg_id);
    let tokens: Vec<CancellationToken> = {
        let mut map = state.pending_turns.lock();
        match target {
            Some(ref id) => map.remove(id).into_iter().collect(),
            None => map.drain().map(|(_, t)| t).collect(),
        }
    };
    for t in tokens {
        t.cancel();
    }
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
