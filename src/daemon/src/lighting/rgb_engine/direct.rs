// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;

use halod_shared::effect_designer::{self, DesignerParams, DESIGNER_EFFECT_ID};
use halod_shared::types::{Animation, EffectParamValue};

use super::color::{srgb_to_linear, LinearColor};

// Direct effects compute `led_color` from the shared clock `t` — no pixmap needed.
pub trait DirectLedEffect: Send {
    fn tick(&mut self, _t: f32, _dt: f32) {}
    /// `p` is fractional chain position; `p_ring` is ring-local position
    /// (equal to `p` for single-ring zones). `nx`/`ny` feed the twinkle hash.
    fn led_color(&self, p: f32, p_ring: f32, nx: f32, ny: f32, t: f32) -> LinearColor;

    /// Sensor id this effect wants fed via `set_sensor_value` before each
    /// `tick()`, or `None` if it doesn't consume a sensor.
    fn sensor_id(&self) -> Option<&str> {
        None
    }
    /// Called once per engine tick, before `tick()`, with the latest reading
    /// for `sensor_id()` (`None` when the sensor is unset or unavailable).
    fn set_sensor_value(&mut self, _value: Option<f64>) {}
}

struct Designer {
    params: DesignerParams,
}

impl Designer {
    fn descriptor() -> Animation {
        Animation {
            id: DESIGNER_EFFECT_ID.to_string(),
            name: "Designer".to_string(),
            params: effect_designer::param_descriptors(),
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Box<dyn DirectLedEffect> {
        Box::new(Self {
            params: DesignerParams::from_params(params),
        })
    }
}

impl DirectLedEffect for Designer {
    fn led_color(&self, p: f32, p_ring: f32, nx: f32, ny: f32, t: f32) -> LinearColor {
        let pos = match self.params.ring_scope {
            effect_designer::RingScope::Zone => p,
            effect_designer::RingScope::PerRing => p_ring,
        };
        let (r, g, b) = self.params.color(pos, nx, ny, t);
        LinearColor {
            r: srgb_to_linear((r * 255.0).round() as u8),
            g: srgb_to_linear((g * 255.0).round() as u8),
            b: srgb_to_linear((b * 255.0).round() as u8),
        }
    }
}

pub fn build_direct(
    id: &str,
    params: &HashMap<String, EffectParamValue>,
) -> Option<Box<dyn DirectLedEffect>> {
    match id {
        DESIGNER_EFFECT_ID => Some(Designer::from_params(params)),
        _ => None,
    }
}

pub fn direct_descriptors() -> Vec<Animation> {
    vec![Designer::descriptor()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_dispatches_known_ids_only() {
        assert!(build_direct(DESIGNER_EFFECT_ID, &HashMap::new()).is_some());
        assert!(build_direct("breathing", &HashMap::new()).is_none());
        assert!(build_direct("nope", &HashMap::new()).is_none());
    }

    #[test]
    fn descriptors_list_designer_only() {
        let ids: Vec<String> = direct_descriptors().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec![DESIGNER_EFFECT_ID.to_string()]);
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn designer_output_stays_in_unit_gamut(
            nx in 0.0f32..1.0,
            ny in 0.0f32..1.0,
            t in 0.0f32..1000.0,
        ) {
            let fx = build_direct(DESIGNER_EFFECT_ID, &HashMap::new()).unwrap();
            let c = fx.led_color(nx, nx, nx, ny, t);
            for ch in [c.r, c.g, c.b] {
                prop_assert!((0.0..=1.0).contains(&ch), "channel {ch} out of gamut");
            }
        }
    }
}
