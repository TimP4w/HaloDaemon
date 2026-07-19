// SPDX-License-Identifier: GPL-3.0-or-later
//! Serial plugin transport backend: a byte-stream `Transport` bound to the port
//! selected through the plugin's own resolved config (see `manifest::SerialConfig`),
//! not a hardware discovery handle. Config-instantiated exactly like `tcp` — an
//! integration reads its port from config — so `matches` is `None` and the root
//! is reached through the plugin-integrations scanner calling `open` directly.

use anyhow::{bail, Context, Result};
use halod_shared::types::{Permission, WriteRateLimit};

use crate::domain::plugin::engine::transport::{PluginIo, PluginTransportDescriptor};
use crate::domain::plugin::manifest::PluginManifest;
use crate::domain::registry::observers::discovery::DiscoveryHandle;
use crate::infrastructure::drivers::transports::serial::SerialTransport;

fn open(
    manifest: &PluginManifest,
    _handle: &DiscoveryHandle<'_>,
    config: &crate::domain::plugin::ResolvedConfig,
    granted: &[Permission],
    limit: Option<WriteRateLimit>,
) -> Result<PluginIo> {
    if !granted.contains(&Permission::Serial) {
        bail!(
            "plugin '{}': serial transport requires the 'serial' permission",
            manifest.plugin_id
        );
    }
    let serial = manifest.transports.serial.clone().unwrap_or_default();
    let port = config
        .get(&serial.port_key)
        .map(crate::domain::plugin::ResolvedConfigValue::to_config_string)
        .unwrap_or_default();
    if port.is_empty() {
        bail!(
            "plugin '{}': config field '{}' (serial port) is not set",
            manifest.plugin_id,
            serial.port_key
        );
    }
    let transport = SerialTransport::open_blocking(&port, &serial)
        .with_context(|| format!("plugin '{}': opening serial port", manifest.plugin_id))?;
    // A manifest write ceiling overrides the config's own, matching every other
    // backend where the declared per-device limit wins.
    if limit.is_some() {
        crate::infrastructure::drivers::transports::Transport::set_write_rate_limit(
            &transport, limit,
        );
    }
    Ok(PluginIo::Stream {
        transport: std::sync::Arc::new(transport),
        usb: None,
    })
}

// `serial` is config-instantiated (the port comes from config, not a handle),
// so it carries only `open` — see `integration_scan`, which calls it directly.
inventory::submit!(PluginTransportDescriptor {
    kind: "serial",
    matches: None,
    open,
    id_suffix: None,
    validate: None,
});

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn manifest_with_serial() -> PluginManifest {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("serialtest");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: serialtest\ntype: integration\npermissions: [serial]\ntransports:\n  serial:\n    port_key: serial_port\n    baud: 115200\nconfig:\n  fields:\n    - { key: serial_port, label: Port, kind: serial_port }\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
        crate::domain::plugin::parse_manifest_from_dir(&dir).unwrap()
    }

    fn err_msg(r: Result<PluginIo>) -> String {
        match r {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    fn handle<'a>() -> DiscoveryHandle<'a> {
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
    fn open_requires_the_serial_permission() {
        let manifest = manifest_with_serial();
        let config = HashMap::from([(
            "serial_port".to_string(),
            crate::domain::plugin::ResolvedConfigValue::String("/dev/null".to_string()),
        )]);
        let err = err_msg(open(&manifest, &handle(), &config, &[], None));
        assert!(err.contains("serial"), "{err}");
    }

    #[test]
    fn open_errors_when_port_is_unset() {
        let manifest = manifest_with_serial();
        let err = err_msg(open(
            &manifest,
            &handle(),
            &HashMap::new(),
            &[Permission::Serial],
            None,
        ));
        assert!(err.contains("not set"), "{err}");
    }
}
