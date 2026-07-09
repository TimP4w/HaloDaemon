// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, OnceLock};

use crate::{
    drivers::transports::smbus::{downcast_smbus_device, SmBusDevice, SmbusBusKind},
    drivers::vendors::corsair::protocols::corsair_dram::{
        corsair_direction_from_str, corsair_mode_from_id, corsair_speed_from_str, CorsairDramInfo,
        CorsairDramProtocol, NativeEffectParams,
    },
    drivers::{
        vendors::generic::devices::common::{effect_str, linear_rgb_zone, per_led_frame},
        CapabilityRef, Device, RgbCapability, RgbStateSlot, VisibilitySlot,
    },
    registry::discovery::{DeviceDescriptor, DiscoveryHandle, SmBusScanEntry},
};
use halod_shared::types::{
    EffectParamDescriptor, EffectParamValue, NativeEffect, ParamKind, RgbColor, RgbDescriptor,
    RgbState,
};
use halod_shared::zone_transform::transform_colors;

static CORSAIR_DRAM_ADDRESSES: [u8; 16] = [
    0x58, 0x59, 0x5A, 0x5B, 0x5C, 0x5D, 0x5E, 0x5F, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
];

inventory::submit!(DeviceDescriptor {
    matches: |h| matches!(h, DiscoveryHandle::Smbus {
        bus_kind: SmbusBusKind::Chipset, addr, ..
    } if CORSAIR_DRAM_ADDRESSES.contains(addr)),
    make: |h| {
        let DiscoveryHandle::Smbus { bus, addr, .. } = h else {
            anyhow::bail!("descriptor matched non-SMBus handle");
        };
        Ok(Arc::new(CorsairDramDevice::new_uninitialized(
            downcast_smbus_device(bus),
            addr,
        )) as Arc<dyn crate::drivers::Device>)
    },
});

inventory::submit!(SmBusScanEntry {
    bus_kind: SmbusBusKind::Chipset,
    addresses: &CORSAIR_DRAM_ADDRESSES,
    pre_scan: None,
    write_rate_limit: None,
});

const LED_ZONE_ID: &str = "leds";

static EMPTY_CORSAIR_DESCRIPTOR: std::sync::OnceLock<RgbDescriptor> = std::sync::OnceLock::new();

pub struct CorsairDramDevice {
    protocol: CorsairDramProtocol,
    info: OnceLock<CorsairDramInfo>,
    id: String,
    rgb_descriptor: OnceLock<RgbDescriptor>,
    rgb: RgbStateSlot,
    visibility: VisibilitySlot,
}

