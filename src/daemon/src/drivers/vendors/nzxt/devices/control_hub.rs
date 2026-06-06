// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2021 Aleksandr Mezin <mezin.alexander@gmail.com>
use crate::{
    discovery::{DeviceDescriptor, DiscoveryHandle},
    drivers::{
        chain::{ChainAdapter, ChainHost, ChannelDescriptor},
        vendors::generic::devices::common::{build_device_id, stable_serial},
        vendors::nzxt::protocols::nzxt_control_hub::{NzxtControlHubProtocol, FAN_CHANNELS, MAX_NZXT_CHAIN_LEDS},
        transports::hid::HidTransport,
        CapabilityRef, ChainCapability, ChainLinkKind, Controller, Device, NzxtFanHub,
    },
    state::AppState,
};
/// Protocol reference: Linux kernel nzxt-smart2 driver by Aleksandr Mezin (GPL-2.0-or-later)
///   https://github.com/torvalds/linux/blob/master/drivers/hwmon/nzxt-smart2.c
///
use anyhow::Result;
use async_trait::async_trait;
use halod_protocol::types::{DeviceType, RgbColor};
use std::sync::{Arc, OnceLock, Weak};

inventory::submit! {
    DeviceDescriptor {
        matches: |h| matches!(h, DiscoveryHandle::Hid { vid: 0x1E71, pid: 0x2022, .. }),
        make: |h| {
            let DiscoveryHandle::Hid { path, serial, idx, .. } = h else {
                anyhow::bail!("descriptor matched non-HID handle");
            };
            NZXTControlHub::new(path, serial, idx).map(|arc| arc as Arc<dyn Device>)
        },
    }
}

pub struct NZXTControlHub {
    /// Built via `Arc::new_cyclic` in `new` so we can hand the parent (= the
    /// `ChainAdapter` impl) to a `ChainHost` without cloning the device.
    self_ref: Weak<Self>,
    id: String,
    serial_number: Option<String>,
    protocol: NzxtControlHubProtocol<HidTransport>,
    /// Shared chain runtime. Built in `new()` since the channel set is
    /// known up front (5 fixed channels on the hub).
    chain_host: OnceLock<Arc<ChainHost>>,
}

impl NZXTControlHub {
    pub fn new(path: &str, serial: Option<&str>, index: usize) -> Result<Arc<Self>> {
        let protocol = NzxtControlHubProtocol::open(path)?;
        let id = build_device_id("nzxt_hub", serial, index);
        let serial_number = stable_serial(serial);
        let arc = Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            id,
            serial_number,
            protocol,
            chain_host: OnceLock::new(),
        });
        // Channel layout is fixed for this device — seed the host now so
        // `chainable_channels` populates immediately, even before discovery.
        let host = ChainHost::new(arc.clone(), ChainLinkKind::GenericNzxtArgb);
        let _ = arc.chain_host.set(host);
        Ok(arc)
    }

    fn arc_self_chain_hub(&self) -> Arc<dyn crate::drivers::chain::ChainHub> {
        self.chain_host
            .get()
            .expect("chain_host not yet set")
            .clone()
    }

    fn arc_self_fan_hub(&self) -> Arc<dyn NzxtFanHub> {
        self.self_ref
            .upgrade()
            .expect("arc_self_fan_hub called after device drop")
    }
}

#[async_trait]
impl Device for NZXTControlHub {
    fn id(&self) -> String { self.id.clone() }
    fn name(&self) -> &str { "NZXT Control Hub" }
    fn vendor(&self) -> &str { "NZXT" }
    fn model(&self) -> &str { "Control Hub" }

    async fn initialize(&self) -> Result<bool> {
        self.protocol.detect_fans().await;
        let fw = self.protocol.base.get_firmware_version().await?;
        log::info!("[NZXT Control Hub] Initialized firmware version: {}", fw);
        Ok(true)
    }

    async fn close(&self) {
        self.protocol.poll_task.lock().await.take();
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Hub
    }

    fn wire_serial_number(&self) -> Option<String> {
        self.serial_number.clone()
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![CapabilityRef::Controller(self), CapabilityRef::Chain(self)]
    }
}

