// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: liquidctl contributors <https://github.com/liquidctl/liquidctl>
/// Protocol reference: liquidctl kraken3.py by contributors (GPL-3.0)
///   https://github.com/liquidctl/liquidctl/blob/main/liquidctl/driver/kraken2.py
///   https://github.com/liquidctl/liquidctl/blob/main/liquidctl/driver/kraken3.py
///
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::f32::consts::PI;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::drivers::vendors::nzxt::protocols::{decode_static_image_rgba, KrakenWire, NzxtKrakenProtocol};
use crate::{
    discovery::{DeviceDescriptor, DiscoveryHandle},
    drivers::{
        chain::{ChainAdapter, ChainHost, ChannelDescriptor},
        vendors::generic::devices::common::{build_device_id, per_led_frame, stable_serial, WireDeviceBuilder},
        transports::hid::HidTransport,
        CapabilityRef, ChainCapability, ChainLinkKind, Device, FanCapability, FanStateSlot,
        LcdCapability, LcdStateSlot, NzxtFanHub, RgbCapability, RgbStateSlot, SensorCapability,
        VisibilitySlot,
    },
    state::AppState,
};
use halod_protocol::types::{
    DeviceCapability, DeviceType, LcdDescriptor, LcdMode, LedPosition, PumpStatus, RgbColor,
    RgbDescriptor, RgbState, RgbZone, ScreenShape, Sensor, SensorType, SensorUnit, VisibilityState,
    WireDevice, ZoneTopology,
};
use halod_protocol::zone_transform::transform_colors;

/// Max LEDs the external accessory channel chain may carry. NZXT firmware
/// caps the Kraken's accessory header at 96 LEDs per channel.
pub const MAX_NZXT_KRAKEN_CHAIN_LEDS: u32 = 96;

/// Kraken has a single external accessory header. We expose it as logical
/// channel `0` in the chain IPC; the wire path
/// (`NzxtKrakenProtocol::write_ext_frame`) doesn't take a channel byte.
const EXT_CHAIN_CHANNEL: u8 = 0;

static KRAKEN_IDS: &[(u16, u16)] = &[
    (0x1E71, 0x2007), // NZXT Kraken X53/X63/X73
    (0x1E71, 0x2014), // NZXT Kraken X53/X63/X73
    (0x1E71, 0x3008), // NZXT Kraken Z53/63/73
    (0x1E71, 0x300C), // NZXT Kraken Elite
    (0x1E71, 0x300E), // NZXT Kraken
    (0x1E71, 0x3012), // NZXT Kraken Elite V2 RGB
    (0x1E71, 0x3014), // NZXT Kraken Plus RGB
];

inventory::submit! {
    DeviceDescriptor {
        matches: |h| {
            let DiscoveryHandle::Hid { vid, pid, .. } = h else { return false };
            KRAKEN_IDS.iter().any(|&(v, p)| v == *vid && p == *pid)
        },
        make: |h| {
            let DiscoveryHandle::Hid { path, serial, idx, vid, pid, .. } = h else {
                anyhow::bail!("descriptor matched non-HID handle");
            };
            NZXTKraken::new(path, serial, idx, vid, pid).map(|arc| arc as Arc<dyn Device>)
        },
    }
}

struct KrakenProfile {
    model_name: &'static str,
    wire: KrakenWire,
    has_lcd: bool,
    has_fan_channel: bool,
    /// False for X-series: pump is on the CPU_OPT header, not USB-controlled.
    has_pump_control: bool,
    /// True for X-series: the pump head has a single logo LED zone.
    has_logo: bool,
    ring_led_count: usize,
    /// Panel resolution in pixels; meaningful only when `has_lcd` is true.
    lcd_size: (u32, u32),
}

