// SPDX-License-Identifier: GPL-3.0-or-later
//! USB vendor-control plugin transport backend: opens the matched `UsbNonHid`
//! device (DDC/CI monitor, ENE RGB controller, …) and any secondary control
//! devices the manifest declares, exposing them all as named endpoints.
//!
//! Control transfers open by VID/PID directly (no enumerated device path), so a
//! single plugin can bundle several physical USB devices — the mechanism behind
//! presenting a monitor's DDC controller and its Ambiglow LED controller as one
//! merged device.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Result};

use crate::drivers::plugins::manifest::{DeviceSpec, PluginManifest};
use crate::drivers::plugins::transport::{ControlEndpoints, PluginIo, PluginTransportDescriptor};
use crate::drivers::transports::usb_control::UsbControlTransport;
use crate::drivers::transports::ControlTransport;
use crate::registry::discovery::DiscoveryHandle;

fn matches(spec: &DeviceSpec, handle: &DiscoveryHandle<'_>) -> bool {
    let DiscoveryHandle::UsbNonHid { vid, pid } = handle else {
        return false;
    };
    let pid_ok = if spec.pids.is_empty() {
        spec.pid.is_none_or(|p| p == *pid)
    } else {
        spec.pids.contains(pid)
    };
    spec.vid == Some(*vid) && pid_ok
}

fn open(
    manifest: &PluginManifest,
    handle: &DiscoveryHandle<'_>,
    _config: &HashMap<String, String>,
) -> Result<PluginIo> {
    let DiscoveryHandle::UsbNonHid { vid, pid } = handle else {
        bail!(
            "plugin '{}' matched a non-USB-control handle",
            manifest.plugin_id
        );
    };
    let cfg = manifest.transports.usb_control.clone().unwrap_or_default();

    let mut endpoints: HashMap<String, Arc<dyn ControlTransport>> = HashMap::new();
    let primary = UsbControlTransport::open(*vid, *pid, cfg.interface, None)?;
    endpoints.insert(ControlEndpoints::PRIMARY.to_owned(), Arc::new(primary));

    // Secondary devices this plugin bundles (e.g. the Ambiglow LED controller):
    // opened by their declared VID/PID and reached from Lua by their id.
    for ep in &cfg.endpoints {
        let t = UsbControlTransport::open(ep.vid, ep.pid, ep.interface, None)?;
        endpoints.insert(ep.id.clone(), Arc::new(t));
    }

    Ok(PluginIo::Control(ControlEndpoints::new(endpoints)))
}

fn id_suffix(handle: &DiscoveryHandle<'_>) -> String {
    match handle {
        DiscoveryHandle::UsbNonHid { vid, pid } => format!("{vid:04x}_{pid:04x}"),
        _ => "0".to_owned(),
    }
}

fn validate(spec: &DeviceSpec) -> Result<()> {
    if spec.vid.is_none() {
        bail!("usb_control match requires a `vid`");
    }
    if spec.pid.is_none() && spec.pids.is_empty() {
        bail!("usb_control match requires a `pid` (or `pids`)");
    }
    Ok(())
}

inventory::submit!(PluginTransportDescriptor {
    kind: "usb_control",
    matches,
    open,
    id_suffix,
    validate,
});
