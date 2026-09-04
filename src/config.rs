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
    /// How this slot handles image attachments:
    /// * `"auto"` (default) — ask a llama.cpp server whether it has a
    ///   multimodal projector loaded (`/props` modalities); any other
    ///   provider falls back to the known vision-model name list. The
    ///   model answers image turns itself when it can see, and the
    ///   external Vision endpoint covers it otherwise.
    /// * `"native"` — always send images to this model.
    /// * `"external"` — always route images to the Vision endpoint,
    ///   the pre-0.2.103 behavior.
    #[serde(default = "default_image_recognition")]
    pub image_recognition: String,
}

fn default_image_recognition() -> String {
    "auto".into()
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
            image_recognition: default_image_recognition(),
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
    /// A SearXNG instance the family runs themselves — metasearch over
    /// whichever upstream engines they enable, reached by URL, no key and
    /// no third party. The one engine here that keeps a family member's
    /// query on their own hardware, which is the promise the rest of the
    /// product makes.
    Searxng,
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
    /// Base URL of the family's own SearXNG, used when `search_engine`
    /// is `Searxng`. Defaults to the usual local install. The instance
    /// must have the JSON output format enabled — SearXNG ships with it
    /// off, and without it every search returns HTML we can't parse.
    #[serde(default = "default_searxng_url")]
    pub searxng_url: String,
    /// Base URL of the family's own transcript service (see
    /// `docs/TRANSCRIPT-SERVICE.md`), e.g. `http://192.168.1.25:8099`.
    /// Empty — the default — disables the `video_transcript` tool
    /// entirely, so a household that hasn't set one up never sees it.
    ///
    /// Host-configured ONLY, exactly like `searxng_url`. That is the
    /// security property: `fetch_page` refuses LAN addresses so a web
    /// page can't steer the model into probing the house, and this URL
    /// is the deliberate exception the OWNER opens — never something a
    /// fetched page can influence.
    #[serde(default)]
    pub transcript_url: String,
    /// Fall back to SearXNG when the paid engine is out of credits (or its
    /// key is rejected) instead of failing the turn.
    ///
    /// On 2026-07-29 Exa hit its credit limit mid-conversation and every
    /// current-information question failed for the whole family until the
    /// account was topped up. A self-hosted instance costs nothing and is
    /// already on the LAN, so there is no reason to go dark. Only permanent
    /// failures trigger it — a timeout or a 5xx is retried on Exa itself.
    #[serde(default = "default_true")]
    pub search_fallback_searxng: bool,
}

fn default_searxng_url() -> String {
    "http://127.0.0.1:8888".into()
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
            searxng_url: default_searxng_url(),
            search_fallback_searxng: true,
            // Off until the household stands up the service and pastes
            // its URL — see docs/TRANSCRIPT-SERVICE.md.
            transcript_url: String::new(),
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
/// (e.g. http://192.168.1.50:8188). Same provider CCC chat targets.
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

/// Voice replies (text-to-speech). Host-only; the engine is the macOS
/// `say` command (see src/tts.rs). `enabled` is the master switch —
/// family members opt in per Telegram chat with /voice, and the host's
/// desktop chat gets a speak button on replies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Voice for English replies. Premium/Enhanced voices sound far
    /// better and are free one-time downloads in System Settings.
    #[serde(default = "default_voice_en")]
    pub voice_en: String,
    /// Voice for German replies (language auto-detected per reply).
    #[serde(default = "default_voice_de")]
    pub voice_de: String,
    /// Desktop (host) chat: speak every finished reply automatically
    /// instead of waiting for the speak button. Telegram is unaffected
    /// (voice notes there are per-member /voice opt-ins).
    #[serde(default)]
    pub auto_speak: bool,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            voice_en: default_voice_en(),
            voice_de: default_voice_de(),
            auto_speak: false,
        }
    }
}

fn default_voice_en() -> String {
    "Zoe (Premium)".into()
}
fn default_voice_de() -> String {
    "Anna (Premium)".into()
}

/// Voice input (speech-to-text) for inbound Telegram voice messages.
/// Host-only; whisper.cpp embedded (see src/stt.rs). Disabled until the
/// host downloads a model in Settings → Voice replies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Which downloaded model to use — an id from stt::MODELS.
    #[serde(default = "default_stt_model")]
    pub model: String,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: default_stt_model(),
        }
    }
}

fn default_stt_model() -> String {
    "small-q5_1".into()
}