static KRAKEN_PROFILES: &[(u16, KrakenProfile)] = &[
    (
        0x2007,
        KrakenProfile {
            model_name: "Kraken X53/X63/X73",
            wire: KrakenWire::X3,
            has_lcd: false,
            has_fan_channel: false,
            has_pump_control: false,
            has_logo: true,
            ring_led_count: 8,
            lcd_size: (0, 0),
        },
    ),
    (
        0x2014,
        KrakenProfile {
            model_name: "Kraken X53/X63/X73",
            wire: KrakenWire::X3,
            has_lcd: false,
            has_fan_channel: false,
            has_pump_control: false,
            has_logo: true,
            ring_led_count: 8,
            lcd_size: (0, 0),
        },
    ),
    (
        0x3008,
        KrakenProfile {
            model_name: "Kraken Z53/63/73",
            wire: KrakenWire::ZElite,
            has_lcd: true,
            has_fan_channel: true,
            has_pump_control: true,
            has_logo: false,
            ring_led_count: 24,
            lcd_size: (320, 320),
        },
    ),
    (
        0x300C,
        KrakenProfile {
            model_name: "Kraken Elite 2023",
            wire: KrakenWire::ZElite,
            has_lcd: true,
            has_fan_channel: true,
            has_pump_control: true,
            has_logo: false,
            ring_led_count: 24,
            lcd_size: (640, 640),
        },
    ),
    (
        0x300E,
        KrakenProfile {
            model_name: "Kraken 2023",
            wire: KrakenWire::ZElite,
            has_lcd: true,
            has_fan_channel: true,
            has_pump_control: true,
            has_logo: false,
            ring_led_count: 24,
            lcd_size: (240, 240),
        },
    ),
    (
        0x3012,
        KrakenProfile {
            model_name: "Kraken Elite RGB 2024",
            wire: KrakenWire::ZElite,
            has_lcd: true,
            has_fan_channel: true,
            has_pump_control: true,
            has_logo: false,
            ring_led_count: 24,
            lcd_size: (640, 640),
        },
    ),
    (
        0x3014,
        KrakenProfile {
            model_name: "Kraken Plus 2024",
            wire: KrakenWire::ZElite,
            has_lcd: true,
            has_fan_channel: true,
            has_pump_control: true,
            has_logo: false,
            ring_led_count: 24,
            lcd_size: (240, 240),
        },
    ),
];

fn kraken_profile(pid: u16) -> &'static KrakenProfile {
    KRAKEN_PROFILES
        .iter()
        .find(|(p, _)| *p == pid)
        .map(|(_, prof)| prof)
        .unwrap_or_else(|| {
            KRAKEN_PROFILES
                .iter()
                .find(|(p, _)| *p == 0x3008)
                .map(|(_, prof)| prof)
                .expect("Z63 profile present in table")
        })
}

pub struct NZXTKraken {
    self_ref: Weak<Self>,
    id: String,
    serial_number: Option<String>,
    vid: u16,
    pid: u16,
    model_name: &'static str,
    protocol: NzxtKrakenProtocol<HidTransport>,
    rgb_descriptor: RgbDescriptor,
    rgb: RgbStateSlot,
    lcd_descriptor: LcdDescriptor,
    fan: FanStateSlot,
    lcd: LcdStateSlot,
    visibility: VisibilitySlot,
    sensor_visibility: Mutex<HashMap<String, VisibilityState>>,
    /// Shared chain runtime for the single external accessory channel. Built
    /// in `new()` since the channel set is fixed (one channel: `"0"`).
    chain_host: OnceLock<Arc<ChainHost>>,
}

