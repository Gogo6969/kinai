//! "Your VPN is eating the connection" detection.
//!
//! Field report 2026-08-12: a family member on Fedora installed Mullvad,
//! and KinAI went dark with nothing but
//!
//!   Couldn't reach the KinAI host at ws://192.168.1.56:4847/kin:
//!   IO error: Connection timed out (os error 110)
//!
//! The host was perfectly healthy — every other device was connected,
//! and `ip route get 192.168.1.56` on the affected machine correctly
//! reported `dev enp3s0`, its ethernet. Routing was never the problem:
//! Mullvad (like most VPNs) blocks local-network traffic by default as
//! part of its kill switch, which drops the packets AFTER the routing
//! decision. The error gave no hint and the machine looked broken.
//!
//! So, when a connection to a LAN address fails, check both: the
//! interface the address routes through, AND whether any tunnel is up
//! at all. The second is what catches the kill-switch case — the first
//! test on its own would have said "no VPN here" and been wrong.
//!
//! Deliberately narrow — it only runs after a failure, only for private
//! addresses, and a wrong guess costs nothing but an unhelpful sentence.

use std::net::IpAddr;
use std::time::Duration;

/// How long the OS route query may take before we give up and stay
/// quiet. This runs on a path where the user is already waiting on a
/// failed connection, so it must not add a noticeable pause.
const ROUTE_QUERY_BUDGET: Duration = Duration::from_millis(1500);

/// Interface-name fragments that mean "virtual tunnel".
///
/// Matched case-insensitively against the interface the OS says it would
/// use. Covers the WireGuard/OpenVPN family (`wg0-mullvad`, `tun0`,
/// `proton0`, `nordlynx`), macOS's `utun*`, Windows adapter aliases that
/// carry the vendor name, and mesh VPNs (Tailscale, ZeroTier).
const TUNNEL_MARKERS: &[&str] = &[
    "wg", "tun", "tap", "ppp", "ipsec", "utun", "mullvad", "proton",
    "nord", "expressvpn", "surfshark", "wireguard", "openvpn",
    "tailscale", "zerotier", "zt", "vpn",
];

fn is_tunnel_interface(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    if n.is_empty() {
        return false;
    }
    // Guard the obvious physical names first: "wg" would otherwise match
    // nothing real, but "tun" is a substring of nothing common while
    // "zt"/"vpn" are short enough to want a sanity check.
    const PHYSICAL: &[&str] = &["eth", "en", "wl", "wlan", "wifi", "ethernet", "lo", "br", "docker"];
    if PHYSICAL.iter().any(|p| n == *p) {
        return false;
    }
    TUNNEL_MARKERS.iter().any(|m| n.contains(m))
}

/// The address KinAI is trying to reach, if the URL names a literal IP.
///
/// A hostname is deliberately NOT resolved: this runs on a failure path
/// where DNS may be exactly what is broken, and the hint is only worth
/// showing for a private address anyway.
fn host_ip(url: &str) -> Option<IpAddr> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let authority = after_scheme.split(['/', '?']).next()?;
    // Strip user-info and the port. IPv6 literals are bracketed.
    let hostpart = authority.rsplit('@').next()?;
    let host = if let Some(end) = hostpart.strip_prefix('[') {
        end.split(']').next()?.to_string()
    } else {
        hostpart.split(':').next()?.to_string()
    };
    host.parse().ok()
}

