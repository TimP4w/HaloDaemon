use anyhow::Result;
use async_trait::async_trait;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock},
};

const ENE_GPU_ADDRESS: u8 = 0x67;
const ENE_RAM_ADDRESSES: &[u8] = &[
    0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x4F, 0x66, 0x67, 0x39, 0x3A, 0x3B, 0x3C, 0x3D,
];

use crate::{
    discovery::{DeviceDescriptor, DiscoveryHandle, SmBusScanEntry},
    drivers::transports::smbus::{SmBusDevice, SmbusBusKind},
    drivers::vendors::asus::protocols::ene_smbus::{
        remap_dram_addresses, EneDeviceInfo, EneMode, EneSmBusProtocol, EneSpeed,
    },
    drivers::{
        vendors::generic::devices::common::{
            effect_color, effect_str, linear_rgb_zone, per_led_frame,
        },
        CapabilityRef, Device, RgbCapability, RgbStateSlot, VisibilitySlot,
    },
};
use halod_protocol::types::{
    DeviceType, EffectParamDescriptor, EffectParamValue, NativeEffect, ParamKind, RgbColor,
    RgbDescriptor, RgbState,
};
use halod_protocol::zone_transform::transform_colors;

// ── ENE inventory registrations ───────────────────────────────────────────────

static ENE_GPU_ADDRESSES: [u8; 1] = [ENE_GPU_ADDRESS];

async fn ene_dram_pre_scan(bus: Arc<SmBusDevice>) -> Result<()> {
    remap_dram_addresses(&bus, ENE_RAM_ADDRESSES).await
}

fn ene_dram_pre_scan_boxed(
    bus: Arc<SmBusDevice>,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
    Box::pin(ene_dram_pre_scan(bus))
}

inventory::submit!(DeviceDescriptor {
    matches: |h| matches!(h, DiscoveryHandle::Smbus {
        bus_kind: SmbusBusKind::Chipset, addr, ..
    } if ENE_RAM_ADDRESSES.contains(addr)),
    make: |h| {
        let DiscoveryHandle::Smbus { bus, addr, .. } = h else {
            anyhow::bail!("descriptor matched non-SMBus handle");
        };
        Ok(
            Arc::new(EneRgbDevice::new_dram_uninitialized(Arc::clone(bus), addr))
                as Arc<dyn crate::drivers::Device>,
        )
    },
});

inventory::submit!(SmBusScanEntry {
    bus_kind: SmbusBusKind::Chipset,
    addresses: ENE_RAM_ADDRESSES,
    pre_scan: Some(ene_dram_pre_scan_boxed),
});

inventory::submit!(DeviceDescriptor {
    matches: |h| matches!(h, DiscoveryHandle::Smbus {
        bus_kind: SmbusBusKind::Gpu, addr, ..
    } if ENE_GPU_ADDRESSES.contains(addr)),
    make: |h| {
        let DiscoveryHandle::Smbus { bus, addr, .. } = h else {
            anyhow::bail!("descriptor matched non-SMBus handle");
        };
        Ok(
            Arc::new(EneRgbDevice::new_gpu_uninitialized(Arc::clone(bus), addr))
                as Arc<dyn crate::drivers::Device>,
        )
    },
});

inventory::submit!(SmBusScanEntry {
    bus_kind: SmbusBusKind::Gpu,
    addresses: &ENE_GPU_ADDRESSES,
    pre_scan: None,
});

// ── Device kind ───────────────────────────────────────────────────────────────

pub enum EneDeviceKind {
    Dram,
    Gpu,
}

const LED_ZONE_ID: &str = "leds";

pub struct EneRgbDevice {
    proto: EneSmBusProtocol,
    info: OnceLock<EneDeviceInfo>,
    kind: EneDeviceKind,
    rgb_descriptor: OnceLock<RgbDescriptor>,
    rgb: RgbStateSlot,
    visibility: VisibilitySlot,
}

impl EneRgbDevice {
    pub fn new_dram_uninitialized(bus: Arc<SmBusDevice>, addr: u8) -> Self {
        Self {
            proto: EneSmBusProtocol::new(bus, addr),
            info: OnceLock::new(),
            kind: EneDeviceKind::Dram,
            rgb_descriptor: OnceLock::new(),
            rgb: RgbStateSlot::default(),
            visibility: VisibilitySlot::default(),
        }
    }