impl NZXTKraken {
    pub fn new(
        path: &str,
        serial: Option<&str>,
        index: usize,
        vid: u16,
        pid: u16,
    ) -> Result<Arc<Self>> {
        let model_name = Self::model_name_for(pid);
        let protocol = NzxtKrakenProtocol::open(path, kraken_profile(pid).wire)?;
        let id = build_device_id("nzxt_kraken", serial, index);
        let serial_number = stable_serial(serial);
        let arc = Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            id,
            serial_number,
            vid,
            pid,
            model_name,
            protocol,
            rgb_descriptor: Self::build_rgb_descriptor(pid),
            rgb: RgbStateSlot::default(),
            lcd_descriptor: Self::build_lcd_descriptor(pid),
            fan: FanStateSlot::default(),
            lcd: LcdStateSlot::default(),
            visibility: VisibilitySlot::default(),
            sensor_visibility: Mutex::new(HashMap::new()),
            chain_host: OnceLock::new(),
        });
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

    fn model_name_for(pid: u16) -> &'static str {
        kraken_profile(pid).model_name
    }

    fn lcd_size_for(pid: u16) -> (u32, u32) {
        kraken_profile(pid).lcd_size
    }

    fn build_lcd_descriptor(pid: u16) -> LcdDescriptor {
        let (w, h) = Self::lcd_size_for(pid);
        LcdDescriptor {
            shape: ScreenShape::Circle,
            width: w,
            height: h,
            supported_rotations: vec![0, 90, 180, 270],
            supported_image_types: vec![
                "image/png".into(),
                "image/jpeg".into(),
                "image/gif".into(),
            ],
        }
    }

    fn build_rgb_descriptor(pid: u16) -> RgbDescriptor {
        let profile = kraken_profile(pid);
        let count = profile.ring_led_count as u32;
        let ring_leds: Vec<LedPosition> = (0..count)
            .map(|i| {
                // X-series wire slot 0 starts at ~1:30 (top-right); offset by π/4.
                let offset = if profile.has_logo {
                    std::f32::consts::FRAC_PI_4
                } else {
                    0.0
                };
                let angle = 2.0 * PI * i as f32 / count as f32 - PI / 2.0 + offset;
                LedPosition {
                    id: i,
                    x: 0.5 + 0.42 * angle.cos(),
                    y: 0.5 + 0.42 * angle.sin(),
                }
            })
            .collect();
        let mut zones = vec![RgbZone {
            id: "ring".to_string(),
            name: "Ring".to_string(),
            topology: ZoneTopology::Ring,
            leds: ring_leds,
        }];
        if profile.has_logo {
            zones.push(RgbZone {
                id: "logo".to_string(),
                name: "Logo".to_string(),
                topology: ZoneTopology::Linear,
                leds: vec![LedPosition {
                    id: 0,
                    x: 0.5,
                    y: 0.5,
                }],
            });
        }
        RgbDescriptor {
            zones,
            native_effects: vec![],
        }
    }

    async fn apply_state(&self, state: &RgbState) -> Result<()> {
        let profile = kraken_profile(self.pid);
        let ring_count = profile.ring_led_count;
        match state {
            RgbState::Static { color } => {
                let colors = vec![*color; ring_count];
                self.protocol.write_ring_frame(&colors).await?;
                if profile.has_logo {
                    self.protocol.write_logo(*color).await?;
                }
                Ok(())
            }
            RgbState::PerLed { zones } => {
                let zone_leds = zones.get("ring").cloned().unwrap_or_default();
                let colors = per_led_frame(&zone_leds, ring_count);
                let ring_zone = self.rgb_descriptor.zones.iter().find(|z| z.id == "ring");
                let colors = if let Some(z) = ring_zone {
                    let transform = self.rgb.transform_for(&z.id);
                    transform_colors(&colors, z, &transform)
                } else {
                    colors
                };
                self.protocol.write_ring_frame(&colors).await?;
                if profile.has_logo {
                    let logo_leds = zones.get("logo").cloned().unwrap_or_default();
                    let logo_colors = per_led_frame(&logo_leds, 1);
                    let logo_color =
                        logo_colors
                            .into_iter()
                            .next()
                            .unwrap_or(RgbColor { r: 0, g: 0, b: 0 });
                    self.protocol.write_logo(logo_color).await?;
                }
                Ok(())
            }
            RgbState::NativeEffect { .. } | RgbState::Engine => Ok(()),
        }
    }
}

#[async_trait]
impl Device for NZXTKraken {
    fn id(&self) -> String { self.id.clone() }
    fn name(&self) -> &str { "NZXT Kraken" }
    fn vendor(&self) -> &str { "NZXT" }
    fn model(&self) -> &str { self.model_name }

    async fn initialize(&self) -> Result<bool> {
        let fw = self.protocol.initialize().await?;
        log::info!("[NZXT Kraken] Initialized, firmware: {fw}");

        if kraken_profile(self.pid).has_lcd {
            if let Some(lcd) = self.protocol.read_lcd_state().await {
                self.lcd.set_brightness(lcd.0);
                self.lcd.set_rotation(lcd.1);
            }
        }

        Ok(true)
    }

    async fn close(&self) {}

