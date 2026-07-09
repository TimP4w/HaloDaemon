// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared helpers for device drivers: stable-ID construction, serial normalisation, the `WireDevice` builder, per-LED frame assembly, and keyboard layout utilities.

use std::collections::HashMap;

use halod_shared::keyboard::KeyLayoutSpec;
use std::f32::consts::PI;

use halod_shared::types::{
    CategoryLayout, ConnectionType, DeviceCapability, DeviceType, EffectParamValue, KeyboardLayout,
    LedPosition, RgbColor, RgbZone, WireDevice, ZoneTopology,
};

use crate::drivers::Device;

/// Build a stable device ID of the form `<prefix>_<serial>`, falling back to `<prefix>_<index>` when no usable serial is available.
pub fn build_device_id(prefix: &str, serial: Option<&str>, index: usize) -> String {
    match serial.filter(|s| !s.is_empty()) {
        Some(s) => format!("{prefix}_{s}"),
        None => format!("{prefix}_{index}"),
    }
}

/// Normalise a hardware serial for the `WireDevice::serial_number` field:
/// an empty string is treated as "no serial" and becomes `None`.
pub fn stable_serial(serial: Option<&str>) -> Option<String> {
    serial.filter(|s| !s.is_empty()).map(str::to_string)
}

fn key_positions(spec: &KeyLayoutSpec<'_>, col_max: f32) -> Vec<LedPosition> {
    spec.resolve()
        .iter()
        .filter_map(|cell| {
            let id = spec
                .cid_map
                .iter()
                .find(|(_, kid)| *kid == cell.id)
                .map(|(driver_id, _)| *driver_id)?;
            Some(LedPosition {
                id,
                x: cell.col / col_max,
                y: (cell.row + 1.5) / 9.0,
            })
        })
        .collect()
}

/// Generate `LedPosition` entries for a TKL keyboard zone from a `KeyLayoutSpec`.
///
/// Grid coordinates are projected into `[0, 1]` space using the standard TKL
/// bounds (`col ∈ [0, 17.5]`, `row ∈ [-1.5, 7.5]`).
pub fn tkl_key_positions(spec: &KeyLayoutSpec<'_>) -> Vec<LedPosition> {
    key_positions(spec, 17.5)
}

/// Generate `LedPosition` entries for a full-size (100%) keyboard zone.
///
/// Same as [`tkl_key_positions`] but normalized to the wider full-size grid
/// (`col ∈ [0, 22.5]`) to accommodate the numpad columns.
pub fn full_size_key_positions(spec: &KeyLayoutSpec<'_>) -> Vec<LedPosition> {
    key_positions(spec, 22.5)
}

/// Swap the `layout` field on a `Keyboard` topology variant, leaving other topology variants unchanged.
pub fn override_keyboard_layout(topology: ZoneTopology, layout: &KeyboardLayout) -> ZoneTopology {
    match topology {
        ZoneTopology::Keyboard { form_factor, .. } => ZoneTopology::Keyboard {
            form_factor,
            layout: layout.clone(),
        },
        other => other,
    }
}

/// Assemble a fixed-length per-LED colour frame from one zone's
/// `index -> colour` map. LED indices are decimal strings; any index missing
/// from the map (or `>= count`) is left black. This turns the sparse
/// `RgbState::PerLed` representation into the contiguous frame hardware wants.
pub fn per_led_frame(led_map: &HashMap<String, RgbColor>, count: usize) -> Vec<RgbColor> {
    (0..count)
        .map(|i| {
            led_map
                .get(&i.to_string())
                .copied()
                .unwrap_or(RgbColor { r: 0, g: 0, b: 0 })
        })
        .collect()
}

/// Extract a color value from a native-effect param map.
pub fn effect_color(params: &HashMap<String, EffectParamValue>, key: &str) -> Option<RgbColor> {
    if let Some(EffectParamValue::Color(c)) = params.get(key) {
        Some(*c)
    } else {
        None
    }
}

/// Extract a string value from a native-effect param map.
pub fn effect_str<'a>(params: &'a HashMap<String, EffectParamValue>, key: &str) -> Option<&'a str> {
    if let Some(EffectParamValue::Str(s)) = params.get(key) {
        Some(s.as_str())
    } else {
        None
    }
}

