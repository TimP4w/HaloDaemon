// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2021 Smasty <hello@smasty.net>
// Reference: g560-led by Smasty (MIT)
//   https://github.com/mijoe/g560-led
/// Logitech G560 RGB Gaming Speaker driver — HID++ 1.0 vendor long report.
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::{
    drivers::{
        transports::hid::HidTransport,
        vendors::generic::devices::common::{build_device_id, stable_serial},
        vendors::logitech::protocols::hidpp::{HidppChannel, HidppMessenger},
        CapabilityRef, Device, DeviceCapability, RangeCapability, RangeStateCache, RgbCapability,
        RgbStateSlot, VisibilitySlot,
    },
    registry::discovery::{DeviceDescriptor, DiscoveryHandle},
};
use halod_shared::types::{
    DeviceType, LedPosition, Range, RgbColor, RgbDescriptor, RgbState, RgbZone, ZoneTopology,
};

inventory::submit! {
    DeviceDescriptor {
        // Windows splits interface 2 into collections; only usage page 0xFF43/0x0202 accepts writes. Linux falls back to usage_page=0/usage=0.
        matches: |h| match h {
            DiscoveryHandle::Hid { vid: 0x046D, pid: 0x0A78, interface_number: Some(2), usage_page, usage, .. } => {
                (*usage_page == 0xFF43 && *usage == 0x0202) || (*usage_page == 0 && *usage == 0)
            }
            _ => false,
        },
        make: |h| {
            let DiscoveryHandle::Hid { path, serial, idx, .. } = h else {
                anyhow::bail!("descriptor matched non-HID handle");
            };
            Ok(Arc::new(LogitechG560::new(path, serial, idx)?))
        },
    }
}

const G560_WRITE_RATE: halod_shared::types::WriteRateLimit = halod_shared::types::WriteRateLimit {
    max_bytes_per_sec: 1500,
};

const ZONES: &[(&str, &str, u8)] = &[
    ("zone_0", "Left Secondary", 0x00),
    ("zone_1", "Right Secondary", 0x01),
    ("zone_2", "Left Primary", 0x02),
    ("zone_3", "Right Primary", 0x03),
];

fn zone_byte(zone_id: &str) -> Option<u8> {
    ZONES
        .iter()
        .find(|(id, _, _)| *id == zone_id)
        .map(|(_, _, b)| *b)
}

fn build_descriptor() -> RgbDescriptor {
    let zones = ZONES
        .iter()
        .enumerate()
        .map(|(i, (id, name, _))| RgbZone {
            id: id.to_string(),
            name: name.to_string(),
            topology: ZoneTopology::Linear,
            leds: vec![LedPosition {
                id: i as u32,
                x: 0.5,
                y: 0.5,
            }],
        })
        .collect();
    RgbDescriptor {
        zones,
        native_effects: vec![],
    }
}

struct SubwooferSlot(AtomicU8);

impl Default for SubwooferSlot {
    fn default() -> Self {
        SubwooferSlot(AtomicU8::new(50))
    }
}

impl SubwooferSlot {
    fn get(&self) -> u8 {
        self.0.load(Ordering::Relaxed)
    }
    fn set(&self, v: u8) {
        self.0.store(v, Ordering::Relaxed);
    }
}

pub struct LogitechG560 {
    id: String,
    serial_number: Option<String>,
    messenger: Arc<HidppMessenger>,
    descriptor: RgbDescriptor,
    rgb: RgbStateSlot,
    subwoofer_slot: SubwooferSlot,
    visibility: VisibilitySlot,
    range_cache: RangeStateCache,
}

impl LogitechG560 {
    pub fn new(path: &str, serial: Option<&str>, idx: usize) -> Result<Self> {
        let transport = HidTransport::open(path, None, 50, false, Some(G560_WRITE_RATE))?;
        let messenger = Arc::new(HidppMessenger::new(transport));
        Ok(Self {
            id: build_device_id("logitech_g560", serial, idx),
            serial_number: stable_serial(serial),
            messenger,
            descriptor: build_descriptor(),
            rgb: RgbStateSlot::default(),
            subwoofer_slot: SubwooferSlot::default(),
            visibility: VisibilitySlot::default(),
            range_cache: RangeStateCache::default(),
        })
    }

    async fn send_zone_color(&self, zone: u8, c: RgbColor) -> Result<()> {
        self.messenger
            .hidpp_long_fire(0xFF, 0x04, 0x3A, &[zone, 0x01, c.r, c.g, c.b, 0x02])
            .await
    }