    async fn serialize(&self) -> WireDevice {
        let status = self.protocol.status_cache.lock().await.clone();
        let host = self.chain_host.get();
        let children = match host {
            Some(h) => h.children().await,
            None => Vec::new(),
        };
        let mut child_wire = Vec::with_capacity(children.len());
        for child in &children {
            child_wire.push(child.serialize().await);
        }

        let mut rgb_status = self.serialize_rgb();
        if let Some(h) = host {
            rgb_status.chainable_channels = h.chainable_channels();
        }

        let mut capabilities = vec![
            DeviceCapability::Pump(PumpStatus {
                rpm: status.pump_rpm,
                duty: status.pump_duty,
                controllable: kraken_profile(self.pid).has_pump_control,
            }),
            DeviceCapability::Sensors(vec![Sensor {
                id: format!("{}_liquid_temp", self.id),
                name: "Liquid Temperature".to_string(),
                value: status.liquid_temp,
                unit: SensorUnit::Celsius,
                sensor_type: SensorType::Temperature,
                visibility: Default::default(),
            }]),
            DeviceCapability::Rgb(rgb_status),
        ];
        if let Some(lcd) = self.as_lcd() {
            capabilities.push(DeviceCapability::Lcd(lcd.lcd_status()));
        }
        if !child_wire.is_empty() {
            capabilities.push(DeviceCapability::Children(child_wire));
        }

        WireDeviceBuilder::from_device(self)
            .device_type(DeviceType::AIO)
            .capabilities(capabilities)
            .serial_number(self.serial_number.clone())
            .build()
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        let profile = kraken_profile(self.pid);
        let visible = self.active_state() == halod_protocol::types::VisibilityState::Visible;
        let mut caps = vec![
            CapabilityRef::Controller(self),
            CapabilityRef::Rgb(self),
            CapabilityRef::Sensor(self),
            CapabilityRef::Chain(self),
        ];
        if profile.has_pump_control && visible {
            caps.push(CapabilityRef::Fan(self));
        }
        if profile.has_lcd {
            caps.push(CapabilityRef::Lcd(self));
        }
        caps
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn as_chain(&self) -> Option<&dyn ChainCapability> {
        Some(self)
    }

    fn set_sensor_visibility(&self, sensor_id: &str, state: VisibilityState) {
        self.sensor_visibility
            .lock()
            .unwrap()
            .insert(sensor_id.to_string(), state);
    }

    fn debug_info_extra(&self) -> Vec<(String, String)> {
        let mut out = vec![
            ("vid".to_string(), format!("{:04x}", self.vid)),
            ("pid".to_string(), format!("{:04x}", self.pid)),
            ("model".to_string(), self.model_name.to_string()),
        ];
        if kraken_profile(self.pid).has_lcd {
            let (w, h) = Self::lcd_size_for(self.pid);
            out.push(("lcd_resolution".to_string(), format!("{w}x{h}")));
        }
        out
    }
}

#[async_trait]
impl crate::drivers::Controller for NZXTKraken {
    async fn discover_children(&self, app: Arc<AppState>) -> Vec<Arc<dyn Device>> {
        log::debug!("[NZXT Kraken] Discovering accessories...");
        let accessories = self
            .protocol
            .base
            .detect_accessories()
            .await
            .unwrap_or_default();
        let mut result = Vec::new();

        // The Kraken has one external fan header. The protocol reports both channels as
        // populated regardless of physical connection, and set_fan_duty ignores the channel.
        // Take the first recognizable accessory only.
        'outer: for accessory in accessories {
            log::debug!(
                "[NZXT Kraken] Detected accessory: channel {}, id 0x{:02X}",
                accessory.channel_id,
                accessory.accessory_id,
            );
            let handle = DiscoveryHandle::NzxtChain {
                channel_id: accessory.channel_id,
                accessory_id: accessory.accessory_id,
                chain_hub: self.arc_self_chain_hub(),
                fan_hub: self.arc_self_fan_hub(),
            };
            let Some(child) = crate::discovery::make_device(handle) else {
                log::warn!(
                    "[NZXT Kraken] Unrecognized accessory id 0x{:02X}",
                    accessory.accessory_id,
                );
                continue;
            };
            if crate::usecases::registration::register_device(&app, child.clone()).await {
                log::debug!(
                    "[NZXT Kraken] Initialized accessory channel {}",
                    accessory.channel_id
                );
                if let Some(host) = self.chain_host.get() {
                    // Kraken's chain is the single ext channel ("0").
                    host.register_auto_link(&EXT_CHAIN_CHANNEL.to_string(), child.clone())
                        .await;
                }
                result.push(child);
                break 'outer;
            }
        }

        self.protocol.resume_polling().await;
        result
    }
}

#[async_trait]
impl FanCapability for NZXTKraken {
    async fn get_duty(&self) -> Result<u8> {
        Ok(self.protocol.status_cache.lock().await.pump_duty)
    }

    async fn set_duty(&self, duty: u8) -> Result<()> {
        self.protocol.write_pump_duty(duty).await
    }

    async fn get_rpm(&self) -> Option<u32> {
        None
    }