/// Compute LED positions for a ring or multi-ring topology; other topologies return an empty vec.
pub fn ring_led_positions(topology: &ZoneTopology, count: u32) -> Vec<LedPosition> {
    match topology {
        ZoneTopology::Ring => (0..count)
            .map(|i| {
                let angle = 2.0 * PI * i as f32 / count as f32 - PI / 2.0;
                LedPosition {
                    id: i,
                    x: 0.5 + 0.42 * angle.cos(),
                    y: 0.5 + 0.42 * angle.sin(),
                }
            })
            .collect(),
        ZoneTopology::Rings { count: rings } => {
            let rings = (*rings).max(1) as u32;
            let per_ring = (count / rings).max(1);
            let ring_r_x = 0.42 / rings as f32;
            (0..count)
                .map(|i| {
                    let ring_idx = (i / per_ring).min(rings - 1);
                    let in_ring = i % per_ring;
                    let cx = (ring_idx as f32 + 0.5) / rings as f32;
                    let angle = 2.0 * PI * in_ring as f32 / per_ring as f32 - PI / 2.0;
                    LedPosition {
                        id: i,
                        x: cx + ring_r_x * angle.cos(),
                        y: 0.5 + 0.42 * angle.sin(),
                    }
                })
                .collect()
        }
        _ => vec![],
    }
}

/// Build a linear RGB zone with `led_count` evenly-spaced LEDs.
pub fn linear_rgb_zone(id: &str, name: &str, led_count: usize) -> RgbZone {
    let leds = (0..led_count)
        .map(|i| LedPosition {
            id: i as u32,
            x: if led_count > 1 {
                i as f32 / (led_count - 1) as f32
            } else {
                0.5
            },
            y: 0.5,
        })
        .collect();
    RgbZone {
        id: id.to_string(),
        name: name.to_string(),
        topology: ZoneTopology::Linear,
        leds,
    }
}

/// Builder for [`WireDevice`], seeded from a [`Device`]'s identity getters; `device_type` defaults to [`DeviceType::Other`] and `connected` to `true`.
pub struct WireDeviceBuilder {
    id: String,
    name: String,
    vendor: String,
    model: String,
    device_type: DeviceType,
    connected: bool,
    capabilities: Vec<DeviceCapability>,
    connection_type: Option<ConnectionType>,
    serial_number: Option<String>,
    control_layout: Vec<CategoryLayout>,
}

impl WireDeviceBuilder {
    /// Seed a builder from the device's `id/name/vendor/model` getters.
    pub fn from_device(device: &dyn Device) -> Self {
        Self::from_parts(
            device.id().to_owned(),
            device.name().to_string(),
            device.vendor().to_string(),
            device.model().to_string(),
        )
    }

    /// Seed a builder from raw string parts — used by the Device trait default
    /// serialize impl where coercing `&Self` to `&dyn Device` is not possible.
    pub fn from_parts(id: String, name: String, vendor: String, model: String) -> Self {
        Self {
            id,
            name,
            vendor,
            model,
            device_type: DeviceType::Other,
            connected: true,
            capabilities: Vec::new(),
            connection_type: None,
            serial_number: None,
            control_layout: Vec::new(),
        }
    }

    /// Override the name seeded from `Device::name()` — for devices whose wire
    /// name is a runtime value (e.g. the model reported by the hardware) rather
    /// than the static name the trait getter returns.
    pub fn name(mut self, name: String) -> Self {
        self.name = name;
        self
    }

    pub fn device_type(mut self, device_type: DeviceType) -> Self {
        self.device_type = device_type;
        self
    }

    pub fn connected(mut self, connected: bool) -> Self {
        self.connected = connected;
        self
    }

    pub fn capabilities(mut self, capabilities: Vec<DeviceCapability>) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn connection_type(mut self, connection_type: Option<ConnectionType>) -> Self {
        self.connection_type = connection_type;
        self
    }

    pub fn serial_number(mut self, serial_number: Option<String>) -> Self {
        self.serial_number = serial_number;
        self
    }

    /// Declare a responsive grid layout for the generic Controls tab's
    /// category cards. Omit (or leave empty) for the default stacked,
    /// alphabetical full-width layout.
    pub fn control_layout(mut self, control_layout: Vec<CategoryLayout>) -> Self {
        self.control_layout = control_layout;
        self
    }

