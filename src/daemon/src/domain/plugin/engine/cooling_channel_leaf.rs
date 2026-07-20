// SPDX-License-Identifier: GPL-3.0-or-later
//! Device projection for a controller-owned cooling channel.

use std::sync::Arc;

use anyhow::{ensure, Result};
use async_trait::async_trait;
use halod_shared::types::{CoolingChannel, CoolingChannelKind, DeviceType};

use crate::domain::device::{
    CapabilityRef, CoolingCapability, CoolingHub, CoolingStateSlot, Device, VisibilitySlot,
};

pub struct CoolingChannelLeaf {
    id: String,
    parent_id: String,
    vendor: String,
    channel: CoolingChannel,
    hub: Arc<dyn CoolingHub>,
    lighting: Option<Arc<dyn Device>>,
    cooling: CoolingStateSlot,
    visibility: VisibilitySlot,
}

fn device_type_for_channel(kind: &CoolingChannelKind) -> DeviceType {
    match kind {
        CoolingChannelKind::Fan => DeviceType::Fan,
        CoolingChannelKind::Pump => DeviceType::AIO,
    }
}

impl CoolingChannelLeaf {
    pub fn new(
        id: String,
        parent_id: String,
        vendor: String,
        channel: CoolingChannel,
        hub: Arc<dyn CoolingHub>,
    ) -> Self {
        Self {
            id,
            parent_id,
            vendor,
            channel,
            hub,
            lighting: None,
            cooling: CoolingStateSlot::default(),
            visibility: VisibilitySlot::default(),
        }
    }

    pub fn with_lighting(mut self, lighting: Arc<dyn Device>) -> Self {
        self.lighting = Some(lighting);
        self
    }

    fn projected(&self, mut channel: CoolingChannel) -> CoolingChannel {
        channel.id = "default".to_owned();
        channel.name = self.channel.name.clone();
        channel
    }
}

#[async_trait]
impl Device for CoolingChannelLeaf {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        self.lighting
            .as_ref()
            .map(|lighting| lighting.name())
            .unwrap_or(&self.channel.name)
    }
    fn vendor(&self) -> &str {
        &self.vendor
    }
    fn model(&self) -> &str {
        self.lighting
            .as_ref()
            .map(|lighting| lighting.model())
            .unwrap_or(&self.channel.name)
    }
    async fn initialize(&self) -> Result<bool> {
        Ok(true)
    }
    async fn close(&self) {}
    fn wire_device_type(&self) -> DeviceType {
        device_type_for_channel(&self.channel.kind)
    }
    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        let mut capabilities = vec![CapabilityRef::Cooling(self)];
        if let Some(lighting) = self
            .lighting
            .as_ref()
            .and_then(|device| device.as_lighting())
        {
            capabilities.push(CapabilityRef::Lighting(lighting));
        }
        capabilities
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockDevice;

    struct NoopCoolingHub;

    #[async_trait]
    impl CoolingHub for NoopCoolingHub {
        async fn get_cooling_status(&self, _channel: &str) -> Result<CoolingChannel> {
            anyhow::bail!("unused test hub")
        }

        async fn set_cooling_duty(&self, _channel: &str, _duty: u8) -> Result<()> {
            anyhow::bail!("unused test hub")
        }
    }

    #[test]
    fn cooling_channel_kind_determines_child_device_type() {
        assert_eq!(
            device_type_for_channel(&CoolingChannelKind::Fan),
            DeviceType::Fan
        );
        assert_eq!(
            device_type_for_channel(&CoolingChannelKind::Pump),
            DeviceType::AIO
        );
    }

    #[test]
    fn cooling_leaf_can_own_a_delegated_lighting_capability() {
        let lighting: Arc<dyn Device> = Arc::new(
            MockDevice::new("rgb_fan")
                .with_name("F120 RGB")
                .with_model("F120 RGB")
                .with_rgb(),
        );
        let leaf = CoolingChannelLeaf::new(
            "fan_leaf".into(),
            "controller".into(),
            "NZXT".into(),
            CoolingChannel {
                id: "fan1".into(),
                name: "Radiator fan".into(),
                kind: CoolingChannelKind::Fan,
                controllable: true,
                rpm: None,
                duty: None,
            },
            Arc::new(NoopCoolingHub),
        )
        .with_lighting(lighting);

        assert_eq!(leaf.name(), "F120 RGB");
        assert_eq!(leaf.model(), "F120 RGB");
        assert!(leaf.as_cooling().is_some());
        assert!(leaf.as_lighting().is_some());
        assert_eq!(leaf.capabilities().len(), 2);
    }
}

#[async_trait]
impl CoolingCapability for CoolingChannelLeaf {
    fn cooling_channels(&self) -> Vec<CoolingChannel> {
        vec![self.projected(self.channel.clone())]
    }

    async fn get_cooling_status(&self, channel_id: &str) -> Result<CoolingChannel> {
        ensure!(
            channel_id == "default",
            "unknown cooling channel: {channel_id}"
        );
        Ok(self.projected(self.hub.get_cooling_status(&self.channel.id).await?))
    }

    async fn set_cooling_duty(&self, channel_id: &str, duty: u8) -> Result<()> {
        ensure!(
            channel_id == "default",
            "unknown cooling channel: {channel_id}"
        );
        self.hub.set_cooling_duty(&self.channel.id, duty).await
    }

    fn cooling_state(&self) -> &CoolingStateSlot {
        &self.cooling
    }

    fn cached_cooling_status(&self) -> Vec<CoolingChannel> {
        self.hub
            .cached_cooling_status(&self.channel.id)
            .map(|channel| self.projected(channel))
            .into_iter()
            .collect()
    }
}
