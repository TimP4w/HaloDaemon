use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, OnceLock};

use crate::{
    drivers::transports::smbus::{downcast_smbus_device, SmbusBusKind},
    drivers::vendors::generic::devices::common::{effect_color, effect_str, linear_rgb_zone},
    drivers::vendors::zotac::protocols::spectra_blackwell::{
        Direction, Mode, ZoneFrame, ZotacBlackwellProtocol, ZOTAC_ADDR,
    },
    drivers::{CapabilityRef, Device, RgbCapability, RgbStateSlot, VisibilitySlot},
    registry::discovery::{DeviceDescriptor, DiscoveryHandle, SmBusScanEntry},
};
use halod_shared::types::{
    DeviceType, EffectParamDescriptor, EffectParamValue, NativeEffect, ParamKind, RgbColor,
    RgbDescriptor, RgbState,
};

static ZOTAC_ADDRESSES: [u8; 1] = [ZOTAC_ADDR];

inventory::submit!(DeviceDescriptor {
    matches: |h| matches!(h, DiscoveryHandle::Smbus {
        bus_kind: SmbusBusKind::Gpu, addr, ..
    } if ZOTAC_ADDRESSES.contains(addr)),
    make: |h| {
        let DiscoveryHandle::Smbus { bus, addr, .. } = h else {
            anyhow::bail!("descriptor matched non-SMBus handle");
        };
        Ok(
            Arc::new(ZotacSpectraGpu::new(downcast_smbus_device(bus), addr))
                as Arc<dyn crate::drivers::Device>,
        )
    },
});

inventory::submit!(SmBusScanEntry {
    bus_kind: SmbusBusKind::Gpu,
    addresses: &ZOTAC_ADDRESSES,
    pre_scan: None,
    write_rate_limit: None,
});

/// One hardware lighting zone.
struct ZoneDef {
    id: &'static str,
    name: &'static str,
    index: u8,
}

const ZONES: &[ZoneDef] = &[
    ZoneDef {
        id: "logo",
        name: "Logo",
        index: 0x00,
    },
    ZoneDef {
        id: "side_bar",
        name: "Side Bar",
        index: 0x01,
    },
    ZoneDef {
        id: "infinity_mirror",
        name: "Infinity Mirror",
        index: 0x02,
    },
];

struct EffectDef {
    id: &'static str,
    name: &'static str,
    mode: Mode,
    color: bool,
    direction: bool,
}

const EFFECTS: &[EffectDef] = &[
    EffectDef {
        id: "breathe",
        name: "Breathe",
        mode: Mode::Breathe,
        color: true,
        direction: false,
    },
    EffectDef {
        id: "fade",
        name: "Fade",
        mode: Mode::Fade,
        color: false,
        direction: false,
    },
    EffectDef {
        id: "wink",
        name: "Wink",
        mode: Mode::Wink,
        color: true,
        direction: false,
    },
    EffectDef {
        id: "glide",
        name: "Glide",
        mode: Mode::Glide,
        color: true,
        direction: true,
    },
    EffectDef {
        id: "prism",
        name: "Prism",
        mode: Mode::Prism,
        color: false,
        direction: true,
    },
    EffectDef {
        id: "bokeh",
        name: "Bokeh",
        mode: Mode::Bokeh,
        color: true,
        direction: false,
    },
    EffectDef {
        id: "beacon",
        name: "Beacon",
        mode: Mode::Beacon,
        color: true,
        direction: false,
    },
    EffectDef {
        id: "tandem",
        name: "Tandem",
        mode: Mode::Tandem,
        color: true,
        direction: false,
    },
    EffectDef {
        id: "tidal",
        name: "Tidal",
        mode: Mode::Tidal,
        color: true,
        direction: true,
    },
    EffectDef {
        id: "astra",
        name: "Astra",
        mode: Mode::Astra,
        color: true,
        direction: false,
    },
    EffectDef {
        id: "cosmic",
        name: "Cosmic",
        mode: Mode::Cosmic,
        color: true,
        direction: false,
    },
    EffectDef {
        id: "volta",
        name: "Volta",
        mode: Mode::Volta,
        color: true,
        direction: false,
    },
];

const FULL_BRIGHTNESS: u8 = 100;
const DEFAULT_SPEED: u8 = 50;

fn effect_by_id(id: &str) -> Option<&'static EffectDef> {
    EFFECTS.iter().find(|e| e.id == id)
}

fn effect_speed(params: &std::collections::HashMap<String, EffectParamValue>) -> u8 {
    match params.get("speed") {
        Some(EffectParamValue::Float(f)) => f.round().clamp(0.0, 100.0) as u8,
        _ => DEFAULT_SPEED,
    }
}

