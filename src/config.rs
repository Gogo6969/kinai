//! Persistent configuration at `~/.kinai/config.toml`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Unconfigured,
    Host,
    Client,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmSettings {
    /// One of: ollama, lmstudio, vllm, llamacpp, openai-compat
    pub provider: String,
    /// Base URL, e.g. http://localhost:11434
    pub base_url: String,
    /// Model id, e.g. llama3.1:8b
    pub model: String,
    /// Context window in tokens
    pub context_window: usize,
    /// API key for OpenAI-compat endpoints
    pub api_key: Option<String>,
    /// Temperature
    pub temperature: f32,
    /// Max output tokens reserved by the token guard
    pub max_tokens: usize,
    /// Friendly system-prompt addendum (the host's preferences)
    pub system_addendum: String,
    /// Pause toggle. When `false` this slot is hidden from slash menus
    /// and skipped during message routing — useful for temporarily
    /// disabling one model without losing its configuration. Defaults
    /// to `true` so existing single-model configs (no `enabled` field
    /// in their TOML) keep behaving the same after the upgrade.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl LlmSettings {
    fn default_inner(base_url: &str, model: &str) -> Self {
        Self {
            provider: "ollama".into(),
            base_url: base_url.into(),
            model: model.into(),
            context_window: 8192,
            api_key: None,
            temperature: 0.7,
            // 0 = auto: use the model's full remaining context after the prompt.
            // Crucial for reasoning models (gpt-oss, R1, QwQ) that need lots of
            // tokens for chain-of-thought before producing visible content.
            max_tokens: 0,
            system_addendum: String::new(),
            enabled: true,
        }
    }

    /// Empty/disabled placeholder for the secondary "deep" slot — until
    /// the user fills in a base_url it isn't a candidate for routing.
    pub fn default_empty() -> Self {
        let mut s = Self::default_inner("", "");
        s.enabled = false;
        s
    }

    /// True iff this slot can serve a chat turn — paused slots and
    /// completely-unconfigured slots both return false. Used by the
    /// slash autocomplete, message router, and /help renderer to keep
    /// their decisions in lockstep.
    pub fn is_active(&self) -> bool {
        self.enabled && !self.base_url.trim().is_empty() && !self.model.trim().is_empty()
    }
}