    pub fn build(self) -> WireDevice {
        WireDevice {
            id: self.id,
            name: self.name,
            vendor: self.vendor,
            model: self.model,
            device_type: self.device_type,
            connected: self.connected,
            capabilities: self.capabilities,
            connection_type: self.connection_type,
            serial_number: self.serial_number,
            active_state: Default::default(),
            // The serializer overlays the real transport for HID devices.
            transport: None,
            // The serializer overlays live write-rate stats when the device
            // reports them.
            write_rate: Default::default(),
            control_layout: self.control_layout,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_device_id_uses_serial_when_present() {
        assert_eq!(
            build_device_id("nzxt_hub", Some("ABC123"), 0),
            "nzxt_hub_ABC123"
        );
    }

    #[test]
    fn build_device_id_falls_back_to_index_when_serial_empty() {
        assert_eq!(build_device_id("nzxt_hub", Some(""), 2), "nzxt_hub_2");
    }

    #[test]
    fn build_device_id_falls_back_to_index_when_serial_none() {
        assert_eq!(build_device_id("nzxt_hub", None, 1), "nzxt_hub_1");
    }

    #[test]
    fn stable_serial_normalises_empty_and_missing() {
        assert_eq!(stable_serial(Some("SN42")), Some("SN42".to_string()));
        assert_eq!(stable_serial(Some("")), None);
        assert_eq!(stable_serial(None), None);
    }

    #[test]
    fn per_led_frame_fills_missing_indices_with_black() {
        let mut map = HashMap::new();
        map.insert(
            "0".to_string(),
            RgbColor {
                r: 10,
                g: 20,
                b: 30,
            },
        );
        map.insert("2".to_string(), RgbColor { r: 1, g: 2, b: 3 });
        // Index 5 is out of range and must be ignored.
        map.insert("5".to_string(), RgbColor { r: 9, g: 9, b: 9 });

        let frame = per_led_frame(&map, 3);
        assert_eq!(frame.len(), 3);
        assert_eq!(
            frame[0],
            RgbColor {
                r: 10,
                g: 20,
                b: 30
            }
        );
        assert_eq!(frame[1], RgbColor { r: 0, g: 0, b: 0 });
        assert_eq!(frame[2], RgbColor { r: 1, g: 2, b: 3 });
    }

    #[test]
    fn effect_color_extracts_color_variant() {
        let mut params = HashMap::new();
        params.insert(
            "color".to_string(),
            EffectParamValue::Color(RgbColor { r: 1, g: 2, b: 3 }),
        );
        assert_eq!(
            effect_color(&params, "color"),
            Some(RgbColor { r: 1, g: 2, b: 3 })
        );
        assert_eq!(effect_color(&params, "missing"), None);
    }

    #[test]
    fn effect_color_returns_none_for_wrong_variant() {
        let mut params = HashMap::new();
        params.insert(
            "color".to_string(),
            EffectParamValue::Str("red".to_string()),
        );
        assert_eq!(effect_color(&params, "color"), None);
    }

    #[test]
    fn effect_str_extracts_str_variant() {
        let mut params = HashMap::new();
        params.insert(
            "speed".to_string(),
            EffectParamValue::Str("fast".to_string()),
        );
        assert_eq!(effect_str(&params, "speed"), Some("fast"));
        assert_eq!(effect_str(&params, "missing"), None);
    }

    #[test]
    fn effect_str_returns_none_for_wrong_variant() {
        let mut params = HashMap::new();
        params.insert(
            "speed".to_string(),
            EffectParamValue::Color(RgbColor { r: 0, g: 0, b: 0 }),
        );
        assert_eq!(effect_str(&params, "speed"), None);
    }

    #[test]
    fn linear_rgb_zone_single_led_centered() {
        let zone = linear_rgb_zone("leds", "LEDs", 1);
        assert_eq!(zone.id, "leds");
        assert_eq!(zone.name, "LEDs");
        assert!(matches!(zone.topology, ZoneTopology::Linear));
        assert_eq!(zone.leds.len(), 1);
        assert!((zone.leds[0].x - 0.5).abs() < f32::EPSILON);
        assert!((zone.leds[0].y - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn linear_rgb_zone_multiple_leds_evenly_spaced() {
        let zone = linear_rgb_zone("leds", "LEDs", 5);
        assert_eq!(zone.leds.len(), 5);
        assert!((zone.leds[0].x - 0.0).abs() < f32::EPSILON);
        assert!((zone.leds[4].x - 1.0).abs() < f32::EPSILON);
        for led in &zone.leds {
            assert!((led.y - 0.5).abs() < f32::EPSILON);
        }
    }
}

/// RAII wrapper around a spawned Tokio task. Aborts the task automatically on
/// drop so device `close()` implementations never need explicit `abort()` calls.
pub struct TaskHandle(tokio::task::JoinHandle<()>);

impl TaskHandle {
    pub fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self(handle)
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        self.0.abort();
    }
}
