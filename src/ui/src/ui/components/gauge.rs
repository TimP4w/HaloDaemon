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

/// Sweep of an [`arc_gauge`]: three quarters of a turn, open at the bottom.
const ARC_SWEEP: f32 = TAU * 0.75;
const ARC_START: f32 = TAU * 0.375;

/// Points along an open dial arc from its lower-left start, clockwise across
/// the top, covering `fraction` of the sweep.
pub fn dial_points(center: Pos2, radius: f32, fraction: f32) -> Vec<Pos2> {
    let filled = (fraction.clamp(0.0, 1.0) * SEGMENTS as f32).round() as usize;
    (0..=filled)
        .map(|i| {
            let angle = ARC_START + ARC_SWEEP * (i as f32 / SEGMENTS as f32);
            Pos2::new(
                center.x + radius * angle.cos(),
                center.y + radius * angle.sin(),
            )
        })
        .collect()
}

/// A three-quarter dial with round caps: a track arc and the filled fraction.
pub fn arc_gauge(p: &egui::Painter, center: Pos2, diameter: f32, fraction: f32, color: Color32) {
    let ring_w = diameter * 9.0 / 112.0;
    let radius = diameter * 46.0 / 112.0;
    for (fraction, color) in [(1.0, theme::BORDER), (fraction, color)] {
        let pts = dial_points(center, radius, fraction);
        if pts.len() >= 2 {
            p.circle_filled(pts[0], ring_w / 2.0, color);
            p.circle_filled(pts[pts.len() - 1], ring_w / 2.0, color);
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
    fn dial_is_symmetric_and_open_at_the_bottom() {
        let pts = dial_points(Pos2::ZERO, 10.0, 1.0);
        let (first, last) = (pts[0], *pts.last().unwrap());
        assert!((first.x + last.x).abs() < 0.001 && (first.y - last.y).abs() < 0.001);
        assert!(first.y > 0.0 && first.x < 0.0, "{first:?}");
        assert!(pts.iter().all(|p| p.y <= first.y + 0.001));
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
