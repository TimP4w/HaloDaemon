// SPDX-License-Identifier: GPL-3.0-or-later
//! Generic integration-leaf child device — one top-level `Device` per
//! controller a config-instantiated integration plugin's `enumerate_controllers`
//! reports (e.g. one OpenRGB controller). Unlike `ChainLeaf` (which composes
//! several children into one shared chain frame), each leaf here is addressed
//! independently by its controller `index` and owns its own worker (connection
//! + Lua VM), so writes to different controllers run in parallel and a slow
//! controller can't stall the others.

use anyhow::Result;
use async_trait::async_trait;

use halod_shared::types::{DeviceType, RgbColor, RgbDescriptor, RgbState};

use super::worker::PluginHandle;
use crate::drivers::vendors::generic::devices::common::transformed_zone_frame;
use crate::drivers::{CapabilityRef, Device, RgbCapability, RgbStateSlot, VisibilitySlot};

pub struct IntegrationLeaf {
    id: String,
    name: String,
    vendor: String,
    index: u32,
    rgb_descriptor: RgbDescriptor,
    rgb: RgbStateSlot,
    visibility: VisibilitySlot,
    /// This controller's own worker (connection + Lua VM).
    worker: PluginHandle,
}

impl IntegrationLeaf {
    pub fn new(
        id: String,
        name: String,
        vendor: String,
        index: u32,
        rgb_descriptor: RgbDescriptor,
        worker: PluginHandle,
    ) -> Self {
        Self {
            id,
            name,
            vendor,
            index,
            rgb_descriptor,
            rgb: RgbStateSlot::default(),
            visibility: VisibilitySlot::default(),
            worker,
        }
    }

    async fn apply_state(&self, state: RgbState) -> Result<()> {
        match &state {
            RgbState::Static { color } => {
                for zone in &self.rgb_descriptor.zones {
                    let colors = vec![*color; zone.leds.len()];
                    self.worker
                        .write_controller_frame(self.index, &zone.id, &colors)
                        .await?;
                }
            }
            RgbState::PerLed { zones } => {
                for zone in &self.rgb_descriptor.zones {
                    let Some(leds) = zones.get(&zone.id) else {
                        continue;
                    };
                    let colors = transformed_zone_frame(zone, &self.rgb, leds);
                    self.worker
                        .write_controller_frame(self.index, &zone.id, &colors)
                        .await?;
                }
            }
            RgbState::NativeEffect { .. } | RgbState::DirectEffect { .. } | RgbState::Engine => {}
        }
        self.rgb.set_state(Some(state));
        Ok(())
    }
}

#[async_trait]
impl Device for IntegrationLeaf {
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
        &self.name
    }

    async fn initialize(&self) -> Result<bool> {
        Ok(true)
    }

    async fn close(&self) {
        self.worker.close().await;
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::LedStrip
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![CapabilityRef::Rgb(self)]
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn debug_transport(&self) -> Option<&'static str> {
        Some("child")
    }
}

#[async_trait]
impl RgbCapability for IntegrationLeaf {
    fn descriptor(&self) -> &RgbDescriptor {
        &self.rgb_descriptor
    }
    fn rgb_state(&self) -> &RgbStateSlot {
        &self.rgb
    }
    async fn apply(&self, state: RgbState) -> Result<()> {
        self.apply_state(state).await
    }
    async fn write_frame(&self, zone_id: &str, colors: &[RgbColor]) -> Result<()> {
        if !self.rgb_descriptor.zones.iter().any(|z| z.id == zone_id) {
            anyhow::bail!("unknown zone: {zone_id}");
        }
        self.worker
            .write_controller_frame(self.index, zone_id, colors)
            .await
    }
}
