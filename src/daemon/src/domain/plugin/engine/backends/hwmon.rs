// SPDX-License-Identifier: GPL-3.0-or-later
//! Linux hwmon integration transport backend.

use anyhow::{bail, Result};
use halod_shared::types::{Permission, WriteRateLimit};

use crate::domain::registry::observers::discovery::DiscoveryHandle;

use super::super::manifest::PluginManifest;
use super::super::transport::{PluginIo, PluginTransportDescriptor};

fn open(
    _: &PluginManifest,
    _: &DiscoveryHandle<'_>,
    _: &crate::domain::plugin::ResolvedConfig,
    granted: &[Permission],
    limit: Option<WriteRateLimit>,
) -> Result<PluginIo> {
    if !granted.contains(&Permission::Hwmon) {
        bail!("hwmon transport requires the hwmon permission");
    }
    #[cfg(target_os = "linux")]
    {
        Ok(PluginIo::Hwmon(std::sync::Arc::new(
            crate::infrastructure::drivers::transports::hwmon::HwmonTransport::discover(limit)?,
        )))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = limit;
        bail!("hwmon is only available on Linux");
    }
}

inventory::submit!(PluginTransportDescriptor {
    kind: "hwmon",
    matches: None,
    open,
    id_suffix: None,
    validate: None
});
