use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

use crate::{
    drivers::{
        chain::{ChainAdapter, ChainHost, ChannelDescriptor},
        transports::{hid::HidTransport, Transport},
        vendors::generic::devices::common::{
            build_device_id, linear_rgb_zone, per_led_frame, stable_serial,
        },
        CapabilityRef, ChainCapability, Controller, Device, RgbCapability, RgbStateSlot,
        VisibilitySlot, CHAIN_LINK_KIND_AURA_ARGB,
    },
    registry::discovery::{DeviceDescriptor, DiscoveryHandle},
};
use halod_shared::types::DeviceType;
use halod_shared::types::{
    EffectParamDescriptor, EffectParamValue, NativeEffect, ParamKind, RgbColor, RgbDescriptor,
    RgbState, RgbZone,
};
use halod_shared::zone_transform::transform_colors;

use crate::drivers::vendors::asus::protocols::aura_usb::{
    parse_config, AuraChannel, AuraUsbProtocol, DEFAULT_ARGB_LEDS, MAX_ARGB_LEDS,
};

fn argb_channel_id(direct_channel: u8) -> String {
    format!("argb_{direct_channel}")
}

pub(crate) fn aura_native_effects() -> Vec<NativeEffect> {
    vec![
        NativeEffect {
            id: "off".into(),
            name: "Off".into(),
            params: vec![],
        },
        NativeEffect {
            id: "breathing".into(),
            name: "Breathing".into(),
            params: vec![EffectParamDescriptor {
                id: "color".into(),
                label: "Color".into(),
                kind: ParamKind::Color,
                default: EffectParamValue::Color(RgbColor {
                    r: 255,
                    g: 255,
                    b: 255,
                }),
            }],
        },
        NativeEffect {
            id: "spectrum_cycle".into(),
            name: "Spectrum Cycle".into(),
            params: vec![],
        },
        NativeEffect {
            id: "rainbow_wave".into(),
            name: "Rainbow Wave".into(),
            params: vec![],
        },
    ]
}

pub(crate) fn split_channels(
    channels: &[AuraChannel],
    non_chainable: &[(u8, &str, &str)],
) -> (RgbDescriptor, Vec<u8>, HashMap<String, u8>) {
    let mut zones: Vec<RgbZone> = Vec::new();
    let mut non_chainable_channels: Vec<u8> = Vec::new();
    let mut chainable_map: HashMap<String, u8> = HashMap::new();

    for ch in channels {
        let known = non_chainable
            .iter()
            .find(|(dc, _, _)| *dc == ch.direct_channel);

        if let Some((_, zid, zname)) = known {
            zones.push(linear_rgb_zone(zid, zname, ch.num_leds as usize));
            non_chainable_channels.push(ch.direct_channel);
        } else {
            chainable_map.insert(argb_channel_id(ch.direct_channel), ch.direct_channel);
        }
    }

    let descriptor = RgbDescriptor {
        zones,
        native_effects: aura_native_effects(),
    };
    (descriptor, non_chainable_channels, chainable_map)
}

pub(crate) struct AuraMotherboardDescriptor {
    vid: u16,
    pid: u16,
    model_name: &'static str,
    non_chainable_zones: &'static [(u8, &'static str, &'static str)],
}

