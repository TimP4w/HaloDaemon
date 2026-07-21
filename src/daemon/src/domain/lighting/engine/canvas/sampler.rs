// SPDX-License-Identifier: GPL-3.0-or-later
use halod_shared::types::{LightingChannel, RgbColor, SamplingMode, ZoneTopology};
use tiny_skia::Pixmap;

use super::super::color::{linear_to_led, linear_to_srgb};
use crate::config::PlacedZone;

const LED_GAMMA: f32 = 2.2;

struct LinearRgb {
    r: f32,
    g: f32,
    b: f32,
}

pub struct Sampler {
    radius: f32,
}

impl Sampler {
    pub fn new(radius: f32) -> Self {
        Self { radius }
    }

    fn sample_box(&self, pixmap: &Pixmap, cx: f32, cy: f32) -> LinearRgb {
        let r = self.radius;
        let x0 = (cx - r).max(0.0) as u32;
        let y0 = (cy - r).max(0.0) as u32;
        let x1 = ((cx + r) as u32 + 1).min(pixmap.width());
        let y1 = ((cy + r) as u32 + 1).min(pixmap.height());

        let (mut red, mut grn, mut blu, mut n) = (0.0f32, 0.0, 0.0, 0u32);
        for y in y0..y1 {
            for x in x0..x1 {
                if let Some(p) = pixmap.pixel(x, y) {
                    red += p.red() as f32;
                    grn += p.green() as f32;
                    blu += p.blue() as f32;
                    n += 1;
                }
            }
        }
        let n = n.max(1) as f32;
        LinearRgb {
            r: red / n,
            g: grn / n,
            b: blu / n,
        }
    }

    fn led_canvas_pos(
        &self,
        lx: f32,
        ly: f32,
        placed: &PlacedZone,
        pw: f32,
        ph: f32,
    ) -> (f32, f32) {
        let (nx, ny) = halod_shared::zone_transform::led_canvas_norm(
            lx,
            ly,
            placed.x,
            placed.y,
            placed.w,
            placed.h,
            placed.rotation,
        );
        (nx * pw, ny * ph)
    }

    pub fn sample_zone(
        &self,
        pixmap: &Pixmap,
        placed: &PlacedZone,
        zone: &LightingChannel,
    ) -> Vec<RgbColor> {
        let pw = pixmap.width() as f32;
        let ph = pixmap.height() as f32;
        let unrolled = placed.sampling_mode == SamplingMode::Unrolled
            && matches!(
                zone.topology,
                ZoneTopology::Ring | ZoneTopology::Rings { .. }
            );
        zone.leds
            .iter()
            .enumerate()
            .map(|(i, led)| {
                let (lx, ly) = if unrolled {
                    halod_shared::zone_transform::unrolled_led_pos(i, zone.leds.len())
                } else {
                    (led.x, led.y)
                };
                let (cx, cy) = self.led_canvas_pos(lx, ly, placed, pw, ph);
                let lin = self.sample_box(pixmap, cx, cy);
                RgbColor {
                    r: linear_to_led(lin.r / 255.0, LED_GAMMA),
                    g: linear_to_led(lin.g / 255.0, LED_GAMMA),
                    b: linear_to_led(lin.b / 255.0, LED_GAMMA),
                }
            })
            .collect()
    }