#[async_trait]
impl Controller for NZXTControlHub {
    async fn discover_children(&self, app: Arc<AppState>) -> Vec<Arc<dyn Device>> {
        log::debug!("[NZXT Control Hub] Discovering accessories...");
        let accessories = self
            .protocol
            .base
            .detect_accessories()
            .await
            .unwrap_or_default();
        let mut result = Vec::new();

        for accessory in accessories {
            log::debug!(
                "[NZXT Control Hub] Detected accessory: Channel {}, Accessory ID {}",
                accessory.channel_id,
                accessory.accessory_id
            );
            let handle = DiscoveryHandle::NzxtChain {
                channel_id: accessory.channel_id,
                accessory_id: accessory.accessory_id,
                chain_hub: self.arc_self_chain_hub(),
                fan_hub: self.arc_self_fan_hub(),
            };
            let Some(impl_) = crate::discovery::make_device(handle) else {
                log::warn!(
                    "[NZXT Control Hub] Unrecognized accessory: Channel {}, Accessory ID {}",
                    accessory.channel_id,
                    accessory.accessory_id
                );
                continue;
            };
            if crate::usecases::registration::register_device(&app, impl_.clone()).await {
                log::debug!(
                    "[NZXT Control Hub] Initialized accessory: Channel {}, Accessory ID {}",
                    accessory.channel_id,
                    accessory.accessory_id
                );
                if let Some(host) = self.chain_host.get() {
                    host.register_auto_link(&accessory.channel_id.to_string(), impl_.clone())
                        .await;
                }
                result.push(impl_);
            }
        }
        // Start polling only after accessory discovery so the poll loop
        // doesn't race with detect_accessories() on the same transport read.
        self.protocol.start_polling(1000).await;

        result
    }
}

#[async_trait]
impl ChainAdapter for NZXTControlHub {
    fn parent_id(&self) -> String {
        self.id.clone()
    }

    fn channels(&self) -> Vec<ChannelDescriptor> {
        (0..FAN_CHANNELS as u8)
            .map(|ch| ChannelDescriptor {
                channel_id: ch.to_string(),
                display_name: format!("Channel {}", ch + 1),
                max_leds: MAX_NZXT_CHAIN_LEDS,
            })
            .collect()
    }

    async fn write_composed_frame(&self, channel_id: &str, composed: &[RgbColor]) -> Result<()> {
        let channel: u8 = channel_id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid channel id: {channel_id}"))?;
        self.protocol.write_rgb_frame(channel, composed).await
    }
}

#[async_trait]
impl NzxtFanHub for NZXTControlHub {
    fn id(&self) -> String {
        self.id.clone()
    }

    async fn get_fan_rpm(&self, channel: &u8) -> Result<u32> {
        Ok(self.protocol.read_fan_rpm(*channel).await)
    }

    async fn get_fan_duty(&self, channel: &u8) -> Result<u8> {
        Ok(self.protocol.read_fan_duty(*channel).await)
    }

    async fn get_fan_controllable(&self, channel: &u8) -> Result<bool> {
        Ok(self.protocol.read_fan_controllable(*channel).await)
    }

    async fn set_fan_duty(&self, channel: &u8, duty: u8) -> Result<()> {
        self.protocol.write_fan_duty(*channel, duty).await
    }
}

impl ChainCapability for NZXTControlHub {
    fn chain_host(&self) -> Option<&Arc<ChainHost>> {
        self.chain_host.get()
    }
}

#[cfg(test)]
mod tests {
    use crate::drivers::vendors::nzxt::protocols::NzxtBaseProtocol;
    use crate::drivers::transports::mock::test_transport::MockTransport;

