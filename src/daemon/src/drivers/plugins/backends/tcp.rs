// SPDX-License-Identifier: GPL-3.0-or-later
//! TCP plugin transport backend: a byte-stream `Transport` connected to a
//! host:port read from the plugin's own resolved config values (see
//! `manifest::TcpConfig`), not from a hardware discovery handle. Currently
//! reachable only via a config-instantiated integration plugin (no `match`
//! spec) — `matches` always returns `false`, so a `match = { transport =
//! "tcp" }` spec (unsupported) simply never triggers rather than erroring.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};

use crate::drivers::plugins::manifest::{MatchSpec, PluginManifest};
use crate::drivers::plugins::transport::{PluginIo, PluginTransportDescriptor};
use crate::drivers::transports::tcp::TcpTransport;
use crate::registry::discovery::DiscoveryHandle;

fn matches(_spec: &MatchSpec, _handle: &DiscoveryHandle<'_>) -> bool {
    false
}

fn open(
    manifest: &PluginManifest,
    _handle: &DiscoveryHandle<'_>,
    config: &HashMap<String, String>,
) -> Result<PluginIo> {
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
    // (host/port), not a discovery handle — see `mod::integration_device_id`.
    "0".to_owned()
}

fn validate(_spec: &MatchSpec) -> Result<()> {
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
        // A real `match` spec is only needed to satisfy `parse_manifest`'s
        // "declares neither a match nor effects" guard — the `tcp` backend's
        // `open` never looks at it (it reads host/port from `config`).
        let src = r#"return {
            match = { transport = "hid", vid = 1, pid = 2 },
            identity = { vendor = "x", model = "y" },
            config = { fields = {
              { key = "host", label = "Host" },
              { key = "port", label = "Port" },
            } },
            transports = { tcp = { timeout_ms = 200 } },
        }"#;
        parse_manifest(src, Path::new("tcptest.lua")).unwrap()
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
        let spec = MatchSpec {
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
        let err = match open(&manifest, &hid(), &config) {
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
        let err = match open(&manifest, &hid(), &config) {
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
        assert!(open(&manifest, &hid(), &config).is_err());
    }
}
