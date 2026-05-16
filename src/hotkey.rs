//! Global hotkey registration.

use tauri::{AppHandle, Runtime};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutEvent, ShortcutState};

use crate::SharedState;

fn map_err(e: impl std::fmt::Display) -> tauri::Error {
    tauri::Error::Anyhow(anyhow::anyhow!("{e}"))
}

pub fn install<R: Runtime>(app: &AppHandle<R>, state: SharedState) -> tauri::Result<()> {
    let hotkey = state.config.read().overlay.hotkey.clone();
    let shortcut: tauri_plugin_global_shortcut::Shortcut = hotkey
        .parse()
        .unwrap_or_else(|_| "CmdOrCtrl+Space".parse().expect("fallback parses"));

    app.global_shortcut()
        .on_shortcut(
            shortcut,
            |app: &AppHandle<R>, _shortcut, event: ShortcutEvent| {
                if matches!(event.state(), ShortcutState::Pressed) {
                    crate::tray::toggle_overlay(app);
                }
            },
        )
        .map_err(map_err)?;
    Ok(())
}

pub fn replace<R: Runtime>(app: &AppHandle<R>, new_hotkey: &str) -> tauri::Result<()> {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let shortcut: tauri_plugin_global_shortcut::Shortcut =
        new_hotkey.parse().map_err(map_err)?;
    gs.on_shortcut(
        shortcut,
        |app: &AppHandle<R>, _shortcut, event: ShortcutEvent| {
            if matches!(event.state(), ShortcutState::Pressed) {
                crate::tray::toggle_overlay(app);
            }
        },
    )
    .map_err(map_err)?;
    Ok(())
}