static ASUS_AURA_DEVICES: &[AuraMotherboardDescriptor] = &[
    AuraMotherboardDescriptor {
        vid: 0x0B05,
        pid: 0x1AA6,
        model_name: "ASUS X870E",
        non_chainable_zones: &[(3, "logo", "ROG Logo")],
    },
    AuraMotherboardDescriptor {
        vid: 0x0B05,
        pid: 0x18A3,
        model_name: "ASUS ROG Strix Z390-F Gaming",
        non_chainable_zones: &[],
    },
    AuraMotherboardDescriptor {
        vid: 0x0B05,
        pid: 0x1866,
        model_name: "ASUS Aura Motherboard",
        non_chainable_zones: &[],
    },
    AuraMotherboardDescriptor {
        vid: 0x0B05,
        pid: 0x18A5,
        model_name: "ASUS Aura Motherboard",
        non_chainable_zones: &[],
    },
    AuraMotherboardDescriptor {
        vid: 0x0B05,
        pid: 0x18F3,
        model_name: "ASUS Aura Motherboard",
        non_chainable_zones: &[],
    },
    AuraMotherboardDescriptor {
        vid: 0x0B05,
        pid: 0x1867,
        model_name: "ASUS Aura Motherboard",
        non_chainable_zones: &[],
    },
    AuraMotherboardDescriptor {
        vid: 0x0B05,
        pid: 0x1872,
        model_name: "ASUS Aura Motherboard",
        non_chainable_zones: &[],
    },
    AuraMotherboardDescriptor {
        vid: 0x0B05,
        pid: 0x1939,
        model_name: "ASUS Aura Motherboard",
        non_chainable_zones: &[],
    },
    AuraMotherboardDescriptor {
        vid: 0x0B05,
        pid: 0x19AF,
        model_name: "ASUS Aura Motherboard",
        non_chainable_zones: &[],
    },
    AuraMotherboardDescriptor {
        vid: 0x0B05,
        pid: 0x1A30,
        model_name: "ASUS Aura Motherboard",
        non_chainable_zones: &[],
    },
    AuraMotherboardDescriptor {
        vid: 0x0B05,
        pid: 0x1A6C,
        model_name: "ASUS Aura Motherboard",
        non_chainable_zones: &[],
    },
    AuraMotherboardDescriptor {
        vid: 0x0B05,
        pid: 0x1B3B,
        model_name: "ASUS Aura Motherboard",
        non_chainable_zones: &[],
    },
    AuraMotherboardDescriptor {
        vid: 0x0B05,
        pid: 0x1BED,
        model_name: "ASUS Aura Motherboard",
        non_chainable_zones: &[],
    },
];

inventory::submit! {
    DeviceDescriptor {
        matches: |h| {
            let DiscoveryHandle::Hid { vid, pid, .. } = h else { return false };
            ASUS_AURA_DEVICES.iter().any(|s| s.vid == *vid && s.pid == *pid)
        },
        make: |h| {
            let DiscoveryHandle::Hid { path, serial, idx, vid, pid, .. } = h else {
                anyhow::bail!("descriptor matched non-HID handle");
            };
            let spec = ASUS_AURA_DEVICES.iter().find(|s| s.vid == vid && s.pid == pid).unwrap();
            AsusAuraUsb::new(path, serial, idx, spec).map(|arc| arc as Arc<dyn Device>)
        },
    }
}

fn build_channels(argb_count: u8, led_counts: &[u8]) -> Vec<AuraChannel> {
    (0..argb_count)
        .map(|i| AuraChannel {
            effect_channel: i + 1,
            direct_channel: i,
            num_leds: led_counts
                .get(i as usize)
                .copied()
                .unwrap_or(DEFAULT_ARGB_LEDS),
        })
        .collect()
}

pub struct AsusAuraUsb<T: Transport = HidTransport> {
    /// Built via `Arc::new_cyclic` in `new` so we can hand the parent (= the
    /// `ChainAdapter` impl) to a `ChainHost` without cloning the device.
    self_ref: Weak<Self>,
    id: String,
    serial_number: Option<String>,
    spec: &'static AuraMotherboardDescriptor,
    protocol: AuraUsbProtocol<T>,
    channels: OnceLock<Vec<AuraChannel>>,
    rgb: RgbStateSlot,
    /// Holds non chainable zones (i.e. native zones to this device, not via headers)
    rgb_descriptor: OnceLock<RgbDescriptor>,
    /// `direct_channel` for each entry in `rgb_descriptor.zones`, in the same
    /// order — zipped together by `apply_state` / `write_frame`.
    non_chainable_channels: OnceLock<Vec<u8>>,
    visibility: VisibilitySlot,
    /// LED count for the fixed on-board mainboard zone. 0 if absent.
    mb_zone_leds: OnceLock<u8>,
    chainable_channel_map: OnceLock<HashMap<String, u8>>,
    /// Shared chain runtime
    chain_host: OnceLock<Arc<ChainHost>>,
}