    pub fn new_gpu_uninitialized(bus: Arc<SmBusDevice>, addr: u8) -> Self {
        Self {
            proto: EneSmBusProtocol::new(bus, addr),
            info: OnceLock::new(),
            kind: EneDeviceKind::Gpu,
            rgb_descriptor: OnceLock::new(),
            rgb: RgbStateSlot::default(),
            visibility: VisibilitySlot::default(),
        }
    }

    fn kind_str(&self) -> &'static str {
        match self.kind {
            EneDeviceKind::Dram => "dram",
            EneDeviceKind::Gpu => "gpu",
        }
    }

    fn build_descriptor(led_count: usize) -> RgbDescriptor {
        let speed_options = vec![
            "fastest".to_string(),
            "fast".to_string(),
            "normal".to_string(),
            "slow".to_string(),
            "slowest".to_string(),
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
                        default: EffectParamValue::Color(RgbColor { r: 255, g: 0, b: 0 }),
                    },
                    EffectParamDescriptor {
                        id: "speed".to_string(),
                        label: "Speed".to_string(),
                        kind: ParamKind::Enum {
                            options: speed_options.clone(),
                        },
                        default: EffectParamValue::Str("normal".to_string()),
                    },
                ],
            },
            NativeEffect {
                id: "spectrum_wave".to_string(),
                name: "Spectrum Wave".to_string(),
                params: vec![EffectParamDescriptor {
                    id: "speed".to_string(),
                    label: "Speed".to_string(),
                    kind: ParamKind::Enum {
                        options: speed_options,
                    },
                    default: EffectParamValue::Str("normal".to_string()),
                }],
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
            .expect("EneRgbDevice used before initialize()");
        let rgb_descriptor = self
            .rgb_descriptor
            .get()
            .expect("EneRgbDevice used before initialize()");
        match &state {
            RgbState::Static { color } => {
                self.proto.set_direct_mode(true).await?;
                self.proto
                    .set_static_color(info, color.r, color.g, color.b)
                    .await?;
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
                    self.proto.set_direct_mode(true).await?;
                    self.proto.set_colors(info, &colors).await?;
                }
            }
            RgbState::NativeEffect { id, params } => {
                let speed = effect_str(params, "speed").unwrap_or("normal");
                let ene_speed = parse_speed(speed);

                match id.as_str() {
                    "breathing" => {
                        let color = effect_color(params, "color").unwrap_or(RgbColor {
                            r: 255,
                            g: 0,
                            b: 0,
                        });
                        let colors = vec![[color.r, color.g, color.b]; info.led_count];
                        self.proto.set_direct_mode(false).await?;
                        self.proto.set_effect_colors(info, &colors).await?;
                        self.proto
                            .set_mode(EneMode::Breathing, ene_speed, 0)
                            .await?;
                    }
                    "spectrum_wave" => {
                        self.proto.set_direct_mode(false).await?;
                        self.proto
                            .set_mode(EneMode::SpectrumCycleWave, ene_speed, 0)
                            .await?;
                    }
                    "off" => {
                        self.proto
                            .set_mode(EneMode::Off, EneSpeed::Normal, 0)
                            .await?;
                    }
                    other => {
                        anyhow::bail!("Unknown native effect: {}", other);
                    }
                }
            }
            RgbState::Engine => {
                // Canvas engine drives frames; nothing to send now.
            }
        }
        self.rgb.set_state(Some(state));
        Ok(())
    }
}

fn parse_speed(s: &str) -> EneSpeed {
    match s {
        "fastest" => EneSpeed::Fastest,
        "fast" => EneSpeed::Fast,
        "slow" => EneSpeed::Slow,
        "slowest" => EneSpeed::Slowest,
        _ => EneSpeed::Normal,
    }
}

#[async_trait]
impl Device for EneRgbDevice {
    fn id(&self) -> String {
        format!(
            "ene-{}-bus{}-addr{:02x}",
            self.kind_str(),
            self.proto.bus_number(),
            self.proto.addr()
        )
    }

