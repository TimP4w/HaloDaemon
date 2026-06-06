//! LED position layout for Logitech RGB zones — mouse linear strips and
//! keyboard per-key grids.

use crate::drivers::vendors::generic::devices::common::tkl_key_positions;
use crate::drivers::vendors::logitech::devices::profile::LogitechZoneInfo;
use halod_protocol::keyboard::KeyLayoutSpec;
use halod_protocol::types::{LedPosition, ZoneTopology};

pub(super) fn mouse_led_positions(count: u8) -> Vec<LedPosition> {
    let max = (count as f32 - 1.0).max(1.0);
    (0..count as u32)
        .map(|i| LedPosition { id: i, x: i as f32 / max, y: 0.5 })
        .collect()
}

pub(super) fn leds_for_zone_info(
    zi: &LogitechZoneInfo,
    key_layout: Option<&'static KeyLayoutSpec>,
) -> Vec<LedPosition> {
    match &zi.topology {
        ZoneTopology::Keyboard { .. } => {
            key_layout.map(tkl_key_positions).unwrap_or_default()
        }
        _ => mouse_led_positions(zi.led_count),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_led_positions_spaces_leds_evenly_across_zero_to_one() {
        let leds = mouse_led_positions(4);
        let xs: Vec<f32> = leds.iter().map(|l| l.x).collect();
        assert_eq!(xs, vec![0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0]);
        assert!(leds.iter().all(|l| l.y == 0.5));
    }

    #[test]
    fn mouse_led_positions_single_led_has_no_divide_by_zero() {
        // count - 1 == 0; the `.max(1.0)` divisor guard must keep x finite.
        let leds = mouse_led_positions(1);
        assert_eq!(leds.len(), 1);
        assert_eq!(leds[0].x, 0.0);
    }
}