impl AsusAuraUsb<HidTransport> {
    pub fn new(
        path: &str,
        serial: Option<&str>,
        index: usize,
        spec: &'static AuraMotherboardDescriptor,
    ) -> Result<Arc<Self>> {
        let protocol = AuraUsbProtocol::open(path)?;
        let id = build_device_id("asus_aura_usb", serial, index);
        Ok(Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            id,
            serial_number: stable_serial(serial),
            spec,
            protocol,
            channels: OnceLock::new(),
            rgb_descriptor: OnceLock::new(),
            non_chainable_channels: OnceLock::new(),
            rgb: RgbStateSlot::default(),
            visibility: VisibilitySlot::default(),
            mb_zone_leds: OnceLock::new(),
            chainable_channel_map: OnceLock::new(),
            chain_host: OnceLock::new(),
        }))
    }
}

impl<T: Transport + 'static> AsusAuraUsb<T> {
    #[cfg(test)]
    fn new_for_test(
        id: &str,
        spec: &'static AuraMotherboardDescriptor,
        protocol: AuraUsbProtocol<T>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            id: id.to_string(),
            serial_number: None,
            spec,
            protocol,
            channels: OnceLock::new(),
            rgb_descriptor: OnceLock::new(),
            non_chainable_channels: OnceLock::new(),
            rgb: RgbStateSlot::default(),
            visibility: VisibilitySlot::default(),
            mb_zone_leds: OnceLock::new(),
            chainable_channel_map: OnceLock::new(),
            chain_host: OnceLock::new(),
        })
    }

    fn arc_self_as_adapter(&self) -> Arc<dyn ChainAdapter> {
        self.self_ref
            .upgrade()
            .expect("arc_self_as_adapter called after device drop")
    }

    async fn apply_state(&self, state: &RgbState) -> Result<()> {
        let channels = self
            .non_chainable_channels
            .get()
            .ok_or_else(|| anyhow::anyhow!("device not initialized"))?;
        let desc = self
            .rgb_descriptor
            .get()
            .ok_or_else(|| anyhow::anyhow!("device not initialized"))?;

        let mb_leds = self.mb_zone_leds.get().copied().unwrap_or(0);

        match state {
            RgbState::Static { color } => {
                if mb_leds > 0 {
                    let colors = vec![*color; mb_leds as usize];
                    self.protocol.send_direct_mb(&colors).await?;
                }
                let non_mb = desc.zones.iter().filter(|z| z.id != "motherboard");
                for (direct_channel, zone) in channels.iter().zip(non_mb) {
                    let colors = vec![*color; zone.leds.len()];
                    self.protocol.send_direct(*direct_channel, &colors).await?;
                }
            }

            RgbState::PerLed { zones } => {
                if mb_leds > 0 {
                    if let Some(led_map) = zones.get("motherboard") {
                        let mb_zone = desc
                            .zones
                            .iter()
                            .find(|z| z.id == "motherboard")
                            .ok_or_else(|| {
                                anyhow::anyhow!("motherboard zone not found in descriptor")
                            })?;
                        let colors = per_led_frame(led_map, mb_leds as usize);
                        let transform = self.rgb.transform_for("motherboard");
                        let colors = transform_colors(&colors, mb_zone, &transform);
                        self.protocol.send_direct_mb(&colors).await?;
                    }
                }
                let non_mb = desc.zones.iter().filter(|z| z.id != "motherboard");
                for (direct_channel, zone) in channels.iter().zip(non_mb) {
                    if let Some(led_map) = zones.get(&zone.id) {
                        let colors = per_led_frame(led_map, zone.leds.len());
                        let transform = self.rgb.transform_for(&zone.id);
                        let colors = transform_colors(&colors, zone, &transform);
                        self.protocol.send_direct(*direct_channel, &colors).await?;
                    }
                }
            }

            // Native effects target the whole device
            RgbState::NativeEffect { id, params } => {
                let all_channels = self
                    .channels
                    .get()
                    .ok_or_else(|| anyhow::anyhow!("device not initialized"))?;
                for ch in all_channels {
                    self.protocol
                        .send_effect(id, ch.effect_channel, params)
                        .await?;
                }
            }

            RgbState::Engine | RgbState::DirectEffect { .. } => {}
        }

        Ok(())
    }
}

