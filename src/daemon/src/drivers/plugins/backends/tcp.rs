// SPDX-License-Identifier: GPL-3.0-or-later
//! TCP plugin transport backend: a byte-stream `Transport` connected to a
//! host:port read from the plugin's own resolved config values (see
//! `manifest::TcpConfig`), not from a hardware discovery handle. Currently
//! reachable only via a config-instantiated integration plugin (no `match`
//! spec) — `matches` always returns `false`, so a `match = { transport =
//! "tcp" }` spec (unsupported) simply never triggers rather than erroring.

use std::collections::HashMap;
use std::net::{IpAddr, ToSocketAddrs};

use anyhow::{bail, Context, Result};
use halod_shared::types::Permission;

use crate::drivers::plugins::manifest::{DeviceSpec, PluginManifest};
use crate::drivers::plugins::transport::{PluginIo, PluginTransportDescriptor};
use crate::drivers::transports::tcp::TcpTransport;
use crate::registry::discovery::DiscoveryHandle;

fn matches(_spec: &DeviceSpec, _handle: &DiscoveryHandle<'_>) -> bool {
    false
}

/// Reject a host that resolves to a loopback/private/link-local/unspecified
/// address unless the manifest opted in via `allow_private` — the SSRF guard
/// keeping an integration off localhost admin services and the cloud metadata
/// endpoint (`169.254.169.254`). Resolution failures fall through to the connect
/// call, which will report them.
fn reject_ssrf_host(host: &str, allow_private: bool) -> Result<()> {
    if allow_private {
        return Ok(());
    }
    // Resolve every address the host maps to; a name that resolves to *any*
    // blocked address is rejected (defeats DNS-rebinding to a public name).
    let addrs = match (host, 0u16).to_socket_addrs() {
        Ok(a) => a,
        Err(_) => return Ok(()),
    };
    for sa in addrs {
        if is_blocked_ip(&sa.ip()) {
            bail!(
                "tcp host '{host}' resolves to a non-routable address {} (set transports.tcp.allow_private to permit LAN/localhost targets)",
                sa.ip()
            );
        }
    }
    Ok(())
}

/// Address ranges an untrusted integration must not reach by default.
fn is_blocked_ip(ip: &IpAddr) -> bool {
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
    reject_ssrf_host(&host, tcp.allow_private)?;
    let port_str = config.get(&tcp.port_key).cloned().unwrap_or_default();
    let port: u16 = port_str.parse().with_context(|| {
        format!(
            "plugin '{}': config field '{}' (tcp port) is not a valid port number: '{port_str}'",
            manifest.plugin_id, tcp.port_key
        )
    })?;

    let transport =
        TcpTransport::connect_blocking(&host, port, tcp.timeout_ms).with_context(|| {
            format!(
                "plugin '{}': connecting to {host}:{port}",
                manifest.plugin_id
            )
        })?;
    Ok(PluginIo::Stream {
        transport: std::sync::Arc::new(transport),
        bulk: None,
    })
}

fn id_suffix(_handle: &DiscoveryHandle<'_>) -> String {
    // Never invoked: an integration root's id is built from its config
    // (host/port), not a discovery handle — see `integration_scan::root_device_id`.
    "0".to_owned()
}

fn validate(_spec: &DeviceSpec) -> Result<()> {
    Ok(())
}

inventory::submit!(PluginTransportDescriptor {
    kind: "tcp",
    matches,
    open,
    id_suffix,
    validate,
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::plugins::manifest::parse_manifest;
    use std::path::Path;

    fn manifest_with_tcp() -> PluginManifest {
        // The device spec only satisfies parse_manifest's guard; `open` reads host/port from `config`.
        // `allow_private` so the loopback-host tests below exercise the port/connect
        // paths rather than tripping the SSRF guard (which has its own tests).
        let src = r#"return {
            permissions = {"network"},
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            config = { fields = {
              { key = "host", label = "Host" },
              { key = "port", label = "Port" },
            } },
            transports = { tcp = { timeout_ms = 200, allow_private = true } },
        }"#;
        parse_manifest(src, Path::new("tcptest.lua")).unwrap()
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
    fn matches_is_always_false() {
        let spec = DeviceSpec {
            vendor: "x".to_string(),
            model: "y".to_string(),
            transport: "tcp".to_string(),
            vid: None,
            pid: None,
            pids: vec![],
            usage_page: None,
            usage: None,
            interface: None,
            bus: None,
            addresses: None,
            extra_addresses: None,
            max_bytes_per_sec: None,
            pre_scan: false,
            probe: Default::default(),
            pci_match: vec![],
            name: None,
            device_type: None,
        };
        assert!(!matches(&spec, &hid()));
    }

    #[test]
    fn open_errors_when_host_field_is_unset() {
        let manifest = manifest_with_tcp();
        let config = HashMap::from([("port".to_string(), "6742".to_string())]);
        let err = match open(&manifest, &hid(), &config, NET) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(err.to_string().contains("host"));
    }

    #[test]
    fn open_errors_on_an_invalid_port() {
        let manifest = manifest_with_tcp();
        let config = HashMap::from([
            ("host".to_string(), "127.0.0.1".to_string()),
            ("port".to_string(), "not-a-number".to_string()),
        ]);
        let err = match open(&manifest, &hid(), &config, NET) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(err.to_string().contains("port"));
    }

    #[test]
    fn open_errors_when_nothing_is_listening() {
        let manifest = manifest_with_tcp();
        let config = HashMap::from([
            ("host".to_string(), "127.0.0.1".to_string()),
            ("port".to_string(), "1".to_string()),
        ]);
        // Requires a runtime for `TcpStream::from_std`'s reactor registration.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        assert!(open(&manifest, &hid(), &config, NET).is_err());
    }

    #[test]
    fn open_requires_the_network_permission() {
        let manifest = manifest_with_tcp();
        let config = HashMap::from([
            ("host".to_string(), "127.0.0.1".to_string()),
            ("port".to_string(), "6742".to_string()),
        ]);
        let err = err_msg(open(&manifest, &hid(), &config, &[]));
        assert!(err.contains("network"), "{err}");
    }

    #[test]
    fn open_blocks_ssrf_hosts_without_allow_private() {
        // A manifest that does NOT opt into private targets.
        let src = r#"return {
            permissions = {"network"},
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            config = { fields = { { key = "host", label = "Host" }, { key = "port", label = "Port" } } },
            transports = { tcp = { timeout_ms = 200 } },
        }"#;
        let manifest = parse_manifest(src, Path::new("tcptest.lua")).unwrap();
        for host in ["127.0.0.1", "169.254.169.254", "10.0.0.1", "::1"] {
            let config = HashMap::from([
                ("host".to_string(), host.to_string()),
                ("port".to_string(), "80".to_string()),
            ]);
            let err = err_msg(open(&manifest, &hid(), &config, NET));
            assert!(
                err.contains("non-routable"),
                "host {host} should be blocked: {err}"
            );
        }
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
