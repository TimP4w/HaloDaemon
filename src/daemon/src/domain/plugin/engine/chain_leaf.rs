// SPDX-License-Identifier: GPL-3.0-or-later
//! Generic chain-leaf child device — the de-vendored `NZXTFFan`. A plugin's
//! parent produces these from `discover_children`; each delegates RGB to the
//! parent's `LightingDivisionHub` (composited into one channel frame) and, if it has a
//! fan, delegates cooling to the parent's `CoolingHub`. It holds no transport
//! of its own — every call routes back to the parent.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use halod_shared::types::{
    CoolingChannel, CoolingChannelKind, DeviceType, LightingDescriptor, LightingState, RgbColor,
};

use crate::infrastructure::drivers::chain::LightingDivisionHub;
use crate::infrastructure::drivers::vendors::generic::devices::common::transformed_zone_frame;
use crate::infrastructure::drivers::{
    CapabilityRef, CoolingCapability, CoolingHub, CoolingStateSlot, Device, LightingCapability,
    LightingStateSlot, VisibilitySlot,
};

use super::manifest::AccessoryManifest;

pub struct ChainLeaf {
    id: String,
    name: String,
    vendor: String,
    /// Chain channel this leaf sits on (string id used with the `LightingDivisionHub`).
    channel_id: String,
    /// Numeric parent cooling channel.
    fan_channel: u8,
    has_fan: bool,
    rgb_descriptor: LightingDescriptor,
    rgb: LightingStateSlot,
    cooling: CoolingStateSlot,
    visibility: VisibilitySlot,
    chain_hub: Arc<dyn LightingDivisionHub>,
    cooling_hub: Arc<dyn CoolingHub>,
}

impl ChainLeaf {
    pub fn new(
        id: String,
        vendor: String,
        channel_id: String,
        fan_channel: u8,
        accessory: &AccessoryManifest,
        chain_hub: Arc<dyn LightingDivisionHub>,
        cooling_hub: Arc<dyn CoolingHub>,
    ) -> Self {
        Self {
            id,
            name: accessory.name.clone(),
            vendor,
            channel_id,
            fan_channel,
            has_fan: accessory.fan,
            rgb_descriptor: accessory.rgb_descriptor(),
            rgb: LightingStateSlot::default(),
            cooling: CoolingStateSlot::default(),
            visibility: VisibilitySlot::default(),
            chain_hub,
            cooling_hub,
        }
    }

    async fn apply_state(&self, state: LightingState) -> Result<()> {
        let led_count = self.rgb_descriptor.channels[0].leds.len();
        let dev_id = self.id.clone();
        match &state {
            LightingState::Static { color } => {
                let colors = vec![*color; led_count];
                self.chain_hub
                    .write_chain_slice(&self.channel_id, &dev_id, &colors)
                    .await?;
            }
            LightingState::PerLed { channels } => {
                if let Some(leds) = channels.get("ring") {
                    let zone = &self.rgb_descriptor.channels[0];
                    let colors = transformed_zone_frame(zone, &self.rgb, leds);
                    self.chain_hub
                        .write_chain_slice(&self.channel_id, &dev_id, &colors)
                        .await?;
                }
            }
            LightingState::NativeEffect { .. }
            | LightingState::DirectEffect { .. }
            | LightingState::Engine => {}
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
        let mut caps = vec![CapabilityRef::Lighting(self)];
        if self.has_fan {
            caps.push(CapabilityRef::Cooling(self));
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
impl LightingCapability for ChainLeaf {
    fn descriptor(&self) -> &LightingDescriptor {
        &self.rgb_descriptor
    }
    fn lighting_state(&self) -> &LightingStateSlot {
        &self.rgb
    }
    async fn apply(&self, state: LightingState) -> Result<()> {
        self.apply_state(state).await
    }
    async fn write_frame(&self, channel_id: &str, bytes: &[u8]) -> Result<()> {
        if channel_id != "ring" {
            anyhow::bail!("unknown zone: {channel_id}");
        }
        anyhow::ensure!(
            bytes.len().is_multiple_of(3),
            "invalid lighting frame length"
        );
        let colors: Vec<_> = bytes
            .chunks_exact(3)
            .map(|chunk| RgbColor {
                r: chunk[0],
                g: chunk[1],
                b: chunk[2],
            })
            .collect();
        self.chain_hub
            .write_chain_slice(&self.channel_id, &self.id, &colors)
            .await
    }
}

#[async_trait]
impl CoolingCapability for ChainLeaf {
    fn cooling_channels(&self) -> Vec<CoolingChannel> {
        vec![CoolingChannel {
            id: "fan".to_string(),
            name: self.name.clone(),
            kind: CoolingChannelKind::Fan,
            controllable: true,
            rpm: None,
            duty: None,
        }]
    }
    async fn get_cooling_status(&self, channel_id: &str) -> Result<CoolingChannel> {
        anyhow::ensure!(channel_id == "fan", "unknown cooling channel: {channel_id}");
        let status = self
            .cooling_hub
            .get_cooling_status(self.fan_channel)
            .await?;
        Ok(CoolingChannel {
            id: "fan".to_string(),
            name: self.name.clone(),
            kind: CoolingChannelKind::Fan,
            controllable: status.controllable,
            rpm: status.rpm,
            duty: status.duty,
        })
    }
    async fn set_cooling_duty(&self, channel_id: &str, duty: u8) -> Result<()> {
        anyhow::ensure!(channel_id == "fan", "unknown cooling channel: {channel_id}");
        self.cooling_hub
            .set_cooling_duty(self.fan_channel, duty)
            .await
    }
    fn cooling_state(&self) -> &CoolingStateSlot {
        &self.cooling
    }
}
