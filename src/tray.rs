//! System tray menu with live status pushed via background tick.

use std::time::Duration;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Runtime};

use crate::SharedState;

pub fn install<R: Runtime>(app: &AppHandle<R>, state: SharedState) -> tauri::Result<()> {
    let open_item = MenuItem::with_id(app, "open", "Open KinAI", true, None::<&str>)?;
    let overlay_item = MenuItem::with_id(app, "overlay", "Toggle Overlay", true, None::<&str>)?;
    let invite_item = MenuItem::with_id(app, "invite", "Create Invite…", true, None::<&str>)?;
    let users_item = MenuItem::with_id(app, "users", "Manage Family…", true, None::<&str>)?;
    let settings_item = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let status_item = MenuItem::with_id(app, "status", "KinAI: starting…", false, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let about_item = MenuItem::with_id(app, "about", "About KinAI", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit KinAI", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &status_item,
            &separator,
            &open_item,
            &overlay_item,
            &separator,
            &invite_item,
            &users_item,
            &settings_item,
            &separator,
            &about_item,
            &quit_item,
        ],
    )?;

    if let Some(tray) = app.tray_by_id("main-tray") {
        tray.set_menu(Some(menu))?;
        tray.on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "overlay" => toggle_overlay(app),
            "invite" => {
                show_main(app);
                let _ = app.emit("kinai://open-route", "/host/invite");
            }
            "users" => {
                show_main(app);
                let _ = app.emit("kinai://open-route", "/host/family");
            }
            "settings" => {
                show_main(app);
                let _ = app.emit("kinai://open-route", "/settings");
            }
            "about" => {
                show_main(app);
                let _ = app.emit("kinai://open-route", "/about");
            }
            "quit" => app.exit(0),
            _ => {}
        });
        tray.on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_overlay(tray.app_handle());
            }
        });
    }

    spawn_status_ticker(app.clone(), state);
    Ok(())
}

fn spawn_status_ticker<R: Runtime>(app: AppHandle<R>, state: SharedState) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let label = compose_label(&state);
            if let Some(tray) = app.tray_by_id("main-tray") {
                let _ = tray.set_tooltip(Some(&label));
            }
            let stats = state.stats.read().clone();
            let _ = app.emit("kinai://stats", &stats);
        }
    });
}

fn compose_label(state: &SharedState) -> String {
    let stats = state.stats.read();
    let cfg = state.config.read();
    let mode = format!("{:?}", cfg.mode);
    let peers = stats.peers_connected;
    let model = &cfg.llm.model;
    match cfg.mode {
        crate::config::Mode::Host => format!(
            "KinAI · Host · {peers} connected · {model}"
        ),
        crate::config::Mode::Client => match &cfg.client.host_label {
            Some(l) => format!("KinAI · Client · {l}"),
            None => "KinAI · Client".into(),
        },
        crate::config::Mode::Unconfigured => format!("KinAI · {mode}"),
    }
}

fn show_main<R: Runtime>(app: &AppHandle<R>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

pub fn toggle_overlay<R: Runtime>(app: &AppHandle<R>) {
    let Some(overlay) = app.get_webview_window("overlay") else {
        return;
    };
    let visible = overlay.is_visible().unwrap_or(false);
    if visible {
        let _ = overlay.hide();
        return;
    }

    // Spotlight-style: when the user triggers the overlay (via the
    // global hotkey, tray icon click, or "Toggle Overlay" menu item),
    // they want the small "Ask KinAI…" input as a quick-chat surface
    // — NOT the full app window also coming forward.
    //
    // The "whole app comes forward" effect is a cross-platform side
    // effect of `set_focus()` on the overlay: on macOS focusing any
    // KinAI window activates the app, pulling every visible KinAI
    // window to the foreground; Windows behaves similarly. The fix is
    // to hide the main window before showing the overlay so the only
    // KinAI surface left visible is the small input.
    //
    // The user can bring the main window back via "Open KinAI" in the
    // tray menu (or by relaunching the app from the Dock / Start
    // menu). Same one-click cost as Spotlight → return-to-app.
    if let Some(main) = app.get_webview_window("main") {
        if main.is_visible().unwrap_or(false) {
            let _ = main.hide();
        }
    }
    let _ = overlay.show();
    let _ = overlay.set_focus();
    let _ = app.emit("kinai://overlay-focus", ());
}
