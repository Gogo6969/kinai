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

/// Interface names that identify a VPN by vendor. Substring match —
/// these are unambiguous wherever they appear.
const VENDOR_MARKERS: &[&str] = &[
    "mullvad", "proton", "nordlynx", "expressvpn", "surfshark",
    "wireguard", "openvpn", "tailscale", "zerotier", "vpn",
];

/// Generic tunnel prefixes. Matched as a PREFIX followed by a digit or a
/// dash — not `contains` — so real hardware keeps its name: an earlier
/// version matched "tun" anywhere and "zt" as a substring, which turned
/// `ztest0` into a VPN. `ppp` is deliberately absent: `ppp0` is a DSL
/// user's actual internet connection, not a tunnel.
const TUNNEL_PREFIXES: &[&str] = &["wg", "utun", "tun", "tap", "ipsec", "zt"];

fn is_tunnel_interface(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    if n.is_empty() {
        return false;
    }
    if VENDOR_MARKERS.iter().any(|m| n.contains(m)) {
        return true;
    }
    TUNNEL_PREFIXES.iter().any(|p| {
        n.strip_prefix(p).is_some_and(|rest| {
            rest.is_empty()
                || rest.starts_with('-')
                || rest.chars().next().is_some_and(|c| c.is_ascii_digit())
        })
    })
}

/// An address that can actually carry traffic somewhere.
///
/// THE macOS DISCRIMINATOR. Every stock Mac runs utun0–utun3 for
/// Continuity, AWDL, Handoff and Private Relay with no VPN installed at
/// all, and they hold nothing but an `fe80::` link-local address. A real
/// VPN tunnel is assigned a routable address (Mullvad handed the Fedora
/// box 10.160.125.87). Without this test the hint fired on every Mac,
/// every time, for any connection failure — verified on this machine.
pub(crate) fn is_routable_addr(addr: &str) -> bool {
    let a = addr.trim().to_ascii_lowercase();
    if a.is_empty() {
        return false;
    }
    !(a.starts_with("fe80")
        || a.starts_with("169.254")
        || a.starts_with("127.")
        || a == "::1"
        || a.starts_with("0."))
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

    cmd.kill_on_drop(true);
    hide_console(&mut cmd);
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

/// Tunnel interfaces that are up AND carry a routable address.
///
/// Checked in addition to the route, because the route table alone is
/// NOT sufficient: a VPN kill switch filters packets AFTER the routing
/// decision. On the 2026-08-12 Fedora case `ip route get 192.168.1.56`
/// correctly reported `dev enp3s0` while Mullvad silently dropped the
/// packets anyway.
///
/// The routable-address requirement is what keeps this honest on macOS,
/// where utun0–utun3 always exist for Continuity/Private Relay.
async fn active_vpn_tunnels() -> Vec<String> {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = tokio::process::Command::new("powershell");
        c.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-NetIPAddress | Where-Object { $_.AddressState -eq 'Preferred' } | ForEach-Object { \"$($_.InterfaceAlias)\t$($_.IPAddress)\" }",
        ]);
        c
    } else if cfg!(target_os = "linux") {
        let mut c = tokio::process::Command::new("ip");
        c.args(["-o", "addr", "show", "up"]);
        c
    } else {
        // macOS: full ifconfig, so each interface block carries its
        // addresses and we can tell a real tunnel from a Continuity one.
        tokio::process::Command::new("ifconfig")
    };
    cmd.kill_on_drop(true);
    hide_console(&mut cmd);

    let Ok(Ok(out)) = tokio::time::timeout(ROUTE_QUERY_BUDGET, cmd.output()).await else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);

    let pairs = if cfg!(target_os = "windows") {
        parse_windows_addrs(&text)
    } else if cfg!(target_os = "linux") {
        parse_linux_addrs(&text)
    } else {
        parse_macos_ifconfig(&text)
    };

    pairs
        .into_iter()
        .filter(|(name, routable)| *routable && is_tunnel_interface(name))
        .map(|(name, _)| name)
        .collect()
}

