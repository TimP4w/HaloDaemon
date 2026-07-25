// SPDX-License-Identifier: GPL-3.0-or-later
//! TCP plugin transport backend: a byte-stream `Transport` connected to a
//! host:port read from the plugin's own resolved config values (see
//! `manifest::TcpConfig`), not from a hardware discovery handle. Currently
//! reachable only via a config-instantiated integration plugin (no `match`
//! spec) — `matches` always returns `false`, so a `match = { transport =
//! "tcp" }` spec (unsupported) simply never triggers rather than erroring.

use anyhow::{bail, Context, Result};
use halod_shared::types::Permission;

use crate::domain::plugin::engine::backends::net_guard::resolve_vetted_addr;
use crate::domain::plugin::engine::transport::{PluginIo, PluginTransportDescriptor};
use crate::domain::plugin::manifest::PluginManifest;
use crate::domain::registry::observers::discovery::DiscoveryHandle;
use crate::infrastructure::drivers::transports::tcp::TcpTransport;

fn open(
    manifest: &PluginManifest,
    _handle: &DiscoveryHandle<'_>,
    config: &crate::domain::plugin::ResolvedConfig,
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
    let host = config
        .get(&tcp.host_key)
        .map(crate::domain::plugin::ResolvedConfigValue::to_config_string)
        .unwrap_or_default();
    if host.is_empty() {
        bail!(
            "plugin '{}': config field '{}' (tcp host) is not set",
            manifest.plugin_id,
            tcp.host_key
        );
    }
    let port_str = config
        .get(&tcp.port_key)
        .map(crate::domain::plugin::ResolvedConfigValue::to_config_string)
        .unwrap_or_default();
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
    crate::infrastructure::drivers::transports::Transport::set_write_rate_limit(&transport, limit);
    Ok(PluginIo::Stream {
        transport: std::sync::Arc::new(transport),
        usb: None,
    })
}

// `tcp` is config-instantiated (an integration reads its host/port from config),
// never discovery-matched, so it carries only `open` — `matches`/`id_suffix`/
// `validate` are `None`. See `integration_scan`, which calls `open` directly.
pub(super) const DESCRIPTOR: PluginTransportDescriptor = PluginTransportDescriptor {
    kind: "tcp",
    matches: None,
    open,
    id_suffix: None,
    validate: None,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn text(value: &str) -> crate::domain::plugin::ResolvedConfigValue {
        crate::domain::plugin::ResolvedConfigValue::String(value.to_owned())
    }

    fn integer(value: i64) -> crate::domain::plugin::ResolvedConfigValue {
        crate::domain::plugin::ResolvedConfigValue::Integer(value)
    }

    fn manifest_with_tcp(allow_private: bool) -> PluginManifest {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("tcptest");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            format!(
                "id: tcptest\npermissions: [hid, network]\ntransports:\n  tcp:\n    host_key: host\n    port_key: port\n    timeout_ms: 200\n    allow_private: {allow_private}\nconfig:\n  fields:\n    - key: host\n      label: Host\n      kind: host\n    - key: port\n      label: Port\n      kind: port\ndevices:\n  - vendor: Test\n    model: TCP\n    match:\n      hid: {{ vid: 1, pid: 2 }}\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
        crate::domain::plugin::parse_manifest_from_dir(&dir).unwrap()
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
        let config = HashMap::from([("port".to_string(), integer(6742))]);
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
            ("host".to_string(), text("127.0.0.1")),
            ("port".to_string(), text("not-a-number")),
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
            ("host".to_string(), text("127.0.0.1")),
            ("port".to_string(), integer(1)),
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
            ("host".to_string(), text("127.0.0.1")),
            ("port".to_string(), integer(6742)),
        ]);
        let err = err_msg(open(&manifest, &hid(), &config, &[], None));
        assert!(err.contains("network"), "{err}");
    }

    #[test]
    fn open_blocks_ssrf_hosts_without_allow_private() {
        let manifest = manifest_with_tcp(false);
        for host in ["127.0.0.1", "169.254.169.254", "10.0.0.1", "::1"] {
            let config = HashMap::from([
                ("host".to_string(), text(host)),
                ("port".to_string(), integer(80)),
            ]);
            let err = err_msg(open(&manifest, &hid(), &config, NET, None));
            assert!(
                err.contains("non-routable"),
                "host {host} should be blocked: {err}"
            );
        }
    }
}