#[async_trait]
impl<T: Transport + 'static> Device for AsusAuraUsb<T> {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        "ASUS Aura"
    }

    fn vendor(&self) -> &str {
        "ASUS"
    }

    fn model(&self) -> &str {
        self.spec.model_name
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Motherboard
    }

    async fn initialize(&self) -> Result<bool> {
        // Stop legacy gen-2 continuous-cycle mode first so we own the channels.
        self.protocol.stop_gen2().await;

        let fw = self.protocol.get_firmware_version().await;
        log::info!("[ASUS Aura USB] Firmware: {:?}", fw);

        let Some(config) = self.protocol.get_config_table().await else {
            anyhow::bail!("[ASUS Aura USB] Could not read config table");
        };

        let (argb_count, led_counts, mb_leds) = parse_config(&config);
        log::info!(
            "[ASUS Aura USB] Config: {} ARGB channels, LED counts: {:?}, {} fixed MB LEDs",
            argb_count,
            led_counts,
            mb_leds,
        );

        if argb_count == 0 && mb_leds == 0 {
            anyhow::bail!("[ASUS Aura USB] No controllable channels found");
        }

        let channels = build_channels(argb_count, &led_counts);

        // Set mainboard effect channel (0) plus all ARGB effect channels to direct.
        for ch in 0..=argb_count {
            if let Err(e) = self.protocol.set_channel_direct(ch).await {
                log::warn!("[ASUS Aura USB] set_channel_direct({ch}) failed: {e}");
            }
        }

        let (mut descriptor, non_chainable, chainable_map) =
            split_channels(&channels, self.spec.non_chainable_zones);

        // Prepend the fixed on-board mainboard zone when the config reports LEDs.
        if mb_leds > 0 {
            descriptor.zones.insert(
                0,
                linear_rgb_zone("motherboard", "Motherboard", mb_leds as usize),
            );
        }

        // OnceLocks must be set before constructing ChainHost — its
        // adapter.channels() reads back through these fields.
        if self.mb_zone_leds.set(mb_leds).is_err() {
            log::warn!(
                "[ASUS Aura USB] initialize() called more than once, mb_zone_leds already set"
            );
        }
        if self.channels.set(channels).is_err() {
            log::warn!("[ASUS Aura USB] initialize() called more than once, channels already set");
        }
        if self.rgb_descriptor.set(descriptor).is_err() {
            log::warn!(
                "[ASUS Aura USB] initialize() called more than once, descriptor already set"
            );
        }
        if self.non_chainable_channels.set(non_chainable).is_err() {
            log::warn!("[ASUS Aura USB] initialize() called more than once, non_chainable_channels already set");
        }
        if self.chainable_channel_map.set(chainable_map).is_err() {
            log::warn!("[ASUS Aura USB] initialize() called more than once, chainable_channel_map already set");
        }

        let host = ChainHost::new(self.arc_self_as_adapter(), CHAIN_LINK_KIND_AURA_ARGB);
        if self.chain_host.set(host).is_err() {
            log::warn!(
                "[ASUS Aura USB] initialize() called more than once, chain_host already set"
            );
        }

        Ok(true)
    }

    async fn close(&self) {}

    fn wire_serial_number(&self) -> Option<String> {
        self.serial_number.clone()
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![
            CapabilityRef::Rgb(self),
            CapabilityRef::Controller(self),
            CapabilityRef::Chain(self),
        ]
    }

    fn write_rate_status(&self) -> Option<halod_shared::types::WriteRateStatus> {
        Some(self.protocol.transport.rate_status())
    }
}