/// `ip -o addr show up` → (interface, has-routable-address).
///
/// `7: wg0-mullvad    inet 10.160.125.87/32 scope global ...`
pub(crate) fn parse_linux_addrs(out: &str) -> Vec<(String, bool)> {
    let mut seen: Vec<(String, bool)> = Vec::new();
    for line in out.lines() {
        let mut it = line.split_whitespace();
        let Some(_idx) = it.next() else { continue };
        let Some(name) = it.next() else { continue };
        let name = name.trim_end_matches(':').split('@').next().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let routable = match (it.next(), it.next()) {
            (Some(fam), Some(addr)) if fam == "inet" || fam == "inet6" => {
                is_routable_addr(addr.split('/').next().unwrap_or(""))
            }
            _ => false,
        };
        match seen.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 |= routable,
            None => seen.push((name, routable)),
        }
    }
    seen
}

/// Full `ifconfig` output (macOS) → (interface, has-routable-address).
pub(crate) fn parse_macos_ifconfig(out: &str) -> Vec<(String, bool)> {
    let mut seen: Vec<(String, bool)> = Vec::new();
    let mut current: Option<String> = None;
    for line in out.lines() {
        if !line.starts_with([' ', '\t']) {
            // "utun3: flags=8051<UP,...> mtu 1500"
            if let Some((name, _)) = line.split_once(':') {
                let name = name.trim().to_string();
                if !name.is_empty() {
                    seen.push((name.clone(), false));
                    current = Some(name);
                }
            }
            continue;
        }
        let t = line.trim();
        let addr = t
            .strip_prefix("inet ")
            .or_else(|| t.strip_prefix("inet6 "))
            .and_then(|rest| rest.split_whitespace().next())
            .map(|a| a.split('%').next().unwrap_or(a));
        if let (Some(addr), Some(name)) = (addr, current.as_ref()) {
            if is_routable_addr(addr) {
                if let Some(e) = seen.iter_mut().find(|(n, _)| n == name) {
                    e.1 = true;
                }
            }
        }
    }
    seen
}

/// PowerShell "alias<TAB>address" lines → (interface, has-routable-address).
/// Tab-separated on purpose: Windows adapter aliases contain spaces
/// ("Ethernet 2"), and splitting on whitespace shredded them.
pub(crate) fn parse_windows_addrs(out: &str) -> Vec<(String, bool)> {
    let mut seen: Vec<(String, bool)> = Vec::new();
    for line in out.lines() {
        let Some((name, addr)) = line.trim_end_matches('\r').split_once('\t') else { continue };
        let name = name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let routable = is_routable_addr(addr);
        match seen.iter_mut().find(|(n, _)| *n == name) {
            Some(e) => e.1 |= routable,
            None => seen.push((name, routable)),
        }
    }
    seen
}

/// Release builds are GUI-subsystem on Windows, so spawning a console
/// program pops a black console window. Two of them, on every reconnect
/// attempt, forever, for a family whose host is simply switched off.
#[cfg(target_os = "windows")]
fn hide_console(cmd: &mut tokio::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}
#[cfg(not(target_os = "windows"))]
fn hide_console(_cmd: &mut tokio::process::Command) {}

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
    let tunnels = active_vpn_tunnels().await;
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
mod interface_tests {
    use super::*;

    /// THE regression this module exists to avoid shipping: a stock Mac
    /// with no VPN. utun0–utun3 are always up for Continuity/Private
    /// Relay and carry only link-local addresses. Verbatim `ifconfig`
    /// shape from the machine that reproduced the false positive.
    #[test]
    fn stock_mac_without_a_vpn_reports_no_tunnel() {
        let out = "\
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\
\tinet 192.168.1.56 netmask 0xffffff00 broadcast 192.168.1.255\n\
utun0: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1500\n\
\tinet6 fe80::ce81:b1c:bd2c:69e%utun0 prefixlen 64 scopeid 0x10\n\
utun3: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1000\n\
\tinet6 fe80::5a1b:2ff:fe00:1%utun3 prefixlen 64 scopeid 0x13\n";
        let vpns: Vec<_> = parse_macos_ifconfig(out)
            .into_iter()
            .filter(|(n, routable)| *routable && is_tunnel_interface(n))
            .collect();
        assert!(vpns.is_empty(), "a Mac with no VPN must report none, got {vpns:?}");
    }

