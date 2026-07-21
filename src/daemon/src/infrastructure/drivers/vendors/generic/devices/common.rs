// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared helpers for the core device implementations.

#[cfg(test)]
use std::collections::HashMap;

pub use crate::domain::device::lighting_segment::{ring_led_positions, transformed_zone_frame};
#[cfg(test)]
use halod_shared::types::RgbColor;
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
        visibility: Default::default(),
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