#[async_trait]
impl<T: Transport + 'static> RgbCapability for AsusAuraUsb<T> {
    fn descriptor(&self) -> &RgbDescriptor {
        static EMPTY: OnceLock<RgbDescriptor> = OnceLock::new();
        self.rgb_descriptor.get().unwrap_or_else(|| {
            EMPTY.get_or_init(|| RgbDescriptor {
                zones: vec![],
                native_effects: vec![],
            })
        })
    }

    fn rgb_state(&self) -> &RgbStateSlot {
        &self.rgb
    }

    async fn apply(&self, state: RgbState) -> Result<()> {
        self.apply_state(&state).await?;
        self.rgb.set_state(Some(state));
        Ok(())
    }

    async fn write_frame(&self, zone_id: &str, colors: &[RgbColor]) -> Result<()> {
        if zone_id == "motherboard" {
            return self.protocol.send_direct_mb(colors).await;
        }

        let non_chainable = self
            .non_chainable_channels
            .get()
            .ok_or_else(|| anyhow::anyhow!("device not initialized"))?;
        let desc = self
            .rgb_descriptor
            .get()
            .ok_or_else(|| anyhow::anyhow!("device not initialized"))?;

        // Exclude the MB zone from position lookup so the index aligns with
        // non_chainable_channels (which only covers non-MB zones).
        let pos = desc
            .zones
            .iter()
            .filter(|z| z.id != "motherboard")
            .position(|z| z.id == zone_id)
            .ok_or_else(|| anyhow::anyhow!("unknown zone: {zone_id}"))?;

        self.protocol.send_direct(non_chainable[pos], colors).await
    }
}

#[async_trait]
impl<T: Transport + 'static> Controller for AsusAuraUsb<T> {
    async fn discover_children(&self) -> Vec<Arc<dyn Device>> {
        // No hardware probe on Aura; chain children are user-added.
        Vec::new()
    }
}

#[async_trait]
impl<T: Transport + 'static> ChainAdapter for AsusAuraUsb<T> {
    fn parent_id(&self) -> String {
        self.id.clone()
    }

    fn channels(&self) -> Vec<ChannelDescriptor> {
        let (Some(map), Some(all_channels)) =
            (self.chainable_channel_map.get(), self.channels.get())
        else {
            return Vec::new();
        };
        // Sort by direct_channel so the UI list stays stable across reboots.
        let mut entries: Vec<(&String, &u8)> = map.iter().collect();
        entries.sort_by_key(|(_, dc)| **dc);
        entries
            .into_iter()
            .map(|(logical_id, direct_channel)| {
                let max_leds = all_channels
                    .iter()
                    .find(|ch| ch.direct_channel == *direct_channel)
                    .map(|ch| ch.num_leds as u32)
                    .unwrap_or(MAX_ARGB_LEDS as u32);
                ChannelDescriptor {
                    channel_id: logical_id.clone(),
                    display_name: format!("ARGB Header {}", direct_channel + 1),
                    max_leds,
                }
            })
            .collect()
    }

    async fn write_composed_frame(&self, channel_id: &str, composed: &[RgbColor]) -> Result<()> {
        let direct_channel = self
            .chainable_channel_map
            .get()
            .ok_or_else(|| anyhow::anyhow!("device not initialized"))?
            .get(channel_id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("unknown chainable channel: {channel_id}"))?;
        self.protocol.send_direct(direct_channel, composed).await
    }
}

