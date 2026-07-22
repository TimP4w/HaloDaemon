// SPDX-License-Identifier: GPL-3.0-or-later
//! Radial ring gauge: a full track circle with the filled fraction drawn
//! clockwise from twelve o'clock.

use std::f32::consts::{FRAC_PI_2, TAU};

use egui::{Color32, Pos2, Shape, Stroke};

use crate::ui::theme;

const SEGMENTS: usize = 64;

/// Ring thickness for a given dial size, so a large gauge doesn't read as a
/// hairline and a small one doesn't close up its hole.
fn ring_width(diameter: f32) -> f32 {
    (diameter * 0.12).clamp(4.0, 12.0)
}

/// Points along the filled arc, clockwise from twelve o'clock. Fewer than two
/// points means there is nothing for [`Shape::line`] to draw.
pub fn arc_points(center: Pos2, radius: f32, fraction: f32) -> Vec<Pos2> {
    let filled = (fraction.clamp(0.0, 1.0) * SEGMENTS as f32).round() as usize;
    (0..=filled)
        .map(|i| {
            let angle = TAU * (i as f32 / SEGMENTS as f32) - FRAC_PI_2;
            Pos2::new(
                center.x + radius * angle.cos(),
                center.y + radius * angle.sin(),
            )
        })
        .collect()
}

/// Paint the gauge ring. `diameter` is the outer size; the hole is left for the
/// caller to fill with a readout. Track and fill are the same primitive so the
/// filled part never reads as a thinner or smaller ring than its track.
pub fn ring_gauge(p: &egui::Painter, center: Pos2, diameter: f32, fraction: f32, color: Color32) {
    let ring_w = ring_width(diameter);
    let radius = (diameter - ring_w) / 2.0;
    for (fraction, color) in [(1.0, theme::TRACK), (fraction, color)] {
        let pts = arc_points(center, radius, fraction);
        if pts.len() >= 2 {
            p.add(Shape::line(pts, Stroke::new(ring_w, color)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_starts_at_twelve_oclock_and_sweeps_clockwise() {
        let pts = arc_points(Pos2::ZERO, 10.0, 0.25);
        assert!(
            pts[0].x.abs() < 0.001 && (pts[0].y + 10.0).abs() < 0.001,
            "{:?}",
            pts[0]
        );
        let last = *pts.last().unwrap();
        assert!(
            (last.x - 10.0).abs() < 0.001 && last.y.abs() < 0.001,
            "{last:?}"
        );
    }

    #[test]
    fn arc_clamps_outside_the_unit_range() {
        assert!(arc_points(Pos2::ZERO, 10.0, -1.0).len() < 2);
        assert_eq!(
            arc_points(Pos2::ZERO, 10.0, 4.0).len(),
            arc_points(Pos2::ZERO, 10.0, 1.0).len()
        );
    }
}
