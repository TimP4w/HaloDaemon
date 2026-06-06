use halod_protocol::types::{RgbColor, RgbZone};
use tiny_skia::Pixmap;

use crate::config::PlacedZone;
use super::effects::{linear_to_led, linear_to_srgb};

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
        LinearRgb { r: red / n, g: grn / n, b: blu / n }
    }

    fn led_canvas_pos(&self, lx: f32, ly: f32, placed: &PlacedZone, pw: f32, ph: f32) -> (f32, f32) {
        let cx = placed.x + placed.w / 2.0;
        let cy = placed.y + placed.h / 2.0;
        let mut dx = (lx - 0.5) * placed.w;
        let mut dy = (ly - 0.5) * placed.h;
        if placed.rotation.abs() > 1e-6 {
            let rad = placed.rotation.to_radians();
            let (s, c) = rad.sin_cos();
            (dx, dy) = (dx * c - dy * s, dx * s + dy * c);
        }
        ((cx + dx) * pw, (cy + dy) * ph)
    }

    pub fn sample_zone(&self, pixmap: &Pixmap, placed: &PlacedZone, zone: &RgbZone) -> Vec<RgbColor> {
        let pw = pixmap.width() as f32;
        let ph = pixmap.height() as f32;
        zone.leds
            .iter()
            .map(|led| {
                let (cx, cy) = self.led_canvas_pos(led.x, led.y, placed, pw, ph);
                let lin = self.sample_box(pixmap, cx, cy);
                RgbColor {
                    r: linear_to_led(lin.r / 255.0, LED_GAMMA),
                    g: linear_to_led(lin.g / 255.0, LED_GAMMA),
                    b: linear_to_led(lin.b / 255.0, LED_GAMMA),
                }
            })
            .collect()
    }

    pub fn pixmap_to_srgb_rgba(&self, pixmap: &Pixmap) -> Vec<u8> {
        let mut out = Vec::with_capacity((pixmap.width() * pixmap.height() * 4) as usize);
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
        out
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use halod_protocol::types::{LedPosition, ZoneTopology};
    use tiny_skia::Color;

    #[test]
    fn sample_zone_solid_color_returns_uniform_output() {
        let mut pixmap = Pixmap::new(64, 64).unwrap();
        pixmap.fill(Color::from_rgba8(128, 0, 64, 255));

        let placed = PlacedZone {
            device_id: "test".to_string(),
            zone_id: "ring".to_string(),
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            rotation: 0.0,
        };

        let zone = RgbZone {
            id: "ring".to_string(),
            name: "Ring".to_string(),
            topology: ZoneTopology::Ring,
            leds: vec![
                LedPosition { id: 0, x: 0.25, y: 0.5 },
                LedPosition { id: 1, x: 0.75, y: 0.5 },
            ],
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
        let rgba = sampler.pixmap_to_srgb_rgba(&pixmap);
        assert_eq!(rgba.len(), (4 * 4 * 4) as usize);
    }
}