impl CorsairDramDevice {
    fn uninitialized_descriptor() -> &'static RgbDescriptor {
        EMPTY_CORSAIR_DESCRIPTOR.get_or_init(|| RgbDescriptor {
            zones: vec![],
            native_effects: vec![],
        })
    }

    pub fn new_uninitialized(bus: Arc<SmBusDevice>, addr: u8) -> Self {
        let protocol = CorsairDramProtocol::new(bus, addr);
        let id = format!(
            "corsair-dram-bus{}-addr{:02x}",
            protocol.bus_number(),
            protocol.addr()
        );
        Self {
            protocol,
            info: OnceLock::new(),
            id,
            rgb_descriptor: OnceLock::new(),
            rgb: RgbStateSlot::default(),
            visibility: VisibilitySlot::default(),
        }
    }

    fn build_descriptor(led_count: usize) -> RgbDescriptor {
        let speed_options = vec!["slow".to_string(), "medium".to_string(), "fast".to_string()];

        let direction_options = vec![
            "up".to_string(),
            "down".to_string(),
            "left".to_string(),
            "right".to_string(),
        ];

        let native_effects = vec![
            NativeEffect {
                id: "breathing".to_string(),
                name: "Breathing".to_string(),
                params: vec![
                    EffectParamDescriptor {
                        id: "color".to_string(),
                        label: "Color".to_string(),
                        kind: ParamKind::Color,
                        default: EffectParamValue::Color(RgbColor {
                            r: 0,
                            g: 128,
                            b: 255,
                        }),
                    },
                    EffectParamDescriptor {
                        id: "speed".to_string(),
                        label: "Speed".to_string(),
                        kind: ParamKind::Enum {
                            options: speed_options.clone(),
                        },
                        default: EffectParamValue::Str("medium".to_string()),
                    },
                ],
            },
            NativeEffect {
                id: "rainbow_wave".to_string(),
                name: "Rainbow Wave".to_string(),
                params: vec![
                    EffectParamDescriptor {
                        id: "speed".to_string(),
                        label: "Speed".to_string(),
                        kind: ParamKind::Enum {
                            options: speed_options.clone(),
                        },
                        default: EffectParamValue::Str("medium".to_string()),
                    },
                    EffectParamDescriptor {
                        id: "direction".to_string(),
                        label: "Direction".to_string(),
                        kind: ParamKind::Enum {
                            options: direction_options,
                        },
                        default: EffectParamValue::Str("right".to_string()),
                    },
                ],
            },
            NativeEffect {
                id: "color_shift".to_string(),
                name: "Color Shift".to_string(),
                params: vec![
                    EffectParamDescriptor {
                        id: "color1".to_string(),
                        label: "Color 1".to_string(),
                        kind: ParamKind::Color,
                        default: EffectParamValue::Color(RgbColor { r: 255, g: 0, b: 0 }),
                    },
                    EffectParamDescriptor {
                        id: "color2".to_string(),
                        label: "Color 2".to_string(),
                        kind: ParamKind::Color,
                        default: EffectParamValue::Color(RgbColor { r: 0, g: 0, b: 255 }),
                    },
                    EffectParamDescriptor {
                        id: "speed".to_string(),
                        label: "Speed".to_string(),
                        kind: ParamKind::Enum {
                            options: speed_options,
                        },
                        default: EffectParamValue::Str("medium".to_string()),
                    },
                ],
            },
            NativeEffect {
                id: "off".to_string(),
                name: "Off".to_string(),
                params: vec![],
            },
        ];

        RgbDescriptor {
            zones: vec![linear_rgb_zone(LED_ZONE_ID, "LEDs", led_count)],
            native_effects,
        }
    }

    async fn apply_state(&self, state: RgbState) -> Result<()> {
        let info = self
            .info
            .get()
            .ok_or_else(|| anyhow::anyhow!("CorsairDramDevice used before initialize()"))?;
        let rgb_descriptor = self
            .rgb_descriptor
            .get()
            .ok_or_else(|| anyhow::anyhow!("CorsairDramDevice used before initialize()"))?;
        match &state {
            RgbState::Static { color } => {
                let colors = vec![[color.r, color.g, color.b]; info.led_count];
                self.write_colors(&colors).await?;
            }
            RgbState::PerLed { zones } => {
                if let Some(led_map) = zones.get(LED_ZONE_ID) {
                    let frame = per_led_frame(led_map, info.led_count);
                    let rgb_zone = &rgb_descriptor.zones[0];
                    let transform = self.rgb.transform_for(&rgb_zone.id);
                    let colors: Vec<[u8; 3]> = transform_colors(&frame, rgb_zone, &transform)
                        .into_iter()
                        .map(|c| [c.r, c.g, c.b])
                        .collect();
                    self.write_colors(&colors).await?;
                }
            }
            RgbState::NativeEffect { id, params } => {
                self.apply_native_effect(id, params).await?;
            }
            RgbState::Engine | RgbState::DirectEffect { .. } => {
                // Canvas engine drives frames; nothing to send now.
            }
        }
        self.rgb.set_state(Some(state));
        Ok(())
    }

    async fn write_colors(&self, colors: &[[u8; 3]]) -> Result<()> {
        let info = self
            .info
            .get()
            .ok_or_else(|| anyhow::anyhow!("CorsairDramDevice used before initialize()"))?;
        self.protocol.set_colors(info, colors).await
    }

    async fn apply_native_effect(
        &self,
        id: &str,
        params: &std::collections::HashMap<String, EffectParamValue>,
    ) -> Result<()> {
        let info = self
            .info
            .get()
            .ok_or_else(|| anyhow::anyhow!("CorsairDramDevice used before initialize()"))?;
        let Some(mode) = corsair_mode_from_id(id) else {
            if id == "off" {
                let colors = vec![[0u8; 3]; info.led_count];
                return self.write_colors(&colors).await;
            }
            anyhow::bail!("Unknown native effect: {}", id);
        };
        let speed = corsair_speed_from_str(effect_str(params, "speed").unwrap_or("medium"));
        let direction =
            corsair_direction_from_str(effect_str(params, "direction").unwrap_or("right"));
        let color1 = color_param(params.get("color1").or(params.get("color")));
        let color2 = color_param(params.get("color2"));

        self.protocol
            .set_native_effect(NativeEffectParams {
                mode,
                speed,
                direction,
                color1,
                color2,
                brightness: 255,
                random: false,
            })
            .await
    }
}

