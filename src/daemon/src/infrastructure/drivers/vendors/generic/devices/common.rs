// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared helpers for the core device implementations.

#[cfg(test)]
use std::collections::HashMap;

pub use crate::domain::device::lighting_segment::{ring_led_positions, transformed_zone_frame};
#[cfg(test)]
use halod_shared::types::{
    CategoryLayout, ConnectionType, DeviceCapability, DeviceType, RgbColor, WireDevice,
};
use halod_shared::types::{LedPosition, LightingChannel, LightingDivision, ZoneTopology};

/// Assemble a fixed-length per-LED colour frame from one zone's
/// `index -> colour` map. LED indices are decimal strings; any index missing
/// from the map (or `>= count`) is left black. This turns the sparse
/// `LightingState::PerLed` representation into the contiguous frame hardware wants.
#[cfg(test)]
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

/// Build a linear RGB zone with `led_count` evenly-spaced LEDs.
pub fn linear_lighting_channel(id: &str, name: &str, led_count: usize) -> LightingChannel {
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
    LightingChannel {
        id: id.to_string(),
        name: name.to_string(),
        topology: ZoneTopology::Linear,
        leds,
        color_order: Default::default(),
        division: LightingDivision::Indivisible,
    }
}

/// Builder for [`WireDevice`], seeded from a [`Device`]'s identity getters; `device_type` defaults to [`DeviceType::Other`] and `connected` to `true`.
#[cfg(test)]
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
    integration_id: Option<String>,
    control_layout: Vec<CategoryLayout>,
}

#[cfg(test)]
impl WireDeviceBuilder {
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
            integration_id: None,
            control_layout: Vec::new(),
        }
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

    /// Mark this device as the root of an integration (owned by `plugin_id`)
    /// rather than a real device. `None` (the default) for everything else,
    /// including the devices an integration exposes as children.
    pub fn integration_id(mut self, integration_id: Option<String>) -> Self {
        self.integration_id = integration_id;
        self
    }

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
            integration_id: self.integration_id,
            conflict: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn linear_rgb_zone_single_led_centered() {
        let zone = linear_lighting_channel("leds", "LEDs", 1);
        assert_eq!(zone.id, "leds");
        assert_eq!(zone.name, "LEDs");
        assert!(matches!(zone.topology, ZoneTopology::Linear));
        assert_eq!(zone.leds.len(), 1);
        assert!((zone.leds[0].x - 0.5).abs() < f32::EPSILON);
        assert!((zone.leds[0].y - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn linear_rgb_zone_multiple_leds_evenly_spaced() {
        let zone = linear_lighting_channel("leds", "LEDs", 5);
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
