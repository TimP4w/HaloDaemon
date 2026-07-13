// SPDX-License-Identifier: GPL-3.0-or-later
// Shared by the pixmap (canvas) and direct effect paths so a direct and a
// pixmap rendering of equal params match on the wire.
#[derive(Debug, Clone, Copy)]
pub struct LinearColor {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

pub fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub fn linear_to_srgb(c: f32) -> u8 {
    let c = if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (c.clamp(0.0, 1.0) * 255.0).round() as u8
}

pub fn linear_to_led(c: f32, gamma: f32) -> u8 {
    (c.clamp(0.0, 1.0).powf(1.0 / gamma) * 255.0).round() as u8
}

#[cfg(test)]
fn hue_to_srgb(hue: f32) -> (u8, u8, u8) {
    let h = (hue.rem_euclid(1.0)) * 6.0;
    let sector = h as i32;
    let f = h - sector as f32;
    let (r, g, b) = match sector {
        0 => (1.0, f, 0.0),
        1 => (1.0 - f, 1.0, 0.0),
        2 => (0.0, 1.0, f),
        3 => (0.0, 1.0 - f, 1.0),
        4 => (f, 0.0, 1.0),
        _ => (1.0, 0.0, 1.0 - f),
    };
    (
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_to_led_applies_gamma_exactly() {
        assert_eq!(linear_to_led(0.25, 2.0), 128);
        assert_eq!(linear_to_led(1.0, 1.0), 255);
        assert_eq!(linear_to_led(0.0, 2.0), 0);
    }

    #[test]
    fn hue_to_srgb_anchors_at_primary_colors() {
        assert_eq!(hue_to_srgb(0.0), (255, 0, 0), "hue 0 → red");
        assert_eq!(hue_to_srgb(1.0 / 3.0), (0, 255, 0), "hue 1/3 → green");
        assert_eq!(hue_to_srgb(2.0 / 3.0), (0, 0, 255), "hue 2/3 → blue");
        assert_eq!(hue_to_srgb(1.0), (255, 0, 0), "hue 1.0 wraps to red");
        assert_eq!(hue_to_srgb(-1.0), (255, 0, 0), "hue -1.0 wraps to red");
    }

    #[test]
    fn hue_to_srgb_covers_mid_sector_ramps() {
        assert_eq!(hue_to_srgb(0.25), (128, 255, 0), "sector 1 ramp");
        assert_eq!(hue_to_srgb(3.5 / 6.0), (0, 128, 255), "sector 3 ramp");
        assert_eq!(
            hue_to_srgb(5.5 / 6.0),
            (255, 0, 128),
            "sector 5 ramp (fallthrough arm)"
        );
    }
}

#[cfg(test)]
mod prop_tests {
    use super::{linear_to_led, linear_to_srgb, srgb_to_linear};
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn srgb_linear_round_trips(c in any::<u8>()) {
            let back = linear_to_srgb(srgb_to_linear(c));
            prop_assert!((back as i16 - c as i16).abs() <= 1, "{c} -> {back}");
        }

        #[test]
        fn srgb_to_linear_is_monotonic(a in any::<u8>(), b in any::<u8>()) {
            let (lo, hi) = (a.min(b), a.max(b));
            prop_assert!(srgb_to_linear(lo) <= srgb_to_linear(hi));
        }

        #[test]
        fn linear_to_srgb_is_monotonic(a in 0.0f32..1.0, b in 0.0f32..1.0) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            prop_assert!(linear_to_srgb(lo) <= linear_to_srgb(hi));
        }

        #[test]
        fn linear_to_led_clamps_and_is_monotonic(
            a in -1.0f32..2.0,
            b in -1.0f32..2.0,
            gamma in 0.5f32..4.0,
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            prop_assert!(linear_to_led(lo, gamma) <= linear_to_led(hi, gamma));
            prop_assert_eq!(linear_to_led(2.0, gamma), 255);
            prop_assert_eq!(linear_to_led(-1.0, gamma), 0);
        }
    }
}
