// SPDX-License-Identifier: GPL-3.0-or-later
//! Scoped direct command execution for command-backed plugins.

use anyhow::{bail, Result};
use halod_shared::types::{Permission, WriteRateLimit};

use crate::domain::registry::observers::discovery::DiscoveryHandle;

use super::super::manifest::{DeviceSpec, PluginManifest};
use super::super::transport::{
    accept_spec, CommandExecutor, Discovery, PluginIo, PluginTransportDescriptor,
};
use crate::domain::plugin::ResolvedConfig;

fn matches(spec: &DeviceSpec, handle: &DiscoveryHandle<'_>) -> bool {
    matches!(handle, DiscoveryHandle::Command { executable } if spec.command().is_some_and(|m| m.command() == *executable))
}

fn single_instance(_: &DiscoveryHandle<'_>) -> String {
    "0".to_owned()
}

fn open(
    manifest: &PluginManifest,
    _: &DiscoveryHandle<'_>,
    _: &ResolvedConfig,
    granted: &[Permission],
    _: Option<WriteRateLimit>,
) -> Result<PluginIo> {
    if !granted.contains(&Permission::Command) {
        bail!("command transport requires the command permission");
    }
    let config =
        manifest.transports.command.as_ref().ok_or_else(|| {
            anyhow::anyhow!("command match has no command transport configuration")
        })?;
    Ok(PluginIo::Command(CommandExecutor::new(
        config.commands.clone(),
    )))
}

pub(super) const DESCRIPTOR: PluginTransportDescriptor = PluginTransportDescriptor {
    kind: "command",
    open,
    discovery: Some(Discovery {
        matches,
        id_suffix: single_instance,
        validate: accept_spec,
    }),
};
