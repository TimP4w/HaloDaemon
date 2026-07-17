// SPDX-License-Identifier: GPL-3.0-or-later
//! Typed LPCIO plugin backend.

use super::super::manifest::{DeviceSpec, PluginManifest};
use super::super::transport::{PluginIo, PluginTransportDescriptor};
use crate::registry::discovery::DiscoveryHandle;
use anyhow::{bail, Result};
use halod_shared::types::{Permission, WriteRateLimit};

#[cfg(target_os = "windows")]
fn matches(spec: &DeviceSpec, handle: &DiscoveryHandle<'_>) -> bool {
    let DiscoveryHandle::Lpcio { chip_id, .. } = handle else {
        return false;
    };
    spec.r#match
        .lpcio
        .as_ref()
        .is_some_and(|m| m.any || m.chip_ids.contains(chip_id))
}
#[cfg(not(target_os = "windows"))]
fn matches(_spec: &DeviceSpec, _handle: &DiscoveryHandle<'_>) -> bool {
    false
}
fn suffix(handle: &DiscoveryHandle<'_>) -> String {
    match handle {
        #[cfg(target_os = "windows")]
        DiscoveryHandle::Lpcio { slot, chip_id, .. } => format!("{chip_id:04x}-slot{slot}"),
        _ => "0".into(),
    }
}
fn open(
    _: &PluginManifest,
    _: &DiscoveryHandle<'_>,
    _: &crate::plugin::ResolvedConfig,
    granted: &[Permission],
    limit: Option<WriteRateLimit>,
) -> Result<PluginIo> {
    if !granted.contains(&Permission::Lpcio) {
        bail!("lpcio transport requires the lpcio permission");
    }
    #[cfg(target_os = "windows")]
    {
        Ok(PluginIo::Lpcio(std::sync::Arc::new(
            crate::drivers::transports::lpcio::LpcIoTransport::open(limit)?,
        )))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = limit;
        bail!("lpcio is only available on Windows");
    }
}
fn validate(spec: &DeviceSpec) -> Result<()> {
    let Some(m) = &spec.r#match.lpcio else {
        bail!("lpcio transport requires an lpcio match");
    };
    if m.any != m.chip_ids.is_empty() {
        bail!("lpcio match requires chip_ids or explicit any: true");
    }
    Ok(())
}
inventory::submit!(PluginTransportDescriptor {
    kind: "lpcio",
    matches: Some(matches),
    open,
    id_suffix: Some(suffix),
    validate: Some(validate)
});