    /// A real VPN on macOS: the tunnel carries a routable address.
    #[test]
    fn a_real_macos_vpn_is_still_detected() {
        let out = "\
en0: flags=8863<UP,BROADCAST,RUNNING> mtu 1500\n\
\tinet 192.168.1.56 netmask 0xffffff00\n\
utun4: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1280\n\
\tinet 10.64.0.2 --> 10.64.0.2 netmask 0xffffffff\n";
        let vpns: Vec<_> = parse_macos_ifconfig(out)
            .into_iter()
            .filter(|(n, r)| *r && is_tunnel_interface(n))
            .map(|(n, _)| n)
            .collect();
        assert_eq!(vpns, vec!["utun4".to_string()]);
    }

    /// The Fedora case, end to end: routing looks innocent, Mullvad is
    /// up with a routable address, so check 2 must catch it.
    #[test]
    fn mullvad_kill_switch_case_is_detected() {
        let route = "192.168.1.56 dev enp3s0 src 192.168.1.218 uid 1000\n    cache\n";
        assert!(!is_tunnel_interface(&parse_linux_route(route).unwrap()));

        let addrs = "\
3: enp3s0    inet 192.168.1.218/24 brd 192.168.1.255 scope global dynamic enp3s0\\       valid_lft 6000sec\n\
7: wg0-mullvad    inet 10.160.125.87/32 scope global wg0-mullvad\\       valid_lft forever\n";
        let vpns: Vec<_> = parse_linux_addrs(addrs)
            .into_iter()
            .filter(|(n, r)| *r && is_tunnel_interface(n))
            .map(|(n, _)| n)
            .collect();
        assert_eq!(vpns, vec!["wg0-mullvad".to_string()]);
        assert!(how_to_fix("wg0-mullvad").contains("mullvad lan set allow"));
    }

    #[test]
    fn real_hardware_is_never_called_a_vpn() {
        for name in ["en0", "eth0", "enp3s0", "wlan0", "Ethernet", "Ethernet 2", "Wi-Fi",
                     "bridge0", "lo0", "docker0", "ppp0", "ztest0", "anpi0", "llw0"] {
            assert!(!is_tunnel_interface(name), "{name} must not read as a VPN");
        }
        for name in ["wg0", "wg0-mullvad", "utun4", "tun0", "tap0", "zt0", "nordlynx",
                     "ProtonVPN", "Mullvad", "tailscale0"] {
            assert!(is_tunnel_interface(name), "{name} must read as a VPN");
        }
    }

    #[test]
    fn link_local_and_loopback_are_not_routable() {
        assert!(!is_routable_addr("fe80::1"));
        assert!(!is_routable_addr("169.254.3.4"));
        assert!(!is_routable_addr("127.0.0.1"));
        assert!(!is_routable_addr("::1"));
        assert!(!is_routable_addr(""));
        assert!(is_routable_addr("10.160.125.87"));
        assert!(is_routable_addr("192.168.1.218"));
    }

    /// Windows aliases contain spaces; splitting on whitespace used to
    /// shred "Ethernet 2" into two bogus interfaces.
    #[test]
    fn windows_aliases_with_spaces_survive() {
        let out = "Ethernet 2\t192.168.1.10\r\nMullvad\t10.64.0.2\r\nWi-Fi\tfe80::1\r\n";
        let pairs = parse_windows_addrs(out);
        assert!(pairs.iter().any(|(n, r)| n == "Ethernet 2" && *r));
        let vpns: Vec<_> = pairs.iter().filter(|(n, r)| *r && is_tunnel_interface(n)).collect();
        assert_eq!(vpns.len(), 1, "only Mullvad, got {vpns:?}");
    }
}
