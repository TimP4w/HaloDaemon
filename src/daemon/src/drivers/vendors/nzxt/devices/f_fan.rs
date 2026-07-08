use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::{
    drivers::{
        chain::ChainHub,
        vendors::generic::devices::common::{per_led_frame, ring_led_positions},
        vendors::nzxt::devices::NzxtFanHub,
        CapabilityRef, Device, FanCapability, FanStateSlot, RgbCapability, RgbStateSlot,
        VisibilitySlot,
    },
    registry::discovery::{DeviceDescriptor, DiscoveryHandle},
};
use halod_shared::types::{DeviceType, RgbColor, RgbDescriptor, RgbState, RgbZone, ZoneTopology};
use halod_shared::zone_transform::transform_colors;

inventory::submit!(DeviceDescriptor {
    matches: |h| matches!(h, DiscoveryHandle::ChainAccessory { accessory_id, .. }
        if [0x13u8, 0x14, 0x17, 0x18, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F].contains(accessory_id)),
    make: |h| {
        let DiscoveryHandle::ChainAccessory {
            channel_id,
            accessory_id,
            chain_hub,
            fan_hub,
        } = h
        else {
            anyhow::bail!("descriptor matched non-ChainAccessory handle");
        };
        Ok(
            Arc::new(NZXTFFan::new(channel_id, accessory_id, chain_hub, fan_hub))
                as Arc<dyn crate::drivers::Device>,
        )
    },
});

pub struct NZXTFFan {
    chain_hub: Arc<dyn ChainHub>,
    fan_hub: Arc<dyn NzxtFanHub>,
    channel_id: u8,
    accessory_id: u8,
    id: String,
    rgb_descriptor: RgbDescriptor,
    rgb: RgbStateSlot,
    fan: FanStateSlot,
    visibility: VisibilitySlot,
}

impl NZXTFFan {
    pub fn new(
        channel_id: u8,
        accessory_id: u8,
        chain_hub: Arc<dyn ChainHub>,
        fan_hub: Arc<dyn NzxtFanHub>,
    ) -> Self {
        let (_, fan_count, leds_per_fan) = Self::device_info_for(accessory_id);
        let id = format!("nzxt_f_fan_{}_{}", fan_hub.id(), channel_id);
        Self {
            channel_id,
            accessory_id,
            chain_hub,
            fan_hub,
            id,
            rgb_descriptor: Self::build_descriptor(fan_count, leds_per_fan),
            rgb: RgbStateSlot::default(),
            fan: FanStateSlot::default(),
            visibility: VisibilitySlot::default(),
        }
    }

    /// Returns `(display_name, fan_count, leds_per_fan)`.
    fn device_info_for(accessory_id: u8) -> (&'static str, u8, usize) {
        match accessory_id {
            0x13 => ("F120 RGB", 1, 8),
            0x14 => ("F140 RGB", 1, 8),
            0x17 => ("F140 RGB Core", 1, 8),
            0x18 => ("F140 RGB Core", 1, 8),
            0x1B => ("F240 RGB Core", 2, 8),
            0x1C => ("F240 RGB Core", 2, 8),
            0x1D => ("F360 RGB Core", 3, 8),
            0x1E => ("F360 RGB Core", 3, 8),
            0x1F => ("F420 RGB Core", 3, 8),
            _ => ("NZXT F Fan", 1, 8),
        }
    }

    fn device_info(&self) -> (&'static str, u8, usize) {
        Self::device_info_for(self.accessory_id)
    }

    fn build_descriptor(fan_count: u8, leds_per_fan: usize) -> RgbDescriptor {
        let topology = if fan_count == 1 {
            ZoneTopology::Ring
        } else {
            ZoneTopology::Rings { count: fan_count }
        };
        let total = fan_count as u32 * leds_per_fan as u32;
        let leds = ring_led_positions(&topology, total);
        RgbDescriptor {
            zones: vec![RgbZone {
                id: "ring".to_string(),
                name: "Ring".to_string(),
                topology,
                leds,
            }],
            native_effects: vec![],
        }
    }