/// True for addresses that can only live on the local network — the only
/// case where "your VPN captured LAN traffic" is a sensible diagnosis.
fn is_private(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        // Unique-local (fc00::/7) and link-local (fe80::/10).
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            (seg[0] & 0xfe00) == 0xfc00 || (seg[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Interface name out of `ip route get` (Linux).
///
/// `192.168.1.56 dev wg0-mullvad table 1836018789 src 10.160.125.87`
pub(crate) fn parse_linux_route(out: &str) -> Option<String> {
    let mut it = out.split_whitespace();
    while let Some(tok) = it.next() {
        if tok == "dev" {
            return it.next().map(str::to_string);
        }
    }
    None
}

/// Interface name out of `route -n get <ip>` (macOS).
///
/// ```text
///    route to: 192.168.1.56
///   interface: utun4
/// ```
pub(crate) fn parse_macos_route(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.trim().strip_prefix("interface:"))
        .map(|v| v.trim().to_string())
}

/// Interface alias out of PowerShell's `Find-NetRoute` (Windows). We ask
/// for the bare property, so the output is the alias on its own line.
pub(crate) fn parse_windows_route(out: &str) -> Option<String> {
    out.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Ask the OS which interface it would use to reach `ip`.
async fn route_interface(ip: &IpAddr) -> Option<String> {
    let target = ip.to_string();

    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("ip");
        c.args(["route", "get", &target]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("route");
        c.args(["-n", "get", &target]);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("powershell");
        c.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "(Find-NetRoute -RemoteIPAddress {target} -ErrorAction Stop | \
Select-Object -First 1 -ExpandProperty InterfaceAlias)"
            ),
        ]);
        c
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let mut cmd = tokio::process::Command::new("true");

    let out = tokio::time::timeout(ROUTE_QUERY_BUDGET, cmd.output())
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);

    #[cfg(target_os = "linux")]
    return parse_linux_route(&text);
    #[cfg(target_os = "macos")]
    return parse_macos_route(&text);
    #[cfg(target_os = "windows")]
    return parse_windows_route(&text);
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    return None;
}

/// Per-VPN instructions when the interface name gives the vendor away.
/// Generic advice otherwise — every VPN has this switch, they just name
/// it differently.
fn how_to_fix(interface: &str) -> String {
    let n = interface.to_ascii_lowercase();
    if n.contains("mullvad") {
        "In Mullvad: Settings → VPN settings → turn ON \"Local network sharing\" \
(or run `mullvad lan set allow`)."
            .into()
    } else if n.contains("proton") {
        "In Proton VPN: Settings → Advanced → turn ON \"Allow LAN connections\".".into()
    } else if n.contains("nord") {
        "In NordVPN: Settings → turn ON \"Invisibility on LAN\" (or run \
`nordvpn set lan-discovery on`)."
            .into()
    } else if n.contains("tailscale") {
        "Tailscale is capturing local traffic — check whether an exit node is \
enabled, and turn it off to reach devices at home."
            .into()
    } else {
        "Look for a setting called \"allow local network\", \"LAN access\" or \
\"local network sharing\" in your VPN app and turn it on."
            .into()
    }
}

/// Tunnel interfaces that are currently UP on this machine.
///
/// Checked in addition to the route, because the route table alone is
/// NOT sufficient: a VPN kill switch filters packets AFTER the routing
/// decision. On the 2026-08-12 Fedora case `ip route get 192.168.1.56`
/// correctly reported `dev enp3s0` — the on-link subnet route — while
/// Mullvad's firewall silently dropped the packets anyway. Routing
/// looked innocent; the connection still timed out.
async fn active_tunnel_interfaces() -> Vec<String> {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = tokio::process::Command::new("powershell");
        c.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-NetAdapter | Where-Object Status -eq 'Up').Name",
        ]);
        c
    } else {
        // `ip -o link` on Linux, `ifconfig -l` on macOS — both list the
        // interface names; we only need the names, not their state
        // details, and a down tunnel simply won't be listed by `ip`.
        let mut c = if cfg!(target_os = "linux") {
            let mut c = tokio::process::Command::new("ip");
            c.args(["-o", "link", "show", "up"]);
            c
        } else {
            let mut c = tokio::process::Command::new("ifconfig");
            c.arg("-lu");
            c
        };
        c.kill_on_drop(true);
        c
    };

    let Ok(Ok(out)) = tokio::time::timeout(ROUTE_QUERY_BUDGET, cmd.output()).await else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    parse_interface_names(&text)
        .into_iter()
        .filter(|n| is_tunnel_interface(n))
        .collect()
}