impl Default for LlmSettings {
    fn default() -> Self {
        Self::default_inner("http://localhost:11434", "llama3.1:8b")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostSettings {
    pub bind_addr: String,
    pub port: u16,
    pub family_name: String,
    /// Advertise via mDNS on the local network
    pub mdns_enabled: bool,
    /// Per-client rate limit (requests / minute)
    pub rate_limit_rpm: u32,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0".into(),
            port: 4847,
            family_name: "Our Family".into(),
            mdns_enabled: true,
            rate_limit_rpm: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientSettings {
    pub host_url: Option<String>,
    pub host_token: Option<String>,
    pub host_label: Option<String>,
    pub display_name: String,
    /// Other hosts the user has joined — for the multi-host switcher (v1.0).
    pub pinned_hosts: Vec<PinnedHost>,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            host_url: None,
            host_token: None,
            host_label: None,
            display_name: default_display_name(),
            pinned_hosts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedHost {
    pub label: String,
    pub host_url: String,
    pub host_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlaySettings {
    pub hotkey: String,
    pub always_on_top: bool,
    pub font_size: u8,
    pub auto_close_on_blur: bool,
}

impl Default for OverlaySettings {
    fn default() -> Self {
        Self {
            #[cfg(target_os = "macos")]
            hotkey: "CmdOrCtrl+Space".into(),
            #[cfg(not(target_os = "macos"))]
            hotkey: "Ctrl+Space".into(),
            always_on_top: true,
            font_size: 14,
            auto_close_on_blur: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SearchEngine {
    #[default]
    Duckduckgo,
    Exa,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSettings {
    pub web_search: bool,
    pub x_search: bool,
    pub calculator: bool,
    pub datetime: bool,
    /// Find images on the web. Same `search_engine` setting as
    /// web_search: DuckDuckGo mode uses Wikimedia Commons (free, no
    /// key); Exa mode reuses the configured Exa key.
    #[serde(default = "default_true")]
    pub image_search: bool,
    /// Which engine `web_search` uses underneath. Default is DuckDuckGo
    /// (free, scrape-based, no API key). Exa is high-signal but requires
    /// an API key from https://exa.ai.
    #[serde(default)]
    pub search_engine: SearchEngine,
    /// API key for the selected search engine, when applicable.
    #[serde(default)]
    pub search_api_key: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for ToolSettings {
    fn default() -> Self {
        Self {
            web_search: true,
            x_search: true,
            calculator: true,
            datetime: true,
            image_search: true,
            search_engine: SearchEngine::default(),
            search_api_key: None,
        }
    }
}

/// One OpenAI-compatible vision endpoint — used either as primary or as
/// failover. The same shape is reused for both slots because every
/// supported provider speaks the OpenAI multipart format (Gemini through
/// `generativelanguage.googleapis.com/v1beta/openai`, Anthropic through
/// their OpenAI-compat shim, vLLM/Ollama llava/qwen-vl natively).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisionEndpoint {
    /// User-facing label ("Gemini Flash", "Local llava"). Helps the
    /// failover error chip read sensibly.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Per-message routing rule for image attachments. Three paths:
///   1. Active chat model is a known vision model → use it (no routing)
///   2. Primary configured + enabled → call it
///   3. On transient failure (5xx / 429 / "high demand") → call failover
///
/// The user disables vision entirely by setting `enabled = false`, in
/// which case image messages return a friendly "no vision endpoint
/// configured" error instead of silently dropping the image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisionSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub primary: VisionEndpoint,
    /// Optional secondary endpoint hit when the primary returns a
    /// transient failure. Set `label` non-empty to opt in; an endpoint
    /// with empty `base_url` is treated as "no failover configured".
    #[serde(default)]
    pub failover: VisionEndpoint,
}

impl Default for VisionSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            primary: VisionEndpoint::default(),
            failover: VisionEndpoint::default(),
        }
    }
}

/// Telegram bot integration — host-side only. The host owner creates
/// ONE bot via @BotFather for the whole family, pastes its token here.
/// Each family member then pairs their personal Telegram from their own
/// device via Settings → Telegram → "Connect Telegram" (a one-time QR
/// scan). Once paired, messages from that Telegram chat route into the
/// member's KinAI thread, slash commands work the same as in KinAI's
/// own chat, and replies are mirrored back to Telegram.
///
/// Empty `bot_token` = feature disabled. No long-poll loop starts, no
/// UI shown to clients.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Bot API token from @BotFather. Empty = feature disabled.
    #[serde(default)]
    pub bot_token: String,
    /// Cached bot username (e.g. "kinai_family_bot"). Populated after
    /// the host validates the token via getMe — used to build the
    /// pairing deep-link `https://t.me/<username>?start=<token>`.
    #[serde(default)]
    pub bot_username: String,
}

/// Image generation via ComfyUI. Empty `base_url` means disabled —
/// no `/pic` or `/picHQ` slash commands will be advertised, and any
/// attempt to invoke them returns a "no image-gen endpoint" error.
///
/// `base_url` is a ComfyUI server reachable from the host machine
/// (e.g. http://192.168.1.25:8188). Same provider CCC chat targets.
/// The host calls /prompt to submit a workflow, polls /history/<id>,
/// fetches the result via /view, saves it to ~/.kinai/pics/, and serves
/// it back to clients through /v1/pic/<filename>.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComfyConfig {
    /// ComfyUI base URL. Empty = feature disabled.
    #[serde(default)]
    pub base_url: String,
    /// Which workflow `/pic` (without HQ) uses. Default "zimage" (Z-Image
    /// Turbo, 8 steps, fast). `/picHQ` always uses "zimagehq".
    #[serde(default)]
    pub default_model: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub mode: Mode,
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub llm: LlmSettings,
    /// Optional "deep" model slot — a slower / higher-quality model the
    /// user can address via `/deep <prompt>`. Defaults to disabled +
    /// empty so brand-new installs and pre-v0.2.25 configs keep their
    /// single-model behaviour. Becomes the default routing target when
    /// `llm` is disabled or unconfigured.
    #[serde(default = "LlmSettings::default_empty")]
    pub llm_deep: LlmSettings,
    #[serde(default)]
    pub host: HostSettings,
    #[serde(default)]
    pub client: ClientSettings,
    #[serde(default)]
    pub overlay: OverlaySettings,
    #[serde(default)]
    pub tools: ToolSettings,
    #[serde(default)]
    pub vision: VisionSettings,
    #[serde(default)]
    pub comfyui: ComfyConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    /// Version string the user last acknowledged in the changelog
    /// modal. The modal opens automatically when this is empty
    /// (first launch) or doesn't match the binary's
    /// `CARGO_PKG_VERSION` (post-upgrade). Dismissing it stamps the
    /// current version back in, so the modal won't re-appear until
    /// the next update.
    #[serde(default)]
    pub last_seen_changelog_version: String,
}

impl AppConfig {
    pub fn config_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".kinai")
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    pub fn db_path(&self) -> PathBuf {
        Self::config_dir().join("kinai.db")
    }

    pub fn keys_dir() -> PathBuf {
        Self::config_dir().join("keys")
    }

    pub fn load_or_default() -> Self {
        let path = Self::config_path();
        let _ = std::fs::create_dir_all(Self::config_dir());
        let _ = std::fs::create_dir_all(Self::keys_dir());
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut cfg: Self = toml::from_str(&text).unwrap_or_else(|e| {
                    tracing::warn!("config parse failed ({e}); using defaults");
                    Self::default()
                });
                // Old configs hard-capped at 1024–4096; switch them to auto so
                // reasoning models get the full remaining context. Users who
                // want a hard cap can set it explicitly in Settings.
                if cfg.llm.max_tokens > 0 && cfg.llm.max_tokens < 2048 {
                    tracing::info!(
                        "switching max_tokens {} → 0 (auto / use remaining context)",
                        cfg.llm.max_tokens
                    );
                    cfg.llm.max_tokens = 0;
                    let _ = cfg.save();
                }
                cfg
            }
            Err(_) => {
                let cfg = Self::default();
                let _ = cfg.save();
                cfg
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path();
        let _ = std::fs::create_dir_all(Self::config_dir());
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }
}

fn default_display_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "Family Member".into())
}