    /// Sends the RGB state to hardware
    /// Used by both `apply()` and `load_state()`.
    async fn apply_state(&self, state: RgbState) -> Result<()> {
        let led_count = self.rgb_descriptor.zones[0].leds.len();
        let dev_id = self.id();
        match &state {
            RgbState::Static { color } => {
                let colors = vec![*color; led_count];
                self.chain_hub
                    .write_chain_slice(&self.channel_id.to_string(), dev_id, &colors)
                    .await?;
            }
            RgbState::PerLed { zones } => {
                if let Some(leds) = zones.get("ring") {
                    let colors = per_led_frame(leds, led_count);
                    let rgb_zone = &self.rgb_descriptor.zones[0];
                    let transform = self.rgb.transform_for(&rgb_zone.id);
                    let colors = transform_colors(&colors, rgb_zone, &transform);
                    self.chain_hub
                        .write_chain_slice(&self.channel_id.to_string(), dev_id, &colors)
                        .await?;
                }
            }
            RgbState::NativeEffect { .. } => {
                // Chain composition reads user-driven colours; firmware effects
                // would need a separate per-channel path the hubs don't expose
                // yet.
            }
            RgbState::Engine | RgbState::DirectEffect { .. } => {}
        }
        self.rgb.set_state(Some(state));
        Ok(())
    }
}

#[async_trait]
impl Device for NZXTFFan {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        self.device_info().0
    }
    fn vendor(&self) -> &str {
        "NZXT"
    }
    fn model(&self) -> &str {
        self.device_info().0
    }

    async fn initialize(&self) -> Result<bool> {
        log::info!("[NZXT F Fan] Initialized: {}", self.model());
        Ok(true)
    }

    async fn close(&self) {}

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Fan
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![CapabilityRef::Fan(self), CapabilityRef::Rgb(self)]
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn debug_transport(&self) -> Option<&'static str> {
        Some("child")
    }
}

#[async_trait]
impl FanCapability for NZXTFFan {
    fn fan_channel_id(&self) -> u8 {
        self.channel_id
    }

    async fn fan_controllable(&self) -> bool {
        self.fan_hub
            .get_fan_controllable(self.channel_id)
            .await
            .unwrap_or(false)
    }

    async fn get_duty(&self) -> Result<u8> {
        self.fan_hub.get_fan_duty(self.channel_id).await
    }

    async fn set_duty(&self, duty: u8) -> Result<()> {
        self.fan_hub.set_fan_duty(self.channel_id, duty).await
    }

    async fn get_rpm(&self) -> Option<u32> {
        Some(self.fan_hub.get_fan_rpm(self.channel_id).await.unwrap_or(0))
    }

    fn fan_state(&self) -> &FanStateSlot {
        &self.fan
    }
}

#[async_trait]
impl RgbCapability for NZXTFFan {
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
        let dev_id = self.id();
        self.chain_hub
            .write_chain_slice(&self.channel_id.to_string(), dev_id, colors)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use async_trait::async_trait;
    use halod_shared::types::{DeviceCapability, DeviceType};

    /// Test double that satisfies both hub traits. Real Kraken/Hub will route
    /// the chain side through `ChainHost`; here we don't care, we just capture
    /// what gets written.
    struct MockHub {
        id: &'static str,
        last_frame: std::sync::Mutex<HashMap<String, Vec<RgbColor>>>,
    }

    #[async_trait]
    impl ChainHub for MockHub {
        async fn write_chain_slice(
            &self,
            channel_id: &str,
            _child_device_id: &str,
            colors: &[RgbColor],
        ) -> anyhow::Result<()> {
            self.last_frame
                .lock()
                .unwrap()
                .insert(channel_id.to_string(), colors.to_vec());
            Ok(())
        }
        fn link_name(&self, _channel_id: &str, _child_device_id: &str) -> Option<String> {
            None
        }
    }