/// Per-machine startup preferences. Only consulted when the OS
/// auto-launches KinAI at login (the binary sees `--autostart` in its
/// argv — see `tauri-plugin-autostart` init in lib.rs). Manual
/// launches (double-clicking the app, opening from the dock) ignore
/// these entirely and always show the window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupSettings {
    /// When true (default), an autostart launch keeps the main window
    /// hidden — only the tray icon appears. Matches the typical
    /// "background daemon" UX users expect from a chat app pinned to
    /// boot. Toggle to false to have the window come up immediately.
    #[serde(default = "default_true")]
    pub autostart_minimized: bool,
}

impl Default for StartupSettings {
    fn default() -> Self {
        Self {
            autostart_minimized: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Optional "balanced" model slot — the middle ground between fast
    /// and deep, addressed via `/balanced <prompt>`. Same disabled+empty
    /// default so existing configs are untouched by the upgrade.
    #[serde(default = "LlmSettings::default_empty")]
    pub llm_balanced: LlmSettings,
    /// Optional ONLINE chat slot — a hosted, OpenAI-compatible model
    /// (DeepSeek, OpenAI, Groq, …) the user can address via `/online`.
    /// Disabled + empty by default, so it simply does not exist until
    /// the host owner fills it in.
    ///
    /// Deliberately NOT part of the automatic failover chain
    /// (`slash::FAILOVER_SLOTS`): every other slot is a machine the
    /// family owns, and a local server going down must never silently
    /// reroute a private family conversation to a paid endpoint off the
    /// premises. Reaching it is always an explicit act — `/online`, or
    /// picking it in the composer.
    #[serde(default = "LlmSettings::default_empty")]
    pub llm_online: LlmSettings,
    /// Fourth, ONLINE model used ONLY by the per-message "fact check"
    /// button (never part of /fast /balanced /deep routing or failover).
    /// OpenAI-compatible API — DeepSeek, OpenAI, Groq, etc. The button
    /// appears once credentials (base URL + API key + model) are set.
    #[serde(default = "LlmSettings::default_empty")]
    pub llm_factcheck: LlmSettings,
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
    #[serde(default)]
    pub tts: TtsConfig,
    #[serde(default)]
    pub stt: SttConfig,
    /// Per-machine startup behaviour. Empty by default so existing
    /// configs don't change; only takes effect when "Launch at login"
    /// is enabled in Settings.
    #[serde(default)]
    pub startup: StartupSettings,
    /// Version string the user last acknowledged in the changelog
    /// modal. The modal opens automatically when this is empty
    /// (first launch) or doesn't match the binary's
    /// `CARGO_PKG_VERSION` (post-upgrade). Dismissing it stamps the
    /// current version back in, so the modal won't re-appear until
    /// the next update.
    #[serde(default)]
    pub last_seen_changelog_version: String,
    /// True once the first-run setup wizard was completed OR deliberately
    /// skipped. Gates the splash + wizard: while false and the mode is
    /// still Unconfigured, the app opens into /setup instead of chat.
    /// Existing installs are auto-marked complete on load (see
    /// load_or_default) so nobody who configured KinAI before this flag
    /// existed ever sees the wizard after an update.
    #[serde(default)]
    pub setup_completed: bool,
}

/// Manual Default: the derived one filled `llm_deep`/`llm_balanced` with
/// LlmSettings::default() — i.e. an ACTIVE slot pointing at the same
/// llama3.1:8b as fast — because `#[serde(default = "…default_empty")]`
/// only applies during TOML parsing, not to `AppConfig::default()`. A
/// truly fresh install therefore showed `/deep` (and would have shown
/// `/balanced`) as available before any model was configured there. The
/// extra slots must start empty + disabled.
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            theme: Theme::default(),
            llm: LlmSettings::default(),
            llm_deep: LlmSettings::default_empty(),
            llm_balanced: LlmSettings::default_empty(),
            llm_online: LlmSettings::default_empty(),
            llm_factcheck: LlmSettings::default_empty(),
            host: HostSettings::default(),
            client: ClientSettings::default(),
            overlay: OverlaySettings::default(),
            tools: ToolSettings::default(),
            vision: VisionSettings::default(),
            comfyui: ComfyConfig::default(),
            telegram: TelegramConfig::default(),
            tts: TtsConfig::default(),
            stt: SttConfig::default(),
            startup: StartupSettings::default(),
            last_seen_changelog_version: String::new(),
            setup_completed: false,
        }
    }
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
                // Anyone already running as a host or client configured KinAI
                // before the setup wizard existed — mark setup complete so an
                // update NEVER drops an established family into first-run
                // onboarding.
                if !cfg.setup_completed && cfg.mode != Mode::Unconfigured {
                    cfg.setup_completed = true;
                    let _ = cfg.save();
                }
                cfg
            }
            Err(_) => {
                // Brand-new install. Pre-acknowledge the running version:
                // release notes are for people who ran the PREVIOUS one,
                // and a first-time user was otherwise met with "what's new
                // in 0.2.95" stacked on top of the setup wizard before
                // they had ever used KinAI at all.
                let mut cfg = Self::default();
                cfg.last_seen_changelog_version = crate::changelog::current_version().to_string();
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

#[cfg(test)]
mod setup_flag_tests {
    use super::*;

    /// An existing family's config predates `setup_completed` — it must
    /// deserialize as false (serde default) with the mode intact, so
    /// load_or_default's auto-mark (mode != Unconfigured → completed) can
    /// take over. Guards against an update dropping an established host
    /// into first-run onboarding.
    #[test]
    fn old_config_without_flag_parses_false_and_keeps_mode() {
        let old = r#"
mode = "host"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "llama3.1:8b"
context_window = 8192
temperature = 0.7
max_tokens = 0
system_addendum = ""
"#;
        let cfg: AppConfig = toml::from_str(old).expect("old config must parse");
        assert_eq!(cfg.mode, Mode::Host);
        assert!(!cfg.setup_completed, "missing flag must default to false");
        // The load_or_default migration condition: configured install → mark.
        assert!(cfg.mode != Mode::Unconfigured);
    }

    #[test]
    fn fresh_default_shows_wizard() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.mode, Mode::Unconfigured);
        assert!(!cfg.setup_completed, "fresh install must route to /setup");
    }

    /// A brand-new install must not be greeted with release notes for a
    /// version it has never run. The changelog gate is
    /// `markdown.is_some() && last_seen != version && setup_completed`
    /// (commands::get_changelog_payload) — reproduce it here for each
    /// state, since the command itself needs a Tauri State to call.
    #[test]
    fn changelog_never_opens_before_setup_is_done() {
        let version = crate::changelog::current_version();
        let should_show =
            |last_seen: &str, setup_completed: bool| last_seen != version && setup_completed;

        // Fresh install, wizard not finished: silent, whatever last_seen is.
        assert!(
            !should_show("", false),
            "release notes must never cover the setup wizard"
        );

        // Wizard just finished. `mark_setup_completed` stamps the current
        // version when last_seen is empty, so the modal must NOT fire on
        // the very first session either.
        assert!(
            !should_show(version, true),
            "a first-time user must not get notes for the version they started on"
        );

        // The case the modal exists for: an established family upgrading.
        assert!(
            should_show("0.2.94", true),
            "an existing user upgrading must still see what changed"
        );
    }

    /// The upgrade path must not be collateral damage: an existing config
    /// carries a non-empty `last_seen`, and nothing in the first-run
    /// changes may overwrite it.
    #[test]
    fn upgrade_keeps_its_last_seen_version() {
        let existing = r#"
mode = "host"
last_seen_changelog_version = "0.2.94"
setup_completed = true

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "llama3.1:8b"
context_window = 8192
temperature = 0.7
max_tokens = 0
system_addendum = ""
"#;
        let cfg: AppConfig = toml::from_str(existing).expect("config must parse");
        assert_eq!(cfg.last_seen_changelog_version, "0.2.94");
        assert!(cfg.setup_completed);
        // mark_setup_completed only stamps when the field is EMPTY, so a
        // returning user's pending release notes survive.
        assert!(!cfg.last_seen_changelog_version.is_empty());
    }

    #[test]
    fn flag_roundtrips_through_toml() {
        let mut cfg = AppConfig::default();
        cfg.setup_completed = true;
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&text).unwrap();
        assert!(back.setup_completed);
    }

    #[test]
    fn pre_0_2_103_slot_toml_defaults_to_auto_image_recognition() {
        // Every family config written before the field existed must keep
        // parsing, and "auto" preserves their behavior (probe/name-list).
        let slot: LlmSettings = toml::from_str(
            r#"
            provider = "llamacpp"
            base_url = "http://192.168.1.25:8081"
            model = "Qwen3.8-27B-Q4_K_M"
            context_window = 65536
            temperature = 0.3
            max_tokens = 0
            system_addendum = ""
            "#,
        )
        .unwrap();
        assert_eq!(slot.image_recognition, "auto");
        assert!(slot.enabled);
    }
}
