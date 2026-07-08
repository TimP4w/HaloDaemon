// SPDX-License-Identifier: GPL-3.0-or-later
//! Small math/paint helpers with no natural home elsewhere.

use egui::{Color32, Pos2};

/// Clamp `v` into `[min, max]` then round to the nearest multiple of `step`
/// (relative to `min`). `step <= 0` just clamps. Shared by the lighting,
/// equalizer and control panels so each doesn't re-derive the snap math.
pub fn snap_to_step(v: f32, min: f32, max: f32, step: f32) -> f32 {
    let clamped = v.clamp(min, max);
    if step <= 0.0 {
        return clamped;
    }
    (min + ((clamped - min) / step).round() * step).clamp(min, max)
}

/// Fill the area under a polyline down to `baseline_y` with `color`, as a
/// triangle-strip mesh. Shared by the sparkline, the curve editor and the
/// read-only cooling curve preview.
pub fn fill_under_line(painter: &egui::Painter, pts: &[Pos2], baseline_y: f32, color: Color32) {
    if pts.len() < 2 {
        return;
    }
    let mut mesh = egui::Mesh::default();
    for w in pts.windows(2) {
        let base = mesh.vertices.len() as u32;
        for v in [
            w[0],
            w[1],
            Pos2::new(w[1].x, baseline_y),
            Pos2::new(w[0].x, baseline_y),
        ] {
            mesh.colored_vertex(v, color);
        }
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    }
    painter.add(egui::Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_to_step_rounds_and_clamps() {
        assert_eq!(snap_to_step(0.0, 0.0, 100.0, 5.0), 0.0);
        assert_eq!(snap_to_step(12.0, 0.0, 100.0, 5.0), 10.0);
        assert_eq!(snap_to_step(13.0, 0.0, 100.0, 5.0), 15.0);
        assert_eq!(snap_to_step(-5.0, 0.0, 100.0, 5.0), 0.0); // clamp low
        assert_eq!(snap_to_step(150.0, 0.0, 100.0, 5.0), 100.0); // clamp high
        assert_eq!(snap_to_step(13.7, 0.0, 100.0, 0.0), 13.7); // step 0 = clamp only
    }
}