    #[async_trait]
    impl NzxtFanHub for MockHub {
        fn id(&self) -> &str {
            self.id
        }
        async fn get_fan_rpm(&self, _: u8) -> anyhow::Result<u32> {
            Ok(1200)
        }
        async fn get_fan_duty(&self, _: u8) -> anyhow::Result<u8> {
            Ok(50)
        }
        async fn get_fan_controllable(&self, _: u8) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn set_fan_duty(&self, _: u8, _: u8) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn mock_hubs() -> (Arc<dyn ChainHub>, Arc<dyn NzxtFanHub>) {
        let hub = Arc::new(MockHub {
            id: "nzxt_hub_ABC123",
            last_frame: std::sync::Mutex::new(HashMap::new()),
        });
        (hub.clone(), hub)
    }

    fn fan(accessory_id: u8) -> NZXTFFan {
        let (chain_hub, fan_hub) = mock_hubs();
        NZXTFFan::new(0, accessory_id, chain_hub, fan_hub)
    }

    fn fan_on_channel(channel: u8, accessory_id: u8) -> NZXTFFan {
        let (chain_hub, fan_hub) = mock_hubs();
        NZXTFFan::new(channel, accessory_id, chain_hub, fan_hub)
    }

    /// DeviceType test for unused imports.
    #[allow(dead_code)]
    fn _device_type_in_scope() -> DeviceType {
        DeviceType::Fan
    }

    #[tokio::test]
    async fn serialize_fields() {
        let fan = fan_on_channel(3, 0x14);
        let w = fan.serialize().await;
        assert_eq!(w.id, "nzxt_f_fan_nzxt_hub_ABC123_3");
        assert_eq!(w.vendor, "NZXT");
        assert_eq!(w.name, "F140 RGB");
        assert!(matches!(w.device_type, DeviceType::Fan));
        assert!(w.connected);
    }

    #[tokio::test]
    async fn serialize_includes_fan_capability() {
        let fan = fan(0x13);
        let w = fan.serialize().await;
        let fan_cap = w
            .capabilities
            .iter()
            .find(|c| matches!(c, DeviceCapability::Fan(_)));
        assert!(fan_cap.is_some(), "expected Fan capability");
        if let Some(DeviceCapability::Fan(status)) = fan_cap {
            assert_eq!(status.channel, 0);
            assert_eq!(status.rpm, 1200);
            assert_eq!(status.duty, 50);
            assert!(status.controllable);
        }
    }

    #[tokio::test]
    async fn serialize_includes_rgb_capability() {
        let fan = fan(0x13);
        let w = fan.serialize().await;
        assert!(
            w.capabilities
                .iter()
                .any(|c| matches!(c, DeviceCapability::Rgb(_))),
            "expected Rgb capability"
        );
    }

    #[tokio::test]
    async fn descriptor_returns_ring_zone_with_correct_led_count() {
        for (accessory_id, expected_leds, expected_fans) in
            [(0x13u8, 8usize, 1u8), (0x1Bu8, 16, 2), (0x1Du8, 24, 3)]
        {
            let fan = fan(accessory_id);
            let desc = fan.descriptor();
            assert_eq!(desc.zones.len(), 1);
            assert_eq!(desc.zones[0].id, "ring");
            assert_eq!(desc.zones[0].leds.len(), expected_leds);
            match (&desc.zones[0].topology, expected_fans) {
                (ZoneTopology::Ring, 1) => {}
                (ZoneTopology::Rings { count }, n) => assert_eq!(*count, n),
                (topo, fans) => panic!("unexpected topology {topo:?} for fan_count={fans}"),
            }
        }
    }

    #[tokio::test]
    async fn apply_static_updates_state() {
        let fan = fan(0x13);
        let color = RgbColor { r: 255, g: 0, b: 0 };
        fan.apply(RgbState::Static { color }).await.unwrap();
        assert!(matches!(fan.current_state(), Some(RgbState::Static { .. })));
    }