    use super::*;
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    fn protocol(responses: Vec<Vec<u8>>) -> NzxtControlHubProtocol<MockTransport> {
        NzxtControlHubProtocol {
            base: NzxtBaseProtocol::new(MockTransport::new(responses)),
            poll_task: Arc::new(Mutex::new(None)),
            fan_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn capturing_protocol() -> NzxtControlHubProtocol<MockTransport> {
        protocol(vec![])
    }

    // --- fan status parsing ---

    #[test]
    fn parse_fan_status_speed_returns_values() {
        let mut pkt = vec![0u8; 48];
        pkt[0] = 0x67;
        pkt[1] = 0x02;
        // channel 0: rpm=1200 (0x04B0 LE), duty=50
        pkt[24] = 0xB0;
        pkt[25] = 0x04;
        pkt[40] = 50;
        // channel 1: rpm=800 (0x0320 LE), duty=30
        pkt[26] = 0x20;
        pkt[27] = 0x03;
        pkt[41] = 30;
        // channel 2: rpm=0, duty=0 (already zero)
        let result = NzxtControlHubProtocol::<MockTransport>::parse_fan_status_speed(&pkt).unwrap();
        assert_eq!(result[0], (1200, 50));
        assert_eq!(result[1], (800, 30));
        assert_eq!(result[2], (0, 0));
    }

    #[test]
    fn parse_fan_status_speed_rejects_wrong_report_id() {
        let mut pkt = vec![0u8; 48];
        pkt[0] = 0x61;
        pkt[1] = 0x02;
        assert!(NzxtControlHubProtocol::<MockTransport>::parse_fan_status_speed(&pkt).is_none());
    }

    #[test]
    fn parse_fan_status_speed_rejects_wrong_type() {
        let mut pkt = vec![0u8; 48];
        pkt[0] = 0x67;
        pkt[1] = 0x04; // voltage subtype, not speed
        assert!(NzxtControlHubProtocol::<MockTransport>::parse_fan_status_speed(&pkt).is_none());
    }

    #[test]
    fn parse_fan_status_speed_rejects_short_packet() {
        let pkt = vec![0x67, 0x02];
        assert!(NzxtControlHubProtocol::<MockTransport>::parse_fan_status_speed(&pkt).is_none());
    }

    #[test]
    fn parse_fan_config_returns_fan_types() {
        let mut pkt = vec![0u8; 16 + FAN_CHANNELS];
        pkt[0] = 0x61;
        pkt[1] = 0x03;
        pkt[16] = 2; // channel 0: PWM
        pkt[17] = 1; // channel 1: DC
        pkt[18] = 0; // channel 2: none
        pkt[19] = 2; // channel 3: PWM
        pkt[20] = 1; // channel 4: DC
        let result = NzxtControlHubProtocol::<MockTransport>::parse_fan_config(&pkt).unwrap();
        assert_eq!(result[0], 2);
        assert_eq!(result[1], 1);
        assert_eq!(result[2], 0);
        assert_eq!(result[3], 2);
        assert_eq!(result[4], 1);
    }

    #[test]
    fn parse_fan_config_rejects_short_packet() {
        let pkt = vec![0x61u8, 0x03, 0x00];
        assert!(NzxtControlHubProtocol::<MockTransport>::parse_fan_config(&pkt).is_none());
    }

    // --- cache read/write ---

    #[tokio::test]
    async fn fan_cache_returns_zero_before_update() {
        let p = protocol(vec![]);
        assert_eq!(p.read_fan_rpm(0).await, 0);
        assert_eq!(p.read_fan_duty(0).await, 0);
    }

    #[tokio::test]
    async fn fan_cache_updated_by_handle_packet() {
        let p = protocol(vec![]);
        let mut pkt = vec![0u8; 48];
        pkt[0] = 0x67;
        pkt[1] = 0x02;
        pkt[24] = 0xB0;
        pkt[25] = 0x04; // channel 0 rpm=1200
        pkt[40] = 75; // channel 0 duty=75
        p.handle_packet(&pkt).await;
        assert_eq!(p.read_fan_rpm(0).await, 1200);
        assert_eq!(p.read_fan_duty(0).await, 75);
    }

    // The FAN_STATUS packet carries fan_type at offset 16+i (1=DC, 2=PWM, 0=absent).
    // read_fan_controllable() must return true only when fan_type != 0; this is
    // what gates the fan-curve widget in the UI.
    #[tokio::test]
    async fn fan_type_from_status_packet_makes_channel_controllable() {
        let p = protocol(vec![]);
        let mut pkt = vec![0u8; 48];
        pkt[0] = 0x67;
        pkt[1] = 0x02;
        pkt[16] = 2; // channel 0: PWM
        pkt[17] = 1; // channel 1: DC
        pkt[18] = 0; // channel 2: absent
        p.handle_packet(&pkt).await;
        assert!(
            p.read_fan_controllable(0).await,
            "PWM fan must be controllable"
        );
        assert!(
            p.read_fan_controllable(1).await,
            "DC fan must be controllable"
        );
        assert!(
            !p.read_fan_controllable(2).await,
            "absent fan must not be controllable"
        );
    }

    #[tokio::test]
    async fn fan_controllable_false_before_any_packet() {
        let p = protocol(vec![]);
        assert!(!p.read_fan_controllable(0).await);
    }

    // --- set_fan_duty write format ---

    #[tokio::test]
    async fn set_fan_duty_sends_correct_bytes() {
        let p = capturing_protocol();
        p.write_fan_duty(1, 75).await.unwrap();
        let written = p.base.transport.written.lock().await;
        assert_eq!(written.len(), 1);
        let pkt = &written[0];
        assert_eq!(pkt[0], 0x62);
        assert_eq!(pkt[1], 0x01);
        assert_eq!(pkt[2], 0b0000_0010); // channel 1 bitmask
        assert_eq!(pkt[3], 0); // channel 0 duty = 0
        assert_eq!(pkt[4], 75); // channel 1 duty = 75
        assert_eq!(pkt[5], 0); // channel 2 duty = 0
    }

    #[tokio::test]
    async fn set_fan_duty_updates_cache_optimistically() {
        let p = capturing_protocol();
        p.write_fan_duty(0, 60).await.unwrap();
        assert_eq!(p.read_fan_duty(0).await, 60);
    }

    #[tokio::test]
    async fn set_fan_duty_out_of_range_returns_error() {
        let p = capturing_protocol();
        assert!(p.write_fan_duty(FAN_CHANNELS as u8, 50).await.is_err());
    }
}
