//! mDNS / Bonjour-style discovery for local-network hosts.
//!
//! When the host runs and `mdns_enabled` is true we advertise as
//! `_kinai._tcp.local.`. Every client that hasn't already paired with a host
//! browses for the same service and emits `kinai://discovery` events so the
//! UI can show "Smith Family KinAI on this network".

use std::sync::OnceLock;

use mdns_sd::{ServiceDaemon, ServiceInfo};
use tauri::{AppHandle, Emitter};

use crate::SharedState;

const SERVICE_TYPE: &str = "_kinai._tcp.local.";

/// The browse daemon spun up by `start_browser`. Held here so other code
/// can ask it to re-emit the resolved set (e.g. when the user opens the
/// Client setup page and the original mDNS announce already fired).
static BROWSE_DAEMON: OnceLock<ServiceDaemon> = OnceLock::new();

pub fn start_advertise(state: &SharedState, app: AppHandle) {
    if !state.config.read().host.mdns_enabled {
        return;
    }
    let cfg = state.config.read().clone();
    let port = cfg.host.port;
    let family_name = cfg.host.family_name.clone();

    tauri::async_runtime::spawn(async move {
        let Ok(daemon) = ServiceDaemon::new() else {
            tracing::warn!("mdns daemon could not be created");
            return;
        };
        let hostname = format!("{}.local.", sanitize(&family_name));
        let instance = format!("KinAI - {}", family_name);
        let Ok(ip) = local_ip_address::local_ip() else {
            tracing::warn!("no local ip available for mdns");
            return;
        };
        let info = match ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &hostname,
            ip.to_string().as_str(),
            port,
            &[
                ("family", family_name.as_str()),
                ("version", env!("CARGO_PKG_VERSION")),
            ][..],
        ) {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!("mdns service info: {e}");
                return;
            }
        };
        if let Err(e) = daemon.register(info) {
            tracing::warn!("mdns register: {e}");
            return;
        }
        tracing::info!("mdns: advertising as '{instance}' on port {port}");
        let _ = app.emit("kinai://mdns-advertising", serde_json::json!({"instance": instance}));
    });
}

pub fn start_browser(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let Ok(daemon) = ServiceDaemon::new() else {
            return;
        };
        let Ok(receiver) = daemon.browse(SERVICE_TYPE) else {
            return;
        };
        // Stash the daemon for `rescan()` — failure here means a second
        // start_browser call happened, which is benign (we just ignore).
        let _ = BROWSE_DAEMON.set(daemon);
        while let Ok(event) = receiver.recv_async().await {
            if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
                let family = info
                    .get_property_val_str("family")
                    .unwrap_or("Unknown")
                    .to_string();
                let host_url = info
                    .get_addresses()
                    .iter()
                    .next()
                    .map(|ip| format!("ws://{ip}:{}/kin", info.get_port()))
                    .unwrap_or_default();
                let payload = serde_json::json!({
                    "family_name": family,
                    "instance": info.get_fullname(),
                    "host_url": host_url,
                });
                let _ = app.emit("kinai://discovery", payload);
            }
        }
    });
}

/// Force the browse daemon to re-issue an mDNS query. The Client setup
/// page calls this so a user who launched the app, granted Local Network
/// permission, then went looking for a host doesn't have to wait for the
/// next periodic re-announce from the host. Returns Ok even if the daemon
/// hasn't started yet (call again after a tick).
pub fn rescan() -> anyhow::Result<()> {
    if let Some(daemon) = BROWSE_DAEMON.get() {
        // `browse` is idempotent — the daemon dedups subscriptions by type.
        // Calling it again triggers a fresh outgoing query.
        let _ = daemon.browse(SERVICE_TYPE);
    }
    Ok(())
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}