fn effect_direction(params: &std::collections::HashMap<String, EffectParamValue>) -> Direction {
    match effect_str(params, "direction") {
        Some("right") => Direction::Right,
        _ => Direction::Left,
    }
}

pub struct ZotacSpectraGpu {
    proto: ZotacBlackwellProtocol,
    id: String,
    rgb_descriptor: OnceLock<RgbDescriptor>,
    rgb: RgbStateSlot,
    visibility: VisibilitySlot,
}

static EMPTY_DESCRIPTOR: OnceLock<RgbDescriptor> = OnceLock::new();

impl ZotacSpectraGpu {
    pub fn new(bus: Arc<crate::drivers::transports::smbus::SmBusDevice>, addr: u8) -> Self {
        let proto = ZotacBlackwellProtocol::new(bus, addr);
        let id = format!(
            "zotac-spectra-gpu-bus{}-addr{:02x}",
            proto.bus_number(),
            proto.addr()
        );
        Self {
            proto,
            id,
            rgb_descriptor: OnceLock::new(),
            rgb: RgbStateSlot::default(),
            visibility: VisibilitySlot::default(),
        }
    }

    fn uninitialized_descriptor() -> &'static RgbDescriptor {
        EMPTY_DESCRIPTOR.get_or_init(|| RgbDescriptor {
            zones: vec![],
            native_effects: vec![],
        })
    }

    fn build_descriptor() -> RgbDescriptor {
        let zones = ZONES
            .iter()
            .map(|z| linear_rgb_zone(z.id, z.name, 1))
            .collect();

        let speed_param = EffectParamDescriptor {
            id: "speed".to_string(),
            label: "Speed".to_string(),
            kind: ParamKind::Range {
                min: 0.0,
                max: 100.0,
                step: 1.0,
            },
            default: EffectParamValue::Float(DEFAULT_SPEED as f64),
        };
        let color_param = EffectParamDescriptor {
            id: "color".to_string(),
            label: "Color".to_string(),
            kind: ParamKind::Color,
            default: EffectParamValue::Color(RgbColor { r: 255, g: 0, b: 0 }),
        };
        let direction_param = EffectParamDescriptor {
            id: "direction".to_string(),
            label: "Direction".to_string(),
            kind: ParamKind::Enum {
                options: vec!["left".to_string(), "right".to_string()],
            },
            default: EffectParamValue::Str("left".to_string()),
        };

        let native_effects = EFFECTS
            .iter()
            .map(|e| {
                let mut params = Vec::new();
                if e.color {
                    params.push(color_param.clone());
                }
                params.push(speed_param.clone());
                if e.direction {
                    params.push(direction_param.clone());
                }
                NativeEffect {
                    id: e.id.to_string(),
                    name: e.name.to_string(),
                    params,
                }
            })
            .collect();

        RgbDescriptor {
            zones,
            native_effects,
        }
    }

    async fn apply_static(&self, color: RgbColor) -> Result<()> {
        let frames: Vec<ZoneFrame> = ZONES
            .iter()
            .map(|z| ZoneFrame {
                zone: z.index,
                mode: Mode::Static,
                color1: color,
                color2: RgbColor::default(),
                brightness: FULL_BRIGHTNESS,
                speed: 0,
                direction: Direction::Left,
            })
            .collect();
        self.proto.apply_zones(&frames).await
    }

    async fn apply_state(&self, state: RgbState) -> Result<()> {
        match &state {
            RgbState::Static { color } => {
                self.apply_static(*color).await?;
            }
            RgbState::PerLed { zones } => {
                let frames: Vec<ZoneFrame> = ZONES
                    .iter()
                    .map(|z| {
                        let color = zones
                            .get(z.id)
                            .and_then(|leds| leds.get("0"))
                            .copied()
                            .unwrap_or_default();
                        ZoneFrame {
                            zone: z.index,
                            mode: Mode::Static,
                            color1: color,
                            color2: RgbColor::default(),
                            brightness: FULL_BRIGHTNESS,
                            speed: 0,
                            direction: Direction::Left,
                        }
                    })
                    .collect();
                self.proto.apply_zones(&frames).await?;
            }
            RgbState::NativeEffect { id, params } => {
                let effect = effect_by_id(id)
                    .ok_or_else(|| anyhow::anyhow!("unknown native effect: {}", id))?;
                let color1 =
                    effect_color(params, "color").unwrap_or(RgbColor { r: 255, g: 0, b: 0 });
                let speed = effect_speed(params);
                let direction = effect_direction(params);
                let frames: Vec<ZoneFrame> = ZONES
                    .iter()
                    .map(|z| ZoneFrame {
                        zone: z.index,
                        mode: effect.mode,
                        color1,
                        color2: RgbColor::default(),
                        brightness: FULL_BRIGHTNESS,
                        speed,
                        direction,
                    })
                    .collect();
                self.proto.apply_zones(&frames).await?;
            }
            RgbState::Engine | RgbState::DirectEffect { .. } => {}
        }
        self.rgb.set_state(Some(state));
        Ok(())
    }
}