    pub fn pixmap_to_srgb_rgba(&self, pixmap: &Pixmap, out: &mut Vec<u8>) {
        out.clear();
        let cap = (pixmap.width() * pixmap.height() * 4) as usize;
        if out.capacity() < cap {
            out.reserve(cap - out.len());
        }
        for y in 0..pixmap.height() {
            for x in 0..pixmap.width() {
                if let Some(p) = pixmap.pixel(x, y) {
                    out.push(linear_to_srgb(p.red() as f32 / 255.0));
                    out.push(linear_to_srgb(p.green() as f32 / 255.0));
                    out.push(linear_to_srgb(p.blue() as f32 / 255.0));
                    out.push(p.alpha());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::{LedPosition, ZoneTopology};
    use tiny_skia::Color;

    #[test]
    fn sample_zone_solid_color_returns_uniform_output() {
        let mut pixmap = Pixmap::new(64, 64).unwrap();
        pixmap.fill(Color::from_rgba8(128, 0, 64, 255));

        let placed = PlacedZone {
            device_id: "test".to_string(),
            channel_id: "ring".to_string(),
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            rotation: 0.0,
            effect: None,
            sampling_mode: Default::default(),
        };

        let zone = LightingChannel {
            id: "ring".to_string(),
            name: "Ring".to_string(),
            topology: ZoneTopology::Ring,
            leds: vec![
                LedPosition {
                    id: 0,
                    x: 0.25,
                    y: 0.5,
                },
                LedPosition {
                    id: 1,
                    x: 0.75,
                    y: 0.5,
                },
            ],
            color_order: Default::default(),
            division: Default::default(),
            visibility: Default::default(),
        };

        let sampler = Sampler::new(3.0);
        let colors = sampler.sample_zone(&pixmap, &placed, &zone);
        assert_eq!(colors.len(), 2);
        assert_eq!(colors[0], colors[1]);
        assert!(colors[0].r > 0);
    }

    #[test]
    fn pixmap_to_srgb_rgba_correct_length() {
        let pixmap = Pixmap::new(4, 4).unwrap();
        let sampler = Sampler::new(3.0);
        let mut rgba = Vec::new();
        sampler.pixmap_to_srgb_rgba(&pixmap, &mut rgba);
        assert_eq!(rgba.len(), (4 * 4 * 4) as usize);
    }

    #[test]
    fn pixmap_to_srgb_rgba_applies_conversion_and_preserves_alpha() {
        use tiny_skia::PremultipliedColorU8;
        // Build a 1×1 pixmap with R=128, G=0, B=0, A=255.
        // linear_to_srgb(128/255) ≈ 188; G and B stay 0; alpha passes through.
        let mut pixmap = Pixmap::new(1, 1).unwrap();
        pixmap.pixels_mut()[0] = PremultipliedColorU8::from_rgba(128, 0, 0, 255).unwrap();
        let sampler = Sampler::new(3.0);
        let mut rgba = Vec::new();
        sampler.pixmap_to_srgb_rgba(&pixmap, &mut rgba);
        assert_eq!(rgba.len(), 4);
        // sRGB-encoded red must differ from the raw linear value (128).
        assert_ne!(rgba[0], 128, "sRGB conversion must change the raw value");
        // Green and blue are zero — conversion of 0 is 0.
        assert_eq!(rgba[1], 0);
        assert_eq!(rgba[2], 0);
        // Alpha is passed through verbatim.
        assert_eq!(rgba[3], 255);
    }

    fn red_blue_pixmap(w: u32, h: u32) -> Pixmap {
        use tiny_skia::PremultipliedColorU8;
        let mut pixmap = Pixmap::new(w, h).unwrap();
        let pixels = pixmap.pixels_mut();
        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) as usize;
                pixels[idx] = if x < w / 2 {
                    PremultipliedColorU8::from_rgba(255, 0, 0, 255).unwrap()
                } else {
                    PremultipliedColorU8::from_rgba(0, 0, 255, 255).unwrap()
                };
            }
        }
        pixmap
    }

    fn single_led_zone(lx: f32, ly: f32) -> LightingChannel {
        LightingChannel {
            id: "z".to_string(),
            name: "Z".to_string(),
            topology: ZoneTopology::Ring,
            leds: vec![LedPosition {
                id: 0,
                x: lx,
                y: ly,
            }],
            color_order: Default::default(),
            division: Default::default(),
            visibility: Default::default(),
        }
    }

    fn full_zone() -> PlacedZone {
        PlacedZone {
            device_id: "d".into(),
            channel_id: "z".into(),
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            rotation: 0.0,
            effect: None,
            sampling_mode: Default::default(),
        }
    }

    #[test]
    fn rotation_zero_samples_correct_half() {
        let pixmap = red_blue_pixmap(100, 100);
        let sampler = Sampler::new(1.0);
        let placed = full_zone();

        let red = sampler.sample_zone(&pixmap, &placed, &single_led_zone(0.25, 0.5));
        assert!(red[0].r > red[0].b, "x=0.25 should sample red half");

        let blue = sampler.sample_zone(&pixmap, &placed, &single_led_zone(0.75, 0.5));
        assert!(blue[0].b > blue[0].r, "x=0.75 should sample blue half");
    }

    // After a 90° rotation an LED at logical (0.5, 0.25) — top-center of the zone —
    // is rotated so its canvas x exceeds the zone center, landing in the blue (right) half.
    // Math: dx=0, dy=-0.125 (above center) → after 90°: new_dx=+0.125 → canvas x=62.5 → blue.
    #[test]
    fn rotation_90_shifts_sample_column() {
        let pixmap = red_blue_pixmap(100, 100);
        let sampler = Sampler::new(1.0);
        let placed = PlacedZone {
            device_id: "d".into(),
            channel_id: "z".into(),
            x: 0.25,
            y: 0.25,
            w: 0.5,
            h: 0.5,
            rotation: 90.0,
            effect: None,
            sampling_mode: Default::default(),
        };

        let colors = sampler.sample_zone(&pixmap, &placed, &single_led_zone(0.5, 0.25));
        assert!(
            colors[0].b > colors[0].r,
            "after 90° rotation, top-center LED should map to the blue (right) half"
        );
    }

    #[test]
    fn rotation_uniform_pixmap_is_invariant() {
        let mut pixmap = Pixmap::new(64, 64).unwrap();
        pixmap.fill(Color::from_rgba8(200, 100, 50, 255));
        let sampler = Sampler::new(2.0);

        let base = {
            let p = PlacedZone {
                rotation: 0.0,
                ..full_zone()
            };
            sampler.sample_zone(&pixmap, &p, &single_led_zone(0.5, 0.5))
        };

        for &deg in &[45.0f32, 90.0, 135.0, 180.0, 270.0] {
            let placed = PlacedZone {
                rotation: deg,
                ..full_zone()
            };
            let colors = sampler.sample_zone(&pixmap, &placed, &single_led_zone(0.5, 0.5));
            assert_eq!(
                colors[0], base[0],
                "rotation={deg}°: uniform pixmap must return same color as 0°"
            );
        }
    }
}
