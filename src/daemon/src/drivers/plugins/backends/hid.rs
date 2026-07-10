// SPDX-License-Identifier: GPL-3.0-or-later
//! HID plugin transport backend: a byte-stream `Transport` opened from the
//! matched device's HID path.

use std::sync::Arc;

use anyhow::{bail, Result};

use crate::drivers::plugins::manifest::{MatchSpec, PluginManifest};
use crate::drivers::plugins::transport::{BulkEndpoint, PluginIo, PluginTransportDescriptor};
use crate::drivers::transports::hid::HidTransport;
use crate::registry::discovery::DiscoveryHandle;

fn matches(spec: &MatchSpec, handle: &DiscoveryHandle<'_>) -> bool {
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
    spec.vid == Some(*vid)
        && pid_ok
        && spec.usage_page.is_none_or(|u| u == *usage_page)
        && spec.usage.is_none_or(|u| u == *usage)
        && spec.interface.is_none_or(|i| Some(i) == *interface_number)
}

fn open(manifest: &PluginManifest, handle: &DiscoveryHandle<'_>) -> Result<PluginIo> {
    let DiscoveryHandle::Hid { path, vid, pid, .. } = handle else {
        bail!("plugin '{}' matched a non-HID handle", manifest.plugin_id);
    };
    let hid = manifest.transports.hid.clone().unwrap_or_default();
    // `report_size = 0` means raw passthrough (no report-id prepend, no padding):
    // the plugin builds the exact wire buffer itself (e.g. the Razer 90-byte report).
    let report_size = (hid.report_size != 0).then_some(hid.report_size);
    let transport =
        HidTransport::open(path, report_size, hid.timeout_ms, hid.feature_report, None)?;
    Ok(PluginIo::Stream {
        transport: Arc::new(transport),
        // Lazy companion bulk endpoint (opened only if the plugin streams LCD).
        bulk: Some(BulkEndpoint::new(*vid, *pid)),
    })
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

fn validate(spec: &MatchSpec) -> Result<()> {
    if spec.vid.is_none() {
        bail!("hid match requires a `vid`");
    }
    Ok(())
}

inventory::submit!(PluginTransportDescriptor {
    kind: "hid",
    matches,
    open,
    id_suffix,
    validate,
});
