// SPDX-License-Identifier: GPL-3.0-or-later
//! Scoped direct command execution for command-backed plugins.

use anyhow::{bail, Result};
use halod_shared::types::{Permission, WriteRateLimit};

use crate::domain::registry::observers::discovery::DiscoveryHandle;

use super::super::manifest::{DeviceSpec, PluginManifest};
use super::super::transport::{CommandExecutor, PluginIo, PluginTransportDescriptor};

fn matches(spec: &DeviceSpec, handle: &DiscoveryHandle<'_>) -> bool {
    matches!(handle, DiscoveryHandle::Command { executable } if spec.r#match.command.as_ref().is_some_and(|m| m.command() == *executable))
}

fn open(
    manifest: &PluginManifest,
    _: &DiscoveryHandle<'_>,
    _: &crate::domain::plugin::ResolvedConfig,
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

inventory::submit! {
    PluginTransportDescriptor {
        kind: "command",
        matches: Some(matches),
        open,
        id_suffix: None,
        validate: Some(validate),
    }
}

fn validate(spec: &DeviceSpec) -> Result<()> {
    if spec.r#match.command.is_none() {
        bail!("command transport requires a command match");
    }
    Ok(())
}
