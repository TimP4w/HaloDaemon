// SPDX-License-Identifier: GPL-3.0-or-later
//! AMD SMN plugin backend. Discovery is platform-specific; this descriptor
//! owns the permission check and typed broker opening.

use super::super::manifest::{DeviceSpec, PluginManifest};
use super::super::transport::{PluginIo, PluginTransportDescriptor};
use crate::registry::discovery::DiscoveryHandle;
use anyhow::{bail, Result};
use halod_shared::types::{Permission, WriteRateLimit};
use std::collections::HashMap;

#[cfg(target_os = "windows")]
fn matches(spec: &DeviceSpec, handle: &DiscoveryHandle<'_>) -> bool {
    spec.r#match.amd_smn.as_ref().is_some_and(|m| m.any)
        && matches!(handle, DiscoveryHandle::AmdSmn { .. })
}
#[cfg(not(target_os = "windows"))]
fn matches(_spec: &DeviceSpec, _handle: &DiscoveryHandle<'_>) -> bool {
    false
}
fn suffix(handle: &DiscoveryHandle<'_>) -> String {
    match handle {
        #[cfg(target_os = "windows")]
        DiscoveryHandle::AmdSmn { .. } => "cpu".into(),
        _ => "0".into(),
    }
}
fn open(
    _: &PluginManifest,
    _: &DiscoveryHandle<'_>,
    _: &HashMap<String, String>,
    granted: &[Permission],
    _: Option<WriteRateLimit>,
) -> Result<PluginIo> {
    if !granted.contains(&Permission::AmdSmn) {
        bail!("amd_smn transport requires the amd_smn permission");
    }
    #[cfg(target_os = "windows")]
    {
        Ok(PluginIo::AmdSmn(std::sync::Arc::new(
            crate::drivers::transports::amd_smn::AmdSmnBus::open()?,
        )))
    }
    #[cfg(not(target_os = "windows"))]
    {
        bail!("amd_smn is only available on Windows");
    }
}
fn validate(spec: &DeviceSpec) -> Result<()> {
    if !spec.r#match.amd_smn.as_ref().is_some_and(|m| m.any) {
        bail!("amd_smn match requires explicit any: true");
    }
    Ok(())
}
inventory::submit!(PluginTransportDescriptor {
    kind: "amd_smn",
    matches: Some(matches),
    open,
    id_suffix: Some(suffix),
    validate: Some(validate)
});
