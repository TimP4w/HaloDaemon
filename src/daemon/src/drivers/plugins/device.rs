// SPDX-License-Identifier: GPL-3.0-or-later
//! The generic device a plugin instantiates. Step 1 is `Device`-only (identity
//! + lifecycle); capability forwarding into the Lua worker lands in later steps.

use anyhow::Result;
use async_trait::async_trait;

use crate::drivers::{CapabilityRef, Device};

use super::manifest::PluginManifest;

/// A device whose behaviour is defined by a plugin script rather than native
/// Rust. It sits behind the same `Device` seam as every native driver.
pub struct LuaDevice {
    id: String,
    name: String,
    vendor: String,
    model: String,
    /// The plugin this device came from — used by management (enable/disable).
    plugin_id: String,
}

impl LuaDevice {
    pub fn new(id: String, manifest: &PluginManifest) -> Self {
        Self {
            id,
            name: manifest.display_name().to_owned(),
            vendor: manifest.identity.vendor.clone(),
            model: manifest.identity.model.clone(),
            plugin_id: manifest.plugin_id.clone(),
        }
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }
}

#[async_trait]
impl Device for LuaDevice {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn vendor(&self) -> &str {
        &self.vendor
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn initialize(&self) -> Result<bool> {
        Ok(true)
    }

    async fn close(&self) {}

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        Vec::new()
    }
}
