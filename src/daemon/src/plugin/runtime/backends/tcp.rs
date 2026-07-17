// SPDX-License-Identifier: GPL-3.0-or-later
//! TCP plugin transport backend: a byte-stream `Transport` connected to a
//! host:port read from the plugin's own resolved config values (see
//! `manifest::TcpConfig`), not from a hardware discovery handle. Currently
//! reachable only via a config-instantiated integration plugin (no `match`
//! spec) — `matches` always returns `false`, so a `match = { transport =
//! "tcp" }` spec (unsupported) simply never triggers rather than erroring.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use anyhow::{bail, Context, Result};
use halod_shared::types::Permission;

use crate::drivers::transports::tcp::TcpTransport;
use crate::plugin::manifest::PluginManifest;
use crate::plugin::runtime::transport::{PluginIo, PluginTransportDescriptor};
use crate::registry::discovery::DiscoveryHandle;

/// Resolve `host:port` **once** and return a single vetted [`SocketAddr`] to
/// connect to — the caller connects to this exact address rather than re-passing
/// the hostname, so a DNS rebind between check and connect can't redirect the
/// socket (resolving twice is the classic SSRF TOCTOU). Unless the manifest opted
/// in via `allow_private`, every resolved address must be routable: a name that
/// resolves to *any* loopback/private/link-local address is rejected, keeping an
/// integration off localhost admin services and the cloud metadata endpoint
/// (`169.254.169.254`).
fn resolve_vetted_addr(host: &str, port: u16, allow_private: bool) -> Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving tcp host '{host}'"))?
        .collect();
    let first = *addrs
        .first()
        .ok_or_else(|| anyhow::anyhow!("tcp host '{host}' resolved to no addresses"))?;
    if allow_private {
        return Ok(first);
    }
    for sa in &addrs {
        if is_blocked_ip(&sa.ip()) {
            bail!(
                "tcp host '{host}' resolves to a non-routable address {} (set transports.tcp.allow_private to permit LAN/localhost targets)",
                sa.ip()
            );
        }
    }
    Ok(first)
}

/// Address ranges an untrusted integration must not reach by default.
pub(crate) fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                // Carrier-grade NAT 100.64.0.0/10.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 0x40)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique-local fc00::/7 and link-local fe80::/10.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped: unwrap and re-check against the v4 rules.
                || v6.to_ipv4_mapped().map(|m| is_blocked_ip(&IpAddr::V4(m))).unwrap_or(false)
        }
    }
}

fn open(
    manifest: &PluginManifest,
    _handle: &DiscoveryHandle<'_>,
    config: &HashMap<String, String>,
    granted: &[Permission],
    limit: Option<halod_shared::types::WriteRateLimit>,
) -> Result<PluginIo> {
    // A tcp transport reaches off the matched hardware onto the network, so it
    // is gated on the explicit Network grant (defence in depth: validate_manifest
    // also forces the manifest to declare it, which is what drives the consent
    // prompt — this rejects the actual connect if the grant is somehow absent).
    if !granted.contains(&Permission::Network) {
        bail!(
            "plugin '{}': tcp transport requires the 'network' permission",
            manifest.plugin_id
        );
    }
    let tcp = manifest.transports.tcp.clone().unwrap_or_default();
    let host = config.get(&tcp.host_key).cloned().unwrap_or_default();
    if host.is_empty() {
        bail!(
            "plugin '{}': config field '{}' (tcp host) is not set",
            manifest.plugin_id,
            tcp.host_key
        );
    }
    let port_str = config.get(&tcp.port_key).cloned().unwrap_or_default();
    let port: u16 = port_str.parse().with_context(|| {
        format!(
            "plugin '{}': config field '{}' (tcp port) is not a valid port number: '{port_str}'",
            manifest.plugin_id, tcp.port_key
        )
    })?;
    let addr = resolve_vetted_addr(&host, port, tcp.allow_private)?;

    let transport =
        TcpTransport::connect_addr_blocking(addr, tcp.timeout_ms).with_context(|| {
            format!(
                "plugin '{}': connecting to {host}:{port} ({addr})",
                manifest.plugin_id
            )
        })?;
    crate::drivers::transports::Transport::set_write_rate_limit(&transport, limit);
    Ok(PluginIo::Stream {
        transport: std::sync::Arc::new(transport),
        usb: None,
    })
}