impl<T: Transport + 'static> ChainCapability for AsusAuraUsb<T> {
    fn chain_host(&self) -> Option<&Arc<ChainHost>> {
        self.chain_host.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::transports::mock::test_transport::MockTransport;

    #[test]
    fn build_channels_correct_effect_and_direct_channels() {
        let chs = build_channels(3, &[120, 60, 30]);
        assert_eq!(chs.len(), 3);
        assert_eq!(chs[0].effect_channel, 1);
        assert_eq!(chs[0].direct_channel, 0);
        assert_eq!(chs[0].num_leds, 120);
        assert_eq!(chs[1].effect_channel, 2);
        assert_eq!(chs[1].direct_channel, 1);
        assert_eq!(chs[2].effect_channel, 3);
        assert_eq!(chs[2].direct_channel, 2);
    }

    #[test]
    fn split_channels_emits_logo_zone_for_x870e() {
        let chs = build_channels(4, &[30, 30, 30, 1]);
        let spec = ASUS_AURA_DEVICES.iter().find(|s| s.pid == 0x1AA6).unwrap();
        let (desc, non_chainable, chainable) = split_channels(&chs, spec.non_chainable_zones);
        assert_eq!(desc.zones.len(), 1, "only logo is a zone");
        assert_eq!(desc.zones[0].id, "logo");
        assert_eq!(non_chainable, vec![3]);
        assert_eq!(chainable.len(), 3);
    }

    #[test]
    fn split_channels_all_chainable_for_pid_with_no_fixed_zones() {
        let chs = build_channels(4, &[30, 30, 30, 1]);
        let spec = ASUS_AURA_DEVICES.iter().find(|s| s.pid == 0x18F3).unwrap();
        let (desc, non_chainable, chainable) = split_channels(&chs, spec.non_chainable_zones);
        assert!(desc.zones.is_empty());
        assert!(non_chainable.is_empty());
        assert_eq!(chainable.len(), 4);
    }

    #[test]
    fn descriptor_registered_in_inventory() {
        let count = inventory::iter::<crate::registry::discovery::DeviceDescriptor>().count();
        assert!(
            count > 0,
            "inventory must have at least one DeviceDescriptor"
        );
    }

    fn x870e_device() -> Arc<AsusAuraUsb<MockTransport>> {
        let spec = ASUS_AURA_DEVICES.iter().find(|s| s.pid == 0x1AA6).unwrap();

        // Firmware response: header 0xEC 0x02, then "1.0\0..."
        let mut fw = vec![0u8; 65];
        fw[0] = 0xEC;
        fw[1] = 0x02;
        fw[2] = b'1';
        fw[3] = b'.';
        fw[4] = b'0';

        // Config response: header 0xEC 0x30, then 60-byte config table at [4..64].
        // Config offsets (relative to resp[4]):
        //   [2]  = argb_count  → resp[6]
        //   [6]  = ch0 leds   → resp[10]
        //   [12] = ch1 leds   → resp[16]
        //   [18] = ch2 leds   → resp[22]
        //   [24] = ch3 leds   → resp[28]
        //   [27] = mb_leds    → resp[31]  (0 = no fixed MB zone)
        let mut cfg = vec![0u8; 65];
        cfg[0] = 0xEC;
        cfg[1] = 0x30;
        cfg[6] = 4; // 4 ARGB channels
        cfg[10] = 10; // ch0: 10 LEDs (chainable)
        cfg[16] = 10; // ch1: 10 LEDs (chainable)
        cfg[22] = 10; // ch2: 10 LEDs (chainable)
        cfg[28] = 1; // ch3:  1 LED  (logo — non-chainable on X870E)

        let proto = AuraUsbProtocol {
            transport: MockTransport::new(vec![fw, cfg]),
        };
        AsusAuraUsb::new_for_test("asus_aura_usb_test", spec, proto)
    }

    #[tokio::test]
    async fn initialized_x870e_serializes_to_expected_json() {
        let device = x870e_device();
        assert!(device.initialize().await.unwrap());

        let wire = device.serialize().await;
        let json = serde_json::to_value(&wire).unwrap();

        let expected = serde_json::json!({
            "id": "asus_aura_usb_test",
            "name": "ASUS Aura",
            "vendor": "ASUS",
            "model": "ASUS X870E",
            "device_type": "motherboard",
            "connected": true,
            "active_state": "visible",
            "capabilities": [
                {
                    "kind": "rgb",
                    "data": {
                        "descriptor": {
                            "zones": [
                                {
                                    "id": "logo",
                                    "name": "ROG Logo",
                                    "topology": {"type": "linear"},
                                    "leds": [{"id": 0, "x": 0.5, "y": 0.5}]
                                }
                            ],
                            "native_effects": [
                                {"id": "off", "name": "Off", "params": []},
                                {
                                    "id": "breathing",
                                    "name": "Breathing",
                                    "params": [{
                                        "id": "color",
                                        "label": "Color",
                                        "kind": {"kind": "color"},
                                        "default": {"r": 255, "g": 255, "b": 255}
                                    }]
                                },
                                {"id": "spectrum_cycle", "name": "Spectrum Cycle", "params": []},
                                {"id": "rainbow_wave", "name": "Rainbow Wave", "params": []}
                            ]
                        },
                        "state": null,
                        "zone_transforms": {},
                        "chainable_channels": [
                            {
                                "channel_id": "argb_0",
                                "name": "ARGB Header 1",
                                "max_leds": 10,
                                "link_kind": "generic_aura_argb",
                                "links": []
                            },
                            {
                                "channel_id": "argb_1",
                                "name": "ARGB Header 2",
                                "max_leds": 10,
                                "link_kind": "generic_aura_argb",
                                "links": []
                            },
                            {
                                "channel_id": "argb_2",
                                "name": "ARGB Header 3",
                                "max_leds": 10,
                                "link_kind": "generic_aura_argb",
                                "links": []
                            }
                        ]
                    }
                }
            ]
        });

        assert_eq!(json, expected);
    }

    fn ch(direct_channel: u8, num_leds: u8) -> AuraChannel {
        AuraChannel {
            effect_channel: direct_channel + 1,
            direct_channel,
            num_leds,
        }
    }

    #[test]
    fn split_channels_all_chainable_when_no_non_chainable_given() {
        let chs = vec![ch(0, 120), ch(1, 60)];
        let (desc, non_chainable, chainable) = split_channels(&chs, &[]);
        assert!(desc.zones.is_empty());
        assert!(non_chainable.is_empty());
        assert_eq!(chainable.len(), 2);
        assert!(chainable.contains_key("argb_0"));
        assert!(chainable.contains_key("argb_1"));
    }

    #[test]
    fn split_channels_has_four_native_effects() {
        let (desc, _, _) = split_channels(&[], &[]);
        assert_eq!(desc.native_effects.len(), 4);
        let ids: Vec<&str> = desc.native_effects.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"off"));
        assert!(ids.contains(&"breathing"));
        assert!(ids.contains(&"spectrum_cycle"));
        assert!(ids.contains(&"rainbow_wave"));
    }

    #[test]
    fn split_channels_non_chainable_channel_becomes_named_zone() {
        let chs = vec![ch(0, 30), ch(1, 30), ch(2, 30), ch(3, 1)];
        let (desc, non_chainable, chainable) = split_channels(&chs, &[(3, "logo", "ROG Logo")]);
        assert_eq!(desc.zones.len(), 1, "only logo is a zone");
        assert_eq!(desc.zones[0].id, "logo");
        assert_eq!(non_chainable, vec![3]);
        assert_eq!(chainable.len(), 3);
        assert!(chainable.contains_key("argb_0"));
        assert!(chainable.contains_key("argb_1"));
        assert!(chainable.contains_key("argb_2"));
        assert_eq!(chainable["argb_0"], 0);
    }

    #[test]
    fn split_channels_all_chainable_when_no_entry_matches() {
        let chs = vec![ch(0, 30), ch(1, 30), ch(2, 30), ch(3, 1)];
        let (desc, non_chainable, chainable) = split_channels(&chs, &[]);
        assert!(desc.zones.is_empty());
        assert!(non_chainable.is_empty());
        assert_eq!(chainable.len(), 4);
    }
}
