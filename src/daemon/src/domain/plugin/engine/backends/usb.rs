// SPDX-License-Identifier: GPL-3.0-or-later
//! General endpoint-oriented USB plugin backend.

use anyhow::{bail, Result};
use halod_shared::types::{Permission, WriteRateLimit};
use std::sync::Arc;

use crate::domain::plugin::engine::transport::{PluginIo, PluginTransportDescriptor};
use crate::domain::plugin::manifest::{DeviceSpec, PluginManifest};
use crate::domain::registry::observers::discovery::DiscoveryHandle;
use crate::infrastructure::drivers::transports::usb::{UsbDevices, UsbSelector};

fn matches(spec: &DeviceSpec, handle: &DiscoveryHandle<'_>) -> bool {
    let DiscoveryHandle::UsbNonHid {
        vid,
        pid,
        interface_number,
        serial,
        ..
    } = handle
    else {
        return false;
    };
    spec.vid == Some(*vid)
        && (spec.pid == Some(*pid) || (!spec.pids.is_empty() && spec.pids.contains(pid)))
        && spec.interface == Some((*interface_number).into())
        && spec
            .serial
            .as_deref()
            .is_none_or(|want| *serial == Some(want))
}

pub(crate) fn open_usb(
    manifest: &PluginManifest,
    selector: UsbSelector,
    granted: &[Permission],
    limit: Option<WriteRateLimit>,
) -> Result<Arc<UsbDevices>> {
    if !granted.contains(&Permission::Usb) {
        bail!("USB permission was not granted");
    }
    let config = manifest
        .transports
        .usb
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("plugin declares no usb transport"))?;
    Ok(Arc::new(UsbDevices::open(
        selector,
        &config.devices,
        limit,
    )?))
}

fn open(
    manifest: &PluginManifest,
    handle: &DiscoveryHandle<'_>,
    _config: &crate::domain::plugin::ResolvedConfig,
    granted: &[Permission],
    limit: Option<WriteRateLimit>,
) -> Result<PluginIo> {
    let DiscoveryHandle::UsbNonHid {
        vid,
        pid,
        bus,
        address,
        port_path,
        serial,
        ..
    } = handle
    else {
        bail!("plugin '{}' matched a non-USB handle", manifest.plugin_id)
    };
    let selector = UsbSelector {
        vid: *vid,
        pid: *pid,
        bus: Some(*bus),
        address: Some(*address),
        port_path: port_path.to_vec(),
        serial: serial.map(str::to_owned),
        index: 0,
    };
    Ok(PluginIo::Usb(open_usb(manifest, selector, granted, limit)?))
}

fn id_suffix(handle: &DiscoveryHandle<'_>) -> String {
    match handle {
        DiscoveryHandle::UsbNonHid {
            bus,
            port_path,
            interface_number,
            serial: Some(serial),
            ..
        } => format!(
            "{}_b{bus}_p{}_if{interface_number}",
            serial,
            port_path
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join("_")
        ),
        DiscoveryHandle::UsbNonHid {
            bus,
            port_path,
            interface_number,
            ..
        } => format!(
            "b{bus}_p{}_if{interface_number}",
            port_path
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join("_")
        ),
        _ => "0".to_owned(),
    }
}

fn validate(spec: &DeviceSpec) -> Result<()> {
    if spec.vid.is_none() || (spec.pid.is_none() && spec.pids.is_empty()) {
        bail!("usb match requires vid and pid (or pids)");
    }
    Ok(())
}

pub(super) const DESCRIPTOR: PluginTransportDescriptor = PluginTransportDescriptor {
    kind: "usb",
    matches: Some(matches),
    open,
    id_suffix: Some(id_suffix),
    validate: Some(validate),
};
