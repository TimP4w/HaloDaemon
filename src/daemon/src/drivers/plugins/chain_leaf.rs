// SPDX-License-Identifier: GPL-3.0-or-later
//! Generic chain-leaf child device — the de-vendored `NZXTFFan`. A plugin's
//! parent produces these from `discover_children`; each delegates RGB to the
//! parent's `ChainHub` (composited into one channel frame) and, if it has a
//! fan, delegates fan speed/duty to the parent's `FanHub`. It holds no transport
//! of its own — every call routes back to the parent.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use halod_shared::types::{DeviceType, RgbColor, RgbDescriptor, RgbState};

use crate::drivers::chain::ChainHub;
use crate::drivers::vendors::generic::devices::common::transformed_zone_frame;
use crate::drivers::{
    CapabilityRef, Device, FanCapability, FanHub, FanStateSlot, RgbCapability, RgbStateSlot,
    VisibilitySlot,
};

use super::manifest::AccessoryManifest;

pub struct ChainLeaf {
    id: String,
    name: String,
    vendor: String,
    /// Chain channel this leaf sits on (string id used with the `ChainHub`).
    channel_id: String,
    /// Numeric channel used for `FanHub` lookups.
    fan_channel: u8,
    has_fan: bool,
    rgb_descriptor: RgbDescriptor,
    rgb: RgbStateSlot,
    fan: FanStateSlot,
    visibility: VisibilitySlot,
    chain_hub: Arc<dyn ChainHub>,
    fan_hub: Arc<dyn FanHub>,
}

impl ChainLeaf {
    pub fn new(
        id: String,
        vendor: String,
        channel_id: String,
        fan_channel: u8,
        accessory: &AccessoryManifest,
        chain_hub: Arc<dyn ChainHub>,
        fan_hub: Arc<dyn FanHub>,
    ) -> Self {
        Self {
            id,
            name: accessory.name.clone(),
            vendor,
            channel_id,
            fan_channel,
            has_fan: accessory.fan,
            rgb_descriptor: accessory.rgb_descriptor(),
            rgb: RgbStateSlot::default(),
            fan: FanStateSlot::default(),
            visibility: VisibilitySlot::default(),
            chain_hub,
            fan_hub,
        }
    }

    async fn apply_state(&self, state: RgbState) -> Result<()> {
        let led_count = self.rgb_descriptor.zones[0].leds.len();
        let dev_id = self.id.clone();
        match &state {
            RgbState::Static { color } => {
                let colors = vec![*color; led_count];
                self.chain_hub
                    .write_chain_slice(&self.channel_id, &dev_id, &colors)
                    .await?;
            }
            RgbState::PerLed { zones } => {
                if let Some(leds) = zones.get("ring") {
                    let zone = &self.rgb_descriptor.zones[0];
                    let colors = transformed_zone_frame(zone, &self.rgb, leds);
                    self.chain_hub
                        .write_chain_slice(&self.channel_id, &dev_id, &colors)
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
impl Device for ChainLeaf {
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

    async fn close(&self) {}

    fn wire_device_type(&self) -> DeviceType {
        if self.has_fan {
            DeviceType::Fan
        } else {
            DeviceType::LedStrip
        }
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        let mut caps = vec![CapabilityRef::Rgb(self)];
        if self.has_fan {
            caps.push(CapabilityRef::Fan(self));
        }
        caps
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn debug_transport(&self) -> Option<&'static str> {
        Some("child")
    }
}

#[async_trait]
impl RgbCapability for ChainLeaf {
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
        if zone_id != "ring" {
            anyhow::bail!("unknown zone: {zone_id}");
        }
        self.chain_hub
            .write_chain_slice(&self.channel_id, &self.id, colors)
            .await
    }
}

#[async_trait]
impl FanCapability for ChainLeaf {
    fn fan_channel_id(&self) -> u8 {
        self.fan_channel
    }
    async fn fan_controllable(&self) -> bool {
        self.fan_hub
            .get_fan_controllable(self.fan_channel)
            .await
            .unwrap_or(false)
    }
    async fn get_duty(&self) -> Result<u8> {
        self.fan_hub.get_fan_duty(self.fan_channel).await
    }
    async fn set_duty(&self, duty: u8) -> Result<()> {
        self.fan_hub.set_fan_duty(self.fan_channel, duty).await
    }
    async fn get_rpm(&self) -> Option<u32> {
        self.fan_hub.get_fan_rpm(self.fan_channel).await.ok()
    }
    fn fan_state(&self) -> &FanStateSlot {
        &self.fan
    }
}