    fn name(&self) -> &str {
        match self.kind {
            EneDeviceKind::Dram => "ENE DRAM RGB",
            EneDeviceKind::Gpu => "ASUS GPU RGB",
        }
    }

    fn wire_device_type(&self) -> DeviceType {
        match self.kind {
            EneDeviceKind::Dram => DeviceType::Ram,
            EneDeviceKind::Gpu => DeviceType::Gpu,
        }
    }

    fn vendor(&self) -> &str {
        "ASUS/ENE"
    }

    fn model(&self) -> &str {
        self.info.get().map(|i| i.version.as_str()).unwrap_or("")
    }

    async fn initialize(&self) -> Result<bool> {
        if !self.proto.test().await {
            return Ok(false);
        }
        let info = self.proto.build_device().await?;
        log::info!(
            "[ENE] Found {} on bus {} addr 0x{:02X} ({}, {} LEDs)",
            self.name(),
            self.proto.bus_number(),
            self.proto.addr(),
            info.version,
            info.led_count
        );
        let descriptor = Self::build_descriptor(info.led_count);
        let _ = self.rgb_descriptor.set(descriptor);
        let _ = self.info.set(info);
        self.proto.set_direct_mode(true).await?;
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
        // GPU ENE controllers sit on a `SmbusBusKind::Gpu` bus: served by NvAPI
        // on Windows, by the regular /dev/i2c-* node filtered for GPU
        // subordinates on Linux. Use a neutral label so the same value renders
        // on both platforms.
        Some(match self.kind {
            EneDeviceKind::Gpu => "smbus_gpu",
            EneDeviceKind::Dram => "smbus",
        })
    }

    fn debug_info_extra(&self) -> Vec<(String, String)> {
        let mut items = vec![
            ("kind".to_string(), self.kind_str().to_string()),
            ("bus".to_string(), self.proto.bus_number().to_string()),
            (
                "address".to_string(),
                format!("0x{:02x}", self.proto.addr()),
            ),
            ("protocol".to_string(), "ASUS Aura SMBus (ENE)".to_string()),
        ];
        if let Some(info) = self.info.get() {
            items.push(("firmware".to_string(), info.version.clone()));
            items.push(("led_count".to_string(), info.led_count.to_string()));
        }
        items
    }
}

#[async_trait]
impl RgbCapability for EneRgbDevice {
    fn descriptor(&self) -> &RgbDescriptor {
        self.rgb_descriptor
            .get()
            .expect("EneRgbDevice used before initialize()")
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
            .expect("EneRgbDevice used before initialize()");
        let buf: Vec<[u8; 3]> = colors.iter().map(|c| [c.r, c.g, c.b]).collect();
        self.proto.write_frame_colors(info, &buf).await
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use halod_protocol::types::ZoneTopology;

    #[test]
    fn test_build_descriptor_led_count() {
        let desc = EneRgbDevice::build_descriptor(8);
        assert_eq!(desc.zones[0].leds.len(), 8);
        assert_eq!(desc.zones[0].id, "leds");
        assert!(matches!(desc.zones[0].topology, ZoneTopology::Linear));
    }

    #[test]
    fn test_build_descriptor_single_led_position() {
        let desc = EneRgbDevice::build_descriptor(1);
        let led = &desc.zones[0].leds[0];
        assert!((led.x - 0.5).abs() < f32::EPSILON);
        assert!((led.y - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_build_descriptor_multi_led_positions() {
        let desc = EneRgbDevice::build_descriptor(5);
        let leds = &desc.zones[0].leds;
        assert!((leds[0].x - 0.0).abs() < f32::EPSILON);
        assert!((leds[4].x - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_build_descriptor_has_native_effects() {
        let desc = EneRgbDevice::build_descriptor(4);
        let ids: Vec<&str> = desc.native_effects.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"breathing"));
        assert!(ids.contains(&"spectrum_wave"));
        assert!(ids.contains(&"off"));
    }

    #[test]
    fn test_parse_speed() {
        assert!(matches!(parse_speed("fastest"), EneSpeed::Fastest));
        assert!(matches!(parse_speed("normal"), EneSpeed::Normal));
        assert!(matches!(parse_speed("slowest"), EneSpeed::Slowest));
        assert!(matches!(parse_speed("unknown"), EneSpeed::Normal));
    }
}
