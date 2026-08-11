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
        pump_events(app, receiver).await;
    });
}

/// Forward resolved services to the UI until the channel closes.
///
/// Split out of `start_browser` so `rescan` can hand over to the receiver
/// its own `browse` call returns — see the comment there for why that
/// matters.
async fn pump_events(app: AppHandle, receiver: mdns_sd::Receiver<mdns_sd::ServiceEvent>) {
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
}

/// Force the browse daemon to re-issue an mDNS query. The Client setup
/// page calls this so a user who launched the app, granted Local Network
/// permission, then went looking for a host doesn't have to wait for the
/// next periodic re-announce from the host.
///
/// The returned receiver MUST be pumped. `browse` is not idempotent the
/// way this code used to assume: mdns-sd registers the listener with
/// `service_queriers.insert(ty, listener)` (service_daemon.rs), a plain
/// map insert, so a second browse for the same type REPLACES the sender.
/// Dropping the new receiver therefore closed the original one too, the
/// `while let Ok(..)` in `start_browser` fell out of its loop, and
/// discovery stayed dead until the app restarted — pressing "Scan again"
/// made things permanently worse instead of better.
pub fn rescan(app: AppHandle) -> anyhow::Result<()> {
    if let Some(daemon) = BROWSE_DAEMON.get() {
        let receiver = daemon.browse(SERVICE_TYPE)?;
        // Hand the fresh channel to a new pump. The previous pump exits
        // on its own the moment its sender is replaced.
        tauri::async_runtime::spawn(pump_events(app, receiver));
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