    async fn send_subwoofer_volume(&self, vol: u8) -> Result<()> {
        self.messenger
            .hidpp_long_fire(0xFF, 0x09, 0x1C, &[vol.min(100)])
            .await
    }

    async fn apply_state(&self, state: &RgbState) -> Result<()> {
        match state {
            RgbState::Static { color } => {
                for (_, _, zone_byte) in ZONES {
                    self.send_zone_color(*zone_byte, *color).await?;
                }
            }
            RgbState::PerLed { zones } => {
                for (zone_id, _, byte) in ZONES {
                    if let Some(leds) = zones.get(*zone_id) {
                        if let Some(color) = leds.values().next() {
                            self.send_zone_color(*byte, *color).await?;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[async_trait]
impl Device for LogitechG560 {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        "G560 Gaming Speakers"
    }
    fn vendor(&self) -> &str {
        "Logitech"
    }
    fn model(&self) -> &str {
        "G560"
    }

    async fn initialize(&self) -> Result<bool> {
        self.messenger.start_listener();
        log::info!("[LogitechG560] Initialized (id={})", self.id);
        Ok(true)
    }

    async fn close(&self) {}

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Speaker
    }

    fn wire_serial_number(&self) -> Option<String> {
        self.serial_number.clone()
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![CapabilityRef::Rgb(self), CapabilityRef::Range(self)]
    }

    fn write_rate_status(&self) -> Option<halod_shared::types::WriteRateStatus> {
        self.messenger.rate_status()
    }
}

#[async_trait]
impl RgbCapability for LogitechG560 {
    fn descriptor(&self) -> &RgbDescriptor {
        &self.descriptor
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
        if let (Some(byte), Some(&color)) = (zone_byte(zone_id), colors.first()) {
            self.send_zone_color(byte, color).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl RangeCapability for LogitechG560 {
    async fn to_wire(&self) -> Option<DeviceCapability> {
        Some(DeviceCapability::Range(vec![Range {
            key: "subwoofer_volume".into(),
            label: "Subwoofer Volume".into(),
            min: 0,
            max: 100,
            step: 1,
            value: self.subwoofer_slot.get() as i32,
            read_only: false,
            category: "Audio".into(),
            start_label: None,
            end_label: None,
            display: Default::default(),
            visible_when: None,
        }]))
    }

    fn range_cache(&self) -> &RangeStateCache {
        &self.range_cache
    }

    async fn set_range(&self, key: &str, value: i32) -> Result<()> {
        self.range_cache.record(key, value);
        match key {
            "subwoofer_volume" => {
                let v = value.clamp(0, 100) as u8;
                self.send_subwoofer_volume(v).await?;
                self.subwoofer_slot.set(v);
                Ok(())
            }
            other => Err(anyhow!("unknown range key: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::vendors::logitech::protocols::hidpp::{build_packet, LONG_LEN};

    #[test]
    fn test_g560_packet() {
        let zone = 0x02u8;
        let (r, g, b) = (255u8, 0u8, 128u8);
        let pkt = build_packet(0xFF, 0x04, 0x3A, &[zone, 0x01, r, g, b, 0x02], true);
        assert_eq!(pkt.len(), LONG_LEN);
        assert_eq!(
            &pkt[..10],
            &[0x11, 0xFF, 0x04, 0x3A, 0x02, 0x01, 255, 0, 128, 0x02]
        );
        assert!(pkt[10..].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_g560_subwoofer_packet() {
        // Subwoofer volume command: [0x11, 0xFF, 0x09, 0x1C, vol, 0x00×15]
        let pkt = build_packet(0xFF, 0x09, 0x1C, &[100], true);
        assert_eq!(pkt.len(), LONG_LEN);
        assert_eq!(&pkt[..5], &[0x11, 0xFF, 0x09, 0x1C, 100]);
        assert!(pkt[5..].iter().all(|&b| b == 0));

        let muted = build_packet(0xFF, 0x09, 0x1C, &[0], true);
        assert_eq!(&muted[..5], &[0x11, 0xFF, 0x09, 0x1C, 0]);
    }

    #[test]
    fn test_g560_descriptor() {
        let desc = build_descriptor();
        assert_eq!(desc.zones.len(), 4);
        assert_eq!(desc.zones[0].id, "zone_0");
        assert_eq!(desc.zones[1].id, "zone_1");
        assert_eq!(desc.zones[2].id, "zone_2");
        assert_eq!(desc.zones[3].id, "zone_3");
        assert!(desc.native_effects.is_empty());
        assert_eq!(zone_byte("zone_2"), Some(0x02));
        assert_eq!(zone_byte("zone_99"), None);
    }
}