#[async_trait]
impl Device for ZotacSpectraGpu {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        "Zotac SPECTRA GPU RGB"
    }

    fn vendor(&self) -> &str {
        "Zotac"
    }

    fn model(&self) -> &str {
        "Blackwell SPECTRA 2.0"
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Gpu
    }

    async fn initialize(&self) -> Result<bool> {
        if !self.proto.detect().await {
            return Ok(false);
        }
        log::info!(
            "[Zotac] Found SPECTRA GPU RGB on bus {} addr 0x{:02X}",
            self.proto.bus_number(),
            self.proto.addr()
        );
        if self.rgb_descriptor.set(Self::build_descriptor()).is_err() {
            log::warn!("ZotacSpectraGpu: rgb_descriptor already initialized");
        }
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
        Some("smbus_gpu")
    }

    fn write_rate_status(&self) -> Option<halod_shared::types::WriteRateStatus> {
        self.proto.bus.rate_status()
    }

    fn debug_info_extra(&self) -> Vec<(String, String)> {
        vec![
            ("bus".to_string(), self.proto.bus_number().to_string()),
            (
                "address".to_string(),
                format!("0x{:02x}", self.proto.addr()),
            ),
            (
                "protocol".to_string(),
                "Zotac SPECTRA Blackwell".to_string(),
            ),
        ]
    }
}

#[async_trait]
impl RgbCapability for ZotacSpectraGpu {
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
        let z = ZONES
            .iter()
            .find(|z| z.id == zone_id)
            .ok_or_else(|| anyhow::anyhow!("unknown zone: {}", zone_id))?;
        let color = colors.first().copied().unwrap_or_default();
        self.proto
            .apply_zones(&[ZoneFrame {
                zone: z.index,
                mode: Mode::Static,
                color1: color,
                color2: RgbColor::default(),
                brightness: FULL_BRIGHTNESS,
                speed: 0,
                direction: Direction::Left,
            }])
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_has_three_zones() {
        let d = ZotacSpectraGpu::build_descriptor();
        assert_eq!(d.zones.len(), 3);
        let ids: Vec<&str> = d.zones.iter().map(|z| z.id.as_str()).collect();
        assert_eq!(ids, vec!["logo", "side_bar", "infinity_mirror"]);
        for z in &d.zones {
            assert_eq!(z.leds.len(), 1, "each zone is a single LED");
        }
    }

    #[test]
    fn descriptor_exposes_all_effects() {
        let d = ZotacSpectraGpu::build_descriptor();
        let ids: Vec<&str> = d.native_effects.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), EFFECTS.len());
        assert!(ids.contains(&"breathe"));
        assert!(ids.contains(&"volta"));
        assert!(!ids.contains(&"static"));
    }

    #[test]
    fn effect_param_flags_drive_descriptor() {
        let d = ZotacSpectraGpu::build_descriptor();
        let by = |id: &str| d.native_effects.iter().find(|e| e.id == id).unwrap();

        // Fade: speed only.
        let fade: Vec<&str> = by("fade").params.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(fade, vec!["speed"]);

        // Glide: color + speed + direction.
        let glide: Vec<&str> = by("glide").params.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(glide, vec!["color", "speed", "direction"]);

        // Prism: rainbow, so speed + direction, no color.
        let prism: Vec<&str> = by("prism").params.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(prism, vec!["speed", "direction"]);
    }

    #[test]
    fn effect_speed_reads_float_and_clamps() {
        use std::collections::HashMap;
        let mut p = HashMap::new();
        assert_eq!(effect_speed(&p), DEFAULT_SPEED);
        p.insert("speed".to_string(), EffectParamValue::Float(80.0));
        assert_eq!(effect_speed(&p), 80);
        p.insert("speed".to_string(), EffectParamValue::Float(250.0));
        assert_eq!(effect_speed(&p), 100);
    }

    #[test]
    fn effect_direction_defaults_left() {
        use std::collections::HashMap;
        let mut p = HashMap::new();
        assert_eq!(effect_direction(&p), Direction::Left);
        p.insert(
            "direction".to_string(),
            EffectParamValue::Str("right".to_string()),
        );
        assert_eq!(effect_direction(&p), Direction::Right);
    }

    #[test]
    fn unknown_effect_id_is_none() {
        assert!(effect_by_id("nope").is_none());
        assert!(effect_by_id("volta").is_some());
    }
}