    fn fan_state(&self) -> &FanStateSlot {
        &self.fan
    }
}

#[async_trait]
impl SensorCapability for NZXTKraken {
    async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        let temp = self.protocol.status_cache.lock().await.liquid_temp;
        let sensor_id = format!("{}_liquid_temp", self.id);
        let visibility = self
            .sensor_visibility
            .lock()
            .unwrap()
            .get(&sensor_id)
            .cloned()
            .unwrap_or_default();
        Ok(vec![Sensor {
            id: sensor_id,
            name: "Liquid Temperature".to_string(),
            value: temp,
            unit: SensorUnit::Celsius,
            sensor_type: SensorType::Temperature,
            visibility,
        }])
    }
}

#[async_trait]
impl RgbCapability for NZXTKraken {
    fn descriptor(&self) -> &RgbDescriptor {
        &self.rgb_descriptor
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
        match zone_id {
            "logo" => {
                let color = colors
                    .first()
                    .copied()
                    .unwrap_or(RgbColor { r: 0, g: 0, b: 0 });
                self.protocol.write_logo(color).await
            }
            _ => self.protocol.write_ring_frame(colors).await,
        }
    }
}

#[async_trait]
impl LcdCapability for NZXTKraken {
    fn lcd_descriptor(&self) -> LcdDescriptor {
        self.lcd_descriptor.clone()
    }

    fn lcd_state(&self) -> &LcdStateSlot {
        &self.lcd
    }

    async fn set_image(&self, data: &[u8]) -> Result<()> {
        let (width, height) = Self::lcd_size_for(self.pid);
        let is_gif = data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a");
        if is_gif {
            // Animated GIF: store it in a device bucket; the panel plays it natively.
            self.protocol
                .upload_gif(self.vid, self.pid, data, width, height)
                .await?;
        } else {
            // Static image: decode + resize, then stream a single Q565 frame over
            // the type-0x08 path — no bucket handshake, no flash to the default screen.
            let raw = data.to_vec();
            let rgba =
                tokio::task::spawn_blocking(move || decode_static_image_rgba(&raw, width, height))
                    .await??;
            self.stream_frame(&rgba, width, height).await?;
        }
        // mode and active_image filename are set by the usecase after saving the file.
        Ok(())
    }

    async fn stream_frame(&self, rgba: &[u8], width: u32, height: u32) -> Result<()> {
        // Q565 encoding is CPU-bound (~20 ms for a 640×640 frame); run it on a
        // blocking thread so it does not stall the async runtime (IPC, other engines).
        let rgba = rgba.to_vec();
        let payload = tokio::task::spawn_blocking(move || {
            NzxtKrakenProtocol::<HidTransport>::rgba_to_q565_payload(&rgba, width, height)
        })
        .await??;
        self.protocol
            .stream_frame(self.vid, self.pid, &payload)
            .await
    }

    async fn set_rotation(&self, degrees: u32) -> Result<()> {
        let brightness = self.lcd.brightness();
        self.protocol
            .write_screen_config(brightness, degrees)
            .await?;
        self.lcd.set_rotation(degrees);
        Ok(())
    }

    async fn set_brightness(&self, brightness: u8) -> Result<()> {
        let rotation = self.lcd.rotation();
        self.protocol
            .write_screen_config(brightness, rotation)
            .await?;
        self.lcd.set_brightness(brightness);
        Ok(())
    }

    async fn reset_to_default(&self) -> Result<()> {
        self.protocol.switch_to_default_display().await?;
        // Device cleared its bucket registry when switching to default mode.
        // Force a full_reset on the next upload so we don't try ping-pong on stale state.
        *self.protocol.active_bucket.lock().await = None;
        self.lcd.set_mode(LcdMode::Default);
        self.lcd.set_active_image(None);
        Ok(())
    }

    async fn set_active_image_filename(&self, filename: Option<String>) {
        if let Some(ref name) = filename {
            let mode = if name.ends_with(".gif") {
                LcdMode::Gif
            } else {
                LcdMode::Image
            };
            self.lcd.set_mode(mode);
        }
        self.lcd.set_active_image(filename);
    }

    fn save_state(&self) -> serde_json::Value {
        let active_image = LcdCapability::current_state(self).active_image;
        serde_json::json!({
            "template_id":  self.lcd_template_id(),
            "params":       self.lcd_template_params(),
            "active_image": active_image,
        })
    }

