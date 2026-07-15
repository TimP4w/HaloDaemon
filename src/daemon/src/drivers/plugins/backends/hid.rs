// SPDX-License-Identifier: GPL-3.0-or-later
//! HID plugin transport backend: a byte-stream `Transport` opened from the
//! matched device's HID path.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Result};

use crate::drivers::plugins::manifest::{DeviceSpec, PluginManifest};
use crate::drivers::plugins::transport::{PluginIo, PluginTransportDescriptor};
use crate::drivers::transports::hid::HidTransport;
use crate::drivers::transports::usb::UsbSelector;
use crate::registry::discovery::DiscoveryHandle;

fn matches(spec: &DeviceSpec, handle: &DiscoveryHandle<'_>) -> bool {
    let DiscoveryHandle::Hid {
        vid,
        pid,
        usage_page,
        usage,
        interface_number,
        ..
    } = handle
    else {
        return false;
    };
    let pid_ok = if spec.pids.is_empty() {
        spec.pid.is_none_or(|p| p == *pid)
    } else {
        spec.pids.contains(pid)
    };
    (spec.generic_hid || spec.vid == Some(*vid))
        && pid_ok
        // Linux hidraw does not expose collection usages (both are zero), so
        // usage selectors only narrow the split collections on platforms that
        // actually report them.
        && spec
            .usage_page
            .is_none_or(|u| *usage_page == 0 || u == *usage_page)
        && spec.usage.is_none_or(|u| *usage == 0 || u == *usage)
        && spec.interface.is_none_or(|i| Some(i) == *interface_number)
}

fn open(
    manifest: &PluginManifest,
    handle: &DiscoveryHandle<'_>,
    _config: &HashMap<String, String>,
    granted: &[halod_shared::types::Permission],
    limit: Option<halod_shared::types::WriteRateLimit>,
) -> Result<PluginIo> {
    let DiscoveryHandle::Hid {
        path,
        vid,
        pid,
        serial,
        interface_number,
        ..
    } = handle
    else {
        bail!("plugin '{}' matched a non-HID handle", manifest.plugin_id);
    };
    let hid = manifest.transports.hid.clone().unwrap_or_default();
    // `report_size = 0` means raw passthrough (no report-id prepend, no padding):
    // the plugin builds the exact wire buffer itself (e.g. the Razer 90-byte report).
    let report_size = (hid.report_size != 0).then_some(hid.report_size);
    let transport = if let Some(companion) = &hid.companion {
        let api = hidapi::HidApi::new()?;
        let serial = serial.filter(|value| !value.is_empty());
        let companion_path = api
            .device_list()
            .filter(|device| device.vendor_id() == *vid && device.product_id() == *pid)
            .filter(|device| {
                interface_number.is_none_or(|interface| device.interface_number() == interface)
            })
            .filter(|device| serial.is_none_or(|value| device.serial_number() == Some(value)))
            .find(|device| {
                device.usage_page() == companion.usage_page && device.usage() == companion.usage
            })
            .map(|device| device.path().to_string_lossy().into_owned());

        HidTransport::open_dual(
            path,
            companion_path.as_deref().unwrap_or(""),
            report_size,
            hid.timeout_ms,
            hid.feature_report,
            limit,
        )?
    } else {
        HidTransport::open(path, report_size, hid.timeout_ms, hid.feature_report, limit)?
    };
    let usb = if manifest.transports.usb.is_some() {
        Some(super::usb::open_usb(
            manifest,
            UsbSelector {
                vid: *vid,
                pid: *pid,
                serial: serial.map(str::to_owned),
                index: handle_idx(handle),
                ..Default::default()
            },
            granted,
            limit,
        )?
            as Arc<dyn crate::drivers::transports::usb::UsbCollection>)
    } else {
        None
    };
    Ok(PluginIo::Stream {
        transport: Arc::new(transport),
        usb,
    })
}

fn handle_idx(handle: &DiscoveryHandle<'_>) -> usize {
    match handle {
        DiscoveryHandle::Hid { idx, .. } => *idx,
        _ => 0,
    }
}

fn id_suffix(handle: &DiscoveryHandle<'_>) -> String {
    match handle {
        DiscoveryHandle::Hid {
            serial: Some(s), ..
        } => (*s).to_owned(),
        DiscoveryHandle::Hid { idx, .. } => idx.to_string(),
        _ => "0".to_owned(),
    }
}

fn validate(spec: &DeviceSpec) -> Result<()> {
    if !spec.generic_hid && spec.vid.is_none() {
        bail!("hid match requires a `vid`");
    }
    Ok(())
}

inventory::submit!(PluginTransportDescriptor {
    kind: "hid",
    matches: Some(matches),
    open,
    id_suffix: Some(id_suffix),
    validate: Some(validate),
});
