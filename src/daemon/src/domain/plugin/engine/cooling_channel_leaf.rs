// SPDX-License-Identifier: GPL-3.0-or-later
//! Device projection for a controller-owned cooling channel.

use std::sync::Arc;

use anyhow::{ensure, Result};
use async_trait::async_trait;
use halod_shared::types::{CoolingChannel, DeviceType};

use crate::domain::device::{
    CapabilityRef, CoolingCapability, CoolingHub, CoolingStateSlot, Device, VisibilitySlot,
};

pub struct CoolingChannelLeaf {
    id: String,
    parent_id: String,
    vendor: String,
    channel: CoolingChannel,
    hub: Arc<dyn CoolingHub>,
    cooling: CoolingStateSlot,
    visibility: VisibilitySlot,
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
            cooling: CoolingStateSlot::default(),
            visibility: VisibilitySlot::default(),
        }
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
        &self.channel.name
    }
    fn vendor(&self) -> &str {
        &self.vendor
    }
    fn model(&self) -> &str {
        &self.channel.name
    }
    async fn initialize(&self) -> Result<bool> {
        Ok(true)
    }
    async fn close(&self) {}
    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Fan
    }
    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![CapabilityRef::Cooling(self)]
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