    async fn restore_state(&self, v: &serde_json::Value) {
        if let Some(id) = v.get("template_id") {
            self.set_lcd_template_id(serde_json::from_value(id.clone()).ok().flatten());
        }
        if let Some(p) = v.get("params") {
            if let Ok(params) = serde_json::from_value(p.clone()) {
                self.set_lcd_template_params(params);
            }
        }
        // Engine template is active — the LCD engine tick will push frames.
        if self.lcd_template_id().is_some() {
            return;
        }
        let active_image = v
            .get("active_image")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        if let Some(filename) = active_image {
            let path = crate::config::lcd_images_dir().join(&filename);
            match std::fs::read(&path) {
                Ok(data) => {
                    if let Err(e) = self.set_image(&data).await {
                        log::warn!(
                            "[Kraken LCD] profile restore: set_image {filename} failed: {e}"
                        );
                    } else {
                        self.set_active_image_filename(Some(filename)).await;
                    }
                }
                Err(e) => {
                    log::warn!("[Kraken LCD] profile restore: image {filename} missing ({e}), resetting to default");
                    let _ = self.reset_to_default().await;
                }
            }
        } else {
            let _ = self.reset_to_default().await;
        }
    }
}

#[async_trait]
impl ChainAdapter for NZXTKraken {
    fn parent_id(&self) -> String {
        self.id.clone()
    }

    fn channels(&self) -> Vec<ChannelDescriptor> {
        vec![ChannelDescriptor {
            channel_id: EXT_CHAIN_CHANNEL.to_string(),
            display_name: "External Accessory".to_string(),
            max_leds: MAX_NZXT_KRAKEN_CHAIN_LEDS,
        }]
    }

    /// `channel_id` is ignored — Kraken has a single external header (the
    /// wire path takes no channel byte).
    async fn write_composed_frame(&self, _channel_id: &str, composed: &[RgbColor]) -> Result<()> {
        self.protocol.write_ext_frame(composed).await
    }
}

#[async_trait]
impl NzxtFanHub for NZXTKraken {
    fn id(&self) -> String {
        self.id.clone()
    }

    async fn get_fan_rpm(&self, _channel: &u8) -> Result<u32> {
        Ok(self.protocol.status_cache.lock().await.fan_rpm)
    }

    async fn get_fan_duty(&self, _channel: &u8) -> Result<u8> {
        Ok(self.protocol.status_cache.lock().await.fan_duty)
    }

    async fn get_fan_controllable(&self, _channel: &u8) -> Result<bool> {
        Ok(self.protocol.status_cache.lock().await.fan_rpm > 0)
    }

    async fn set_fan_duty(&self, _channel: &u8, duty: u8) -> Result<()> {
        if !kraken_profile(self.pid).has_fan_channel {
            return Ok(());
        }
        self.protocol.write_fan_duty(duty).await
    }
}

impl ChainCapability for NZXTKraken {
    fn chain_host(&self) -> Option<&Arc<ChainHost>> {
        self.chain_host.get()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // TODO: should abort instead
    #[test]
    fn profile_for_unknown_pid_falls_back_to_z63() {
        let p = kraken_profile(0x0000);
        assert_eq!(p.model_name, "Kraken Z53/63/73");
    }

    // ── descriptor ───────────────────────────────────────────────────────

    #[test]
    fn x3_descriptor_has_8_led_ring_and_logo_zone() {
        let desc = NZXTKraken::build_rgb_descriptor(0x2007);
        assert_eq!(desc.zones.len(), 2);
        assert_eq!(desc.zones[0].id, "ring");
        assert_eq!(desc.zones[0].leds.len(), 8);
        assert_eq!(desc.zones[1].id, "logo");
        assert_eq!(desc.zones[1].leds.len(), 1);
    }

    // ── LCD descriptor ────────────────────────────────────────────────────

    #[test]
    fn lcd_size_for_unknown_pid_falls_back_to_z63() {
        assert_eq!(NZXTKraken::lcd_size_for(0xFFFF), (320, 320));
    }

    #[test]
    fn build_lcd_descriptor_circle_shape_and_four_rotations() {
        let desc = NZXTKraken::build_lcd_descriptor(0x3008);
        assert_eq!(desc.shape, ScreenShape::Circle);
        assert_eq!(desc.supported_rotations, vec![0, 90, 180, 270]);
        assert_eq!(desc.width, 320);
        assert_eq!(desc.height, 320);
    }
}