fn color_param(v: Option<&EffectParamValue>) -> [u8; 3] {
    match v {
        Some(EffectParamValue::Color(c)) => [c.r, c.g, c.b],
        _ => [255, 255, 255],
    }
}

#[async_trait]
impl Device for CorsairDramDevice {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        self.info
            .get()
            .map(|i| i.model_name)
            .unwrap_or("Corsair DRAM RGB")
    }

    fn vendor(&self) -> &str {
        "Corsair"
    }

    fn model(&self) -> &str {
        self.name()
    }

    async fn initialize(&self) -> Result<bool> {
        if !self.protocol.test().await {
            return Ok(false);
        }
        let info = self.protocol.read_info().await?;
        log::info!(
            "[Corsair DRAM] Found {} on bus {} addr 0x{:02X} ({} LEDs, protocol {})",
            info.model_name,
            self.protocol.bus_number(),
            self.protocol.addr(),
            info.led_count,
            info.protocol_version,
        );
        let descriptor = Self::build_descriptor(info.led_count);
        let _ = self.rgb_descriptor.set(descriptor);
        let _ = self.info.set(info);
        Ok(true)
    }

    async fn close(&self) {}

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![CapabilityRef::Rgb(self)]
    }

    fn debug_transport(&self) -> Option<&'static str> {
        Some("smbus")
    }

    fn write_rate_status(&self) -> Option<halod_shared::types::WriteRateStatus> {
        self.protocol.bus.rate_status()
    }

    fn debug_info_extra(&self) -> Vec<(String, String)> {
        let mut items = vec![
            ("bus".to_string(), self.protocol.bus_number().to_string()),
            (
                "address".to_string(),
                format!("0x{:02x}", self.protocol.addr()),
            ),
            ("protocol".to_string(), "Corsair iCUE SMBus".to_string()),
        ];
        if let Some(info) = self.info.get() {
            items.push(("pid".to_string(), format!("0x{:04x}", info.pid)));
            items.push(("firmware".to_string(), info.firmware.clone()));
            items.push((
                "protocol_version".to_string(),
                info.protocol_version.to_string(),
            ));
            items.push(("led_count".to_string(), info.led_count.to_string()));
        }
        items
    }
}

#[async_trait]
impl RgbCapability for CorsairDramDevice {
    fn descriptor(&self) -> &RgbDescriptor {
        self.rgb_descriptor
            .get()
            .unwrap_or_else(|| Self::uninitialized_descriptor())
    }

    fn rgb_state(&self) -> &RgbStateSlot {
        &self.rgb
    }

    async fn apply(&self, state: RgbState) -> Result<()> {
        self.apply_state(state).await
    }

