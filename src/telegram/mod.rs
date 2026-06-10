//! Telegram bot integration — host-side only.
//!
//! The host owner sets a bot token (from @BotFather) in Settings; this
//! module then long-polls the Telegram Bot API, routes incoming
//! messages to paired family members, runs them through the same chat
//! pipeline KinAI uses internally, and mirrors the reply back to
//! Telegram. Bidirectional: messages typed in KinAI by a paired user
//! are also pushed to that user's Telegram chat.
//!
//! Layout:
//!   - mod.rs        — supervisor task + lifecycle (start / stop / restart)
//!   - api.rs        — thin reqwest wrapper around the Bot API endpoints
//!   - polling.rs    — getUpdates loop with offset bookkeeping
//!   - router.rs     — incoming update → chat-pipeline routing

pub mod api;
pub mod echo;
pub mod polling;
pub mod router;

use std::sync::Arc;

use tauri::{AppHandle, Runtime};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::SharedState;

pub use api::BotApi;

/// Supervisor handle stored on `AppState`. Lets `set_telegram_config`
/// stop a stale loop before starting a fresh one, and gives us a place
/// to abort cleanly during host shutdown.
#[derive(Default)]
pub struct TelegramSupervisor {
    pub task: Mutex<Option<JoinHandle<()>>>,
}

impl TelegramSupervisor {
    pub async fn stop(&self) {
        if let Some(t) = self.task.lock().await.take() {
            t.abort();
        }
    }
}

/// Start the long-poll loop if the host has a non-empty bot token in
/// config. If a loop is already running, stop it first (token may have
/// changed). Safe to call any number of times — no-op when token empty.
pub async fn start_or_restart<R: Runtime>(state: SharedState, app: AppHandle<R>) -> anyhow::Result<()> {
    let token = state.config.read().telegram.bot_token.clone();
    let sup = supervisor(&state);
    sup.stop().await;

    if token.trim().is_empty() {
        tracing::info!("telegram: no bot token configured; supervisor idle");
        return Ok(());
    }

    let api = api::BotApi::new(token);
    // Validate + populate bot_username via getMe before kicking off
    // long-poll. Saves a round-trip on every pairing-link build.
    let me = api.get_me().await?;
    {
        let mut cfg = state.config.write();
        cfg.telegram.bot_username = me.username.clone().unwrap_or_default();
        cfg.save().ok();
    }

    // Best-effort: set the command list so paired users see slash-cmd
    // autocomplete in their phone keyboard. Failures here aren't fatal
    // (the bot still works without the menu).
    if let Err(e) = api.set_my_commands(&default_command_menu()).await {
        tracing::warn!("telegram: setMyCommands failed (non-fatal): {e:?}");
    }

    let handle = tokio::spawn(polling::run(api, state.clone(), app));
    *sup.task.lock().await = Some(handle);
    tracing::info!(
        "telegram: long-poll started as @{}",
        me.username.unwrap_or_else(|| "<unknown>".into())
    );
    Ok(())
}

/// Resolve the supervisor sub-state. Inlined here because all callers
/// hand us the full `SharedState`.
fn supervisor(state: &SharedState) -> Arc<TelegramSupervisor> {
    state.telegram.clone()
}

/// Slash commands surfaced in Telegram's command-menu UI. Mirrors what
/// the chat autocomplete shows. `/fast` and `/deep` are model-routing
/// switches — typing one alone (no body) flips the active slot for
/// the thread, typing one with a question routes that single turn AND
/// makes the slot sticky for follow-ups.
fn default_command_menu() -> Vec<api::BotCommand> {
    vec![
        api::BotCommand {
            command: "help".into(),
            description: "List available slash commands".into(),
        },
        api::BotCommand {
            command: "newchat".into(),
            description: "Start a fresh chat — ignore earlier context (memory kept)".into(),
        },
        api::BotCommand {
            command: "voice".into(),
            description: "Toggle spoken voice-note replies for this chat".into(),
        },
        api::BotCommand {
            command: "fast".into(),
            description: "Switch this chat to the fast model (default)".into(),
        },
        api::BotCommand {
            command: "deep".into(),
            description: "Switch this chat to the deep model (slower, smarter)".into(),
        },
        api::BotCommand {
            command: "pic".into(),
            description: "Generate an image (e.g. /pic a sunset over Miami)".into(),
        },
        api::BotCommand {
            command: "pichq".into(),
            description: "Generate a higher-quality image (slower)".into(),
        },
    ]
}
