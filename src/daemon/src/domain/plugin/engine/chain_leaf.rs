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

use crate::domain::device::chain::LightingDivisionHub;
use crate::domain::device::{
    CapabilityRef, CoolingCapability, CoolingHub, CoolingStateSlot, Device, LightingCapability,
    LightingStateSlot, VisibilitySlot,
};
use crate::infrastructure::drivers::vendors::generic::devices::common::transformed_zone_frame;

use super::manifest::AccessoryManifest;

pub struct ChainLeaf {
    id: String,
    parent_id: String,
    name: String,
    vendor: String,
    /// Chain channel this leaf sits on (string id used with the `LightingDivisionHub`).
    channel_id: String,
    /// Parent cooling channel this leaf owns, as declared by the chainable
    /// channel. `None` leaves the leaf lighting-only.
    fan_channel: Option<String>,
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
        parent_id: String,
        vendor: String,
        channel_id: String,
        fan_channel: Option<String>,
        accessory: &AccessoryManifest,
        chain_hub: Arc<dyn LightingDivisionHub>,
        cooling_hub: Arc<dyn CoolingHub>,
    ) -> Self {
        Self {
            id,
            parent_id,
            name: accessory.name.clone(),
            vendor,
            channel_id,
            // Both sides must agree: the hardware reports a fan and the plugin
            // declares which cooling channel this output hands over.
            fan_channel: fan_channel.filter(|_| accessory.fan),
            rgb_descriptor: accessory.rgb_descriptor(),
            rgb: LightingStateSlot::default(),
            cooling: CoolingStateSlot::default(),
            visibility: VisibilitySlot::default(),
            chain_hub,
            cooling_hub,
        }
    }

    /// Resolve this leaf's single cooling channel back to the parent channel it
    /// was handed.
    fn source_channel(&self, channel_id: &str) -> Result<&str> {
        anyhow::ensure!(channel_id == "fan", "unknown cooling channel: {channel_id}");
        self.fan_channel
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("{} owns no cooling channel", self.id))
    }

    async fn apply_state(&self, state: LightingState) -> Result<()> {
        let Some(zone) = self.rgb_descriptor.channels.first() else {
            anyhow::bail!("chain leaf has no lighting channel");
        };
        let led_count = zone.leds.len();
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
        if self.fan_channel.is_some() {
            DeviceType::Fan
        } else {
            DeviceType::LedStrip
        }
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        let mut caps = vec![CapabilityRef::Lighting(self)];
        if self.fan_channel.is_some() {
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

    fn state_source_id(&self) -> Option<&str> {
        Some(&self.parent_id)
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
        self.fan_channel
            .iter()
            .map(|_| CoolingChannel {
                id: "fan".to_string(),
                name: self.name.clone(),
                kind: CoolingChannelKind::Fan,
                controllable: true,
                rpm: None,
                duty: None,
                visibility: Default::default(),
            })
            .collect()
    }
    async fn get_cooling_status(&self, channel_id: &str) -> Result<CoolingChannel> {
        let status = self
            .cooling_hub
            .get_cooling_status(self.source_channel(channel_id)?)
            .await?;
        Ok(CoolingChannel {
            id: "fan".to_string(),
            name: self.name.clone(),
            kind: CoolingChannelKind::Fan,
            controllable: status.controllable,
            rpm: status.rpm,
            duty: status.duty,
            visibility: Default::default(),
        })
    }
    async fn set_cooling_duty(&self, channel_id: &str, duty: u8) -> Result<()> {
        self.cooling_hub
            .set_cooling_duty(self.source_channel(channel_id)?, duty)
            .await
    }
    fn cooling_state(&self) -> &CoolingStateSlot {
        &self.cooling
    }

    fn cached_cooling_status(&self) -> Vec<CoolingChannel> {
        self.fan_channel
            .as_deref()
            .and_then(|source| self.cooling_hub.cached_cooling_status(source))
            .map(|status| CoolingChannel {
                id: "fan".to_string(),
                name: self.name.clone(),
                kind: CoolingChannelKind::Fan,
                controllable: status.controllable,
                rpm: status.rpm,
                duty: status.duty,
                visibility: Default::default(),
            })
            .into_iter()
            .collect()
    }
}