    async fn write_frame(&self, zone_id: &str, colors: &[RgbColor]) -> Result<()> {
        if zone_id != LED_ZONE_ID {
            anyhow::bail!("unknown zone: {}", zone_id);
        }
        let info = self
            .info
            .get()
            .ok_or_else(|| anyhow::anyhow!("CorsairDramDevice used before initialize()"))?;
        let buf: Vec<[u8; 3]> = colors.iter().map(|c| [c.r, c.g, c.b]).collect();
        // Always use direct mode for animation frames — fastest path regardless of protocol version.
        self.protocol.set_colors_direct(info, &buf).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::ZoneTopology;

    fn make_info(led_count: usize, protocol_version: u8) -> CorsairDramInfo {
        CorsairDramInfo {
            pid: 0x0700,
            led_count,
            reverse: false,
            model_name: "Corsair Vengeance RGB DDR5",
            firmware: "1.0.0".into(),
            protocol_version,
        }
    }

    #[test]
    fn build_descriptor_led_count() {
        let desc = CorsairDramDevice::build_descriptor(10);
        assert_eq!(desc.zones[0].leds.len(), 10);
        assert_eq!(desc.zones[0].id, "leds");
        assert!(matches!(desc.zones[0].topology, ZoneTopology::Linear));
    }

    #[test]
    fn build_descriptor_single_led_centered() {
        let desc = CorsairDramDevice::build_descriptor(1);
        let led = &desc.zones[0].leds[0];
        assert!((led.x - 0.5).abs() < f32::EPSILON);
        assert!((led.y - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn build_descriptor_has_expected_effects() {
        let desc = CorsairDramDevice::build_descriptor(10);
        let ids: Vec<&str> = desc.native_effects.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"breathing"));
        assert!(ids.contains(&"rainbow_wave"));
        assert!(ids.contains(&"color_shift"));
        assert!(ids.contains(&"off"));
    }

    #[test]
    fn build_descriptor_led_count_matches_device_table() {
        use crate::drivers::vendors::corsair::protocols::corsair_dram::device_info_from_pid;
        let (_, led_count, _) = device_info_from_pid(0x0700);
        let desc = CorsairDramDevice::build_descriptor(led_count);
        assert_eq!(desc.zones[0].leds.len(), 10);
    }

    #[test]
    fn speed_from_str_values() {
        use crate::drivers::vendors::corsair::protocols::corsair_dram::{
            corsair_speed_from_str, CorsairDramSpeed,
        };
        assert!(matches!(
            corsair_speed_from_str("slow"),
            CorsairDramSpeed::Slow
        ));
        assert!(matches!(
            corsair_speed_from_str("fast"),
            CorsairDramSpeed::Fast
        ));
        assert!(matches!(
            corsair_speed_from_str("medium"),
            CorsairDramSpeed::Medium
        ));
        assert!(matches!(
            corsair_speed_from_str("unknown"),
            CorsairDramSpeed::Medium
        ));
    }

    #[test]
    fn direction_from_str_values() {
        use crate::drivers::vendors::corsair::protocols::corsair_dram::{
            corsair_direction_from_str, CorsairDramDirection,
        };
        assert!(matches!(
            corsair_direction_from_str("up"),
            CorsairDramDirection::Up
        ));
        assert!(matches!(
            corsair_direction_from_str("down"),
            CorsairDramDirection::Down
        ));
        assert!(matches!(
            corsair_direction_from_str("left"),
            CorsairDramDirection::Left
        ));
        assert!(matches!(
            corsair_direction_from_str("right"),
            CorsairDramDirection::Right
        ));
        assert!(matches!(
            corsair_direction_from_str("other"),
            CorsairDramDirection::Right
        ));
    }

    #[test]
    fn info_protocol_v4_device() {
        let info = make_info(10, 4);
        assert_eq!(info.protocol_version, 4);
        assert_eq!(info.led_count, 10);
    }

    #[test]
    fn descriptor_uninitialized_returns_empty_not_panic() {
        // Regression guard for UH6: descriptor() must return empty RgbDescriptor
        // instead of panicking when called before initialize().
        let desc = CorsairDramDevice::uninitialized_descriptor();
        assert!(desc.zones.is_empty());
        assert!(desc.native_effects.is_empty());
    }
}