/// Interface names out of any of the three listing formats.
///
///   Linux   `ip -o link show up` → "3: enp3s0: <BROADCAST,UP,...> mtu 1500 …"
///   macOS   `ifconfig -lu`       → "lo0 en0 utun0 utun4"
///   Windows adapter names        → one per line
pub(crate) fn parse_interface_names(out: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, ':');
        let head = parts.next().unwrap_or("").trim();
        // Linux form: a numeric interface index before the name.
        if !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()) {
            if let Some(name) = parts.next() {
                // "wg0-mullvad" or "eth0@if12" — keep the part before '@'.
                let name = name.trim().split('@').next().unwrap_or("").trim();
                if !name.is_empty() {
                    names.push(name.to_string());
                }
                continue;
            }
        }
        // macOS lists them all on one line; Windows one per line.
        names.extend(line.split_whitespace().map(str::to_string));
    }
    names
}

/// The sentence to append to a failed-connection error, when a VPN is
/// the likely reason. `None` when the address isn't local, or no VPN is
/// anywhere in the picture.
pub async fn lan_vpn_hint(url: &str) -> Option<String> {
    let ip = host_ip(url)?;
    if !is_private(&ip) {
        return None;
    }

    // Strongest signal: the route itself leaves through a tunnel.
    if let Some(iface) = route_interface(&ip).await {
        if is_tunnel_interface(&iface) {
            return Some(format!(
                "\n\nThis looks like your VPN: traffic to {ip} is going through \"{iface}\" \
instead of your home network, so it never reaches the KinAI host. Most VPNs block \
local devices by default. {}",
                how_to_fix(&iface)
            ));
        }
    }

    // Weaker but just as common: routing is fine and the VPN's kill
    // switch drops the packets anyway. Only worth saying when a tunnel
    // is actually up — otherwise we would blame a VPN nobody is running.
    let tunnels = active_tunnel_interfaces().await;
    let iface = tunnels.first()?;
    Some(format!(
        "\n\nA VPN is running on this computer (\"{iface}\"), and KinAI could not reach \
{ip} on your home network. Most VPNs block local devices by default, even when the \
connection looks correctly routed. {}",
        how_to_fix(iface)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_real_mullvad_output() {
        // Verbatim from the Fedora machine that hit this.
        let out = "192.168.1.56 dev wg0-mullvad table 1836018789 src 10.160.125.87 uid 1000\n    cache mtu 1305\n";
        assert_eq!(parse_linux_route(out).as_deref(), Some("wg0-mullvad"));
        assert!(is_tunnel_interface("wg0-mullvad"));
    }

    #[test]
    fn a_healthy_linux_route_is_not_a_tunnel() {
        let out = "192.168.1.56 dev enp3s0 src 192.168.1.42 uid 1000\n    cache\n";
        let iface = parse_linux_route(out).unwrap();
        assert_eq!(iface, "enp3s0");
        assert!(!is_tunnel_interface(&iface), "ethernet must not read as a tunnel");
    }

    #[test]
    fn parses_macos_route_output() {
        let vpn = "   route to: 192.168.1.56\ndestination: 192.168.1.56\n  interface: utun4\n";
        assert_eq!(parse_macos_route(vpn).as_deref(), Some("utun4"));
        assert!(is_tunnel_interface("utun4"));

        let lan = "   route to: 192.168.1.56\n  interface: en0\n";
        assert_eq!(parse_macos_route(lan).as_deref(), Some("en0"));
        assert!(!is_tunnel_interface("en0"), "en0 is the Mac's Wi-Fi");
    }

    #[test]
    fn parses_windows_alias() {
        assert_eq!(parse_windows_route("\nMullvad\n").as_deref(), Some("Mullvad"));
        assert!(is_tunnel_interface("Mullvad"));
        assert!(!is_tunnel_interface("Ethernet"));
        assert!(!is_tunnel_interface("Wi-Fi"));
    }

    #[test]
    fn only_private_addresses_qualify() {
        assert!(is_private(&"192.168.1.56".parse().unwrap()));
        assert!(is_private(&"10.0.0.5".parse().unwrap()));
        assert!(is_private(&"172.16.4.9".parse().unwrap()));
        // A host reachable over the internet is not a LAN-blocking case.
        assert!(!is_private(&"93.184.216.34".parse().unwrap()));
        // Mullvad's own tunnel address range is public-ish 10/8 — still
        // private, but we only ever test the HOST address, not the src.
        assert!(is_private(&"172.31.255.255".parse().unwrap()));
        assert!(!is_private(&"172.32.0.1".parse().unwrap()));
    }

    #[test]
    fn pulls_the_ip_out_of_a_ws_url() {
        assert_eq!(
            host_ip("ws://192.168.1.56:4847/kin").map(|i| i.to_string()),
            Some("192.168.1.56".into())
        );
        assert_eq!(
            host_ip("wss://10.0.0.9/kin").map(|i| i.to_string()),
            Some("10.0.0.9".into())
        );
        // A hostname is not resolved on purpose.
        assert_eq!(host_ip("ws://kinai.local:4847/kin"), None);
    }

    #[test]
    fn vendor_specific_advice_when_the_name_gives_it_away() {
        assert!(how_to_fix("wg0-mullvad").contains("mullvad lan set allow"));
        assert!(how_to_fix("proton0").contains("Allow LAN"));
        assert!(how_to_fix("utun4").contains("local network"));
    }
}

#[cfg(test)]
mod interface_list_tests {
    use super::*;

    #[test]
    fn parses_linux_ip_link_output() {
        // Real `ip -o link show up` shape, including the Mullvad tunnel.
        let out = "1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536 qdisc noqueue state UNKNOWN\n\
3: enp3s0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc fq_codel state UP\n\
7: wg0-mullvad: <POINTOPOINT,NOARP,UP,LOWER_UP> mtu 1305 qdisc noqueue state UNKNOWN\n";
        let names = parse_interface_names(out);
        assert!(names.contains(&"enp3s0".to_string()), "got {names:?}");
        assert!(names.contains(&"wg0-mullvad".to_string()), "got {names:?}");
        let tunnels: Vec<_> = names.iter().filter(|n| is_tunnel_interface(n)).collect();
        assert_eq!(tunnels, vec![&"wg0-mullvad".to_string()]);
    }

    #[test]
    fn parses_macos_ifconfig_list() {
        let names = parse_interface_names("lo0 gif0 stf0 en0 utun0 utun4\n");
        assert!(names.contains(&"en0".to_string()));
        assert!(names.iter().filter(|n| is_tunnel_interface(n)).count() == 2);
    }

    #[test]
    fn parses_windows_adapter_names() {
        let names = parse_interface_names("Ethernet\r\nWi-Fi\r\nMullvad\r\n");
        assert!(names.contains(&"Mullvad".to_string()), "got {names:?}");
        assert!(!is_tunnel_interface("Ethernet"));
    }

    /// The exact 2026-08-12 situation: the route to the host is the
    /// ordinary ethernet link (so the route check alone finds nothing),
    /// yet Mullvad is up and its kill switch is dropping LAN packets.
    #[test]
    fn kill_switch_case_is_still_detected_via_active_tunnels() {
        let route = "192.168.1.56 dev enp3s0 src 192.168.1.218 uid 1000\n    cache\n";
        let routed = parse_linux_route(route).unwrap();
        assert!(!is_tunnel_interface(&routed), "route looks innocent — that is the trap");

        let links = "3: enp3s0: <UP> mtu 1500\n7: wg0-mullvad: <UP> mtu 1305\n";
        let tunnels: Vec<_> = parse_interface_names(links)
            .into_iter()
            .filter(|n| is_tunnel_interface(n))
            .collect();
        assert_eq!(tunnels, vec!["wg0-mullvad".to_string()],
            "an active tunnel must still be found when routing looks fine");
        assert!(how_to_fix(&tunnels[0]).contains("mullvad lan set allow"));
    }
}