// `tcp` is config-instantiated (an integration reads its host/port from config),
// never discovery-matched, so it carries only `open` — `matches`/`id_suffix`/
// `validate` are `None`. See `integration_scan`, which calls `open` directly.
inventory::submit!(PluginTransportDescriptor {
    kind: "tcp",
    matches: None,
    open,
    id_suffix: None,
    validate: None,
});

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with_tcp(allow_private: bool) -> PluginManifest {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("tcptest");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            format!(
                "id: tcptest\npermissions: [hid, network]\ntransports:\n  tcp:\n    host_key: host\n    port_key: port\n    timeout_ms: 200\n    allow_private: {allow_private}\nconfig:\n  fields:\n    - key: host\n      label: Host\n      kind: text\n    - key: port\n      label: Port\n      kind: number\ndevices:\n  - vendor: Test\n    model: TCP\n    match:\n      hid: {{ vid: 1, pid: 2 }}\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
        crate::plugin::parse_manifest_from_dir(&dir).unwrap()
    }

    const NET: &[Permission] = &[Permission::Network];

    /// `open` returns `Result<PluginIo>` and `PluginIo` isn't `Debug`, so extract
    /// the error's message directly instead of `unwrap_err()`.
    fn err_msg(r: Result<PluginIo>) -> String {
        match r {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    fn hid<'a>() -> DiscoveryHandle<'a> {
        DiscoveryHandle::Hid {
            vid: 0,
            pid: 0,
            path: "",
            serial: None,
            idx: 0,
            usage_page: 0,
            usage: 0,
            interface_number: None,
        }
    }

    #[test]
    fn open_errors_when_host_field_is_unset() {
        let manifest = manifest_with_tcp(true);
        let config = HashMap::from([("port".to_string(), "6742".to_string())]);
        let err = match open(&manifest, &hid(), &config, NET, None) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(err.to_string().contains("host"));
    }

    #[test]
    fn open_errors_on_an_invalid_port() {
        let manifest = manifest_with_tcp(true);
        let config = HashMap::from([
            ("host".to_string(), "127.0.0.1".to_string()),
            ("port".to_string(), "not-a-number".to_string()),
        ]);
        let err = match open(&manifest, &hid(), &config, NET, None) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(err.to_string().contains("port"));
    }

    #[test]
    fn open_errors_when_nothing_is_listening() {
        let manifest = manifest_with_tcp(true);
        let config = HashMap::from([
            ("host".to_string(), "127.0.0.1".to_string()),
            ("port".to_string(), "1".to_string()),
        ]);
        // Requires a runtime for `TcpStream::from_std`'s reactor registration.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        assert!(open(&manifest, &hid(), &config, NET, None).is_err());
    }

    #[test]
    fn open_requires_the_network_permission() {
        let manifest = manifest_with_tcp(true);
        let config = HashMap::from([
            ("host".to_string(), "127.0.0.1".to_string()),
            ("port".to_string(), "6742".to_string()),
        ]);
        let err = err_msg(open(&manifest, &hid(), &config, &[], None));
        assert!(err.contains("network"), "{err}");
    }

    #[test]
    fn open_blocks_ssrf_hosts_without_allow_private() {
        let manifest = manifest_with_tcp(false);
        for host in ["127.0.0.1", "169.254.169.254", "10.0.0.1", "::1"] {
            let config = HashMap::from([
                ("host".to_string(), host.to_string()),
                ("port".to_string(), "80".to_string()),
            ]);
            let err = err_msg(open(&manifest, &hid(), &config, NET, None));
            assert!(
                err.contains("non-routable"),
                "host {host} should be blocked: {err}"
            );
        }
    }

    #[test]
    fn resolve_vetted_addr_returns_one_checked_address() {
        // A public literal resolves to exactly itself and is returned, so the
        // caller connects to the vetted address (no second resolution a rebind
        // could redirect). A blocked literal is rejected unless private is allowed.
        let addr = resolve_vetted_addr("1.1.1.1", 443, false).unwrap();
        assert_eq!(addr.to_string(), "1.1.1.1:443");
        assert!(resolve_vetted_addr("127.0.0.1", 80, false).is_err());
        assert_eq!(
            resolve_vetted_addr("127.0.0.1", 80, true)
                .unwrap()
                .to_string(),
            "127.0.0.1:80"
        );
    }

    #[test]
    fn ssrf_guard_classifies_addresses() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        // A public address is allowed.
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))));
    }
}