    #[tokio::test]
    async fn apply_engine_updates_state() {
        let fan = fan(0x13);
        fan.apply(RgbState::Engine).await.unwrap();
        assert!(matches!(fan.current_state(), Some(RgbState::Engine)));
    }

    #[tokio::test]
    async fn save_state_does_not_include_duty() {
        let fan = fan_on_channel(2, 0x13);
        let state = crate::drivers::Device::save_state(&fan).await;
        assert!(state.get("duty").is_none());
    }

    #[tokio::test]
    async fn save_state_includes_rgb_null_before_apply() {
        let fan = fan(0x13);
        let state = crate::drivers::Device::save_state(&fan).await;
        assert!(state["rgb"].is_null());
    }

    #[tokio::test]
    async fn save_and_load_static_rgb() {
        let f = fan(0x13);
        let color = RgbColor {
            r: 100,
            g: 200,
            b: 50,
        };
        f.apply(RgbState::Static { color }).await.unwrap();

        let saved = crate::drivers::Device::save_state(&f).await;

        let f2 = fan(0x13);
        f2.load_state(&saved).await;

        assert!(matches!(f2.current_state(), Some(RgbState::Static { .. })));
    }

    #[tokio::test]
    async fn save_and_load_engine_state() {
        let f = fan(0x13);
        f.apply(RgbState::Engine).await.unwrap();
        let saved = crate::drivers::Device::save_state(&f).await;
        let f2 = fan(0x13);
        f2.load_state(&saved).await;
        assert!(matches!(f2.current_state(), Some(RgbState::Engine)));
    }

    #[tokio::test]
    async fn perled_apply_honours_zone_transform() {
        use halod_shared::zone_transform::ZoneContentTransform;

        let hub = Arc::new(MockHub {
            id: "nzxt_hub_ABC123",
            last_frame: std::sync::Mutex::new(HashMap::new()),
        });
        let chain_hub: Arc<dyn ChainHub> = hub.clone();
        let fan_hub: Arc<dyn NzxtFanHub> = hub.clone();
        let fan = NZXTFFan::new(0, 0x13, chain_hub, fan_hub);
        // Single 8-LED ring, offset by 1.
        fan.rgb.set_zone_transform(
            "ring".to_string(),
            ZoneContentTransform {
                led_offset: 1,
                ..Default::default()
            },
        );

        let mut leds = HashMap::new();
        leds.insert("0".to_string(), RgbColor { r: 10, g: 0, b: 0 });
        let mut zones = HashMap::new();
        zones.insert("ring".to_string(), leds);
        fan.apply(RgbState::PerLed { zones }).await.unwrap();

        let frame = hub
            .last_frame
            .lock()
            .unwrap()
            .get("0")
            .cloned()
            .expect("frame written on channel 0");
        assert_eq!(frame.len(), 8);
        // offset 1: output[j] = colors[(j+1) % 8], so LED 0's colour lands at index 7.
        assert_eq!(frame[7], RgbColor { r: 10, g: 0, b: 0 });
        assert_eq!(frame[0], RgbColor { r: 0, g: 0, b: 0 });
    }

    #[test]
    fn zone_transforms_round_trip_in_slot() {
        use halod_shared::zone_transform::ZoneContentTransform;

        // Zone transforms are global (not per-profile), stored in
        // `rgb_transform_slot` — not routed through save_state/load_state.
        let f = fan(0x13);
        f.rgb.set_zone_transform(
            "ring".to_string(),
            ZoneContentTransform {
                reverse: true,
                led_offset: 3,
                ..Default::default()
            },
        );
        let t = f.rgb.transform_for("ring");
        assert!(t.reverse);
        assert_eq!(t.led_offset, 3);

        // A zone that has never been set returns the identity transform.
        let identity = f.rgb.transform_for("other_zone");
        assert!(!identity.reverse);
        assert_eq!(identity.led_offset, 0);
    }
}
