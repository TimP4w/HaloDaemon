// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure rotation math for the stage: point/rect rotation about a widget's
//! centre, and angle snapping near the right angles.

use egui::{Pos2, Rect, Vec2};

/// Whether a rotation is a significant partial turn (mirrors `rotation_theta`).
pub(super) fn rotation_active(deg: f32) -> bool {
    let norm = deg.rem_euclid(360.0);
    norm > 0.05 && norm < 359.95
}

/// Rotate `pt` clockwise about `origin` given precomputed `sin`/`cos`.
pub(super) fn rotate_about(pt: Pos2, origin: Pos2, sin: f32, cos: f32) -> Pos2 {
    let d = pt - origin;
    origin + Vec2::new(d.x * cos - d.y * sin, d.x * sin + d.y * cos)
}

/// The corners of `rect` (TL, TR, BR, BL) rotated clockwise by `deg` about `origin`.
pub(super) fn rotated_corners(rect: Rect, origin: Pos2, deg: f32) -> [Pos2; 4] {
    let (sin, cos) = deg.to_radians().sin_cos();
    let r = |pt| rotate_about(pt, origin, sin, cos);
    [
        r(rect.left_top()),
        r(rect.right_top()),
        r(rect.right_bottom()),
        r(rect.left_bottom()),
    ]
}

/// Snap half-window (degrees); tighter than the canvas's 5° to feel less sticky.
const ROTATE_SNAP_DEG: f32 = 2.5;

/// Snap to the nearest multiple of 90° within `ROTATE_SNAP_DEG`, wrapped to `[0, 360)`.
pub(super) fn snap_rotation(deg: f32) -> f32 {
    let snapped = (deg / 90.0).round() * 90.0;
    let out = if (deg - snapped).abs() < ROTATE_SNAP_DEG {
        snapped
    } else {
        deg
    };
    out.rem_euclid(360.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_rotation_snaps_near_right_angles_and_normalizes() {
        // Within the snap window of a right angle snaps exactly; result in [0, 360).
        assert_eq!(snap_rotation(2.0), 0.0);
        assert_eq!(snap_rotation(91.0), 90.0);
        assert_eq!(snap_rotation(-2.0), 0.0);
        assert_eq!(snap_rotation(-89.0), 270.0);
        // Outside the window is left as-is (but wrapped into range).
        assert_eq!(snap_rotation(80.0), 80.0);
        assert_eq!(snap_rotation(-100.0), 260.0);
    }

    #[test]
    fn rotation_active_matches_daemon_gate() {
        assert!(!rotation_active(0.0));
        assert!(!rotation_active(360.0));
        assert!(!rotation_active(-360.0));
        assert!(rotation_active(90.0));
        assert!(rotation_active(-45.0));
    }

    #[test]
    fn rotated_corners_at_zero_matches_rect() {
        let rect = Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(40.0, 60.0));
        let c = rotated_corners(rect, rect.center(), 0.0);
        assert_eq!(c[0], rect.left_top());
        assert_eq!(c[2], rect.right_bottom());
    }

    #[test]
    fn rotated_corners_90_maps_top_right_to_bottom() {
        // A +90° clockwise turn about the centre sends the top edge to the
        // right side; the top-right corner lands at the bottom-right.
        let rect = Rect::from_min_max(egui::pos2(-1.0, -1.0), egui::pos2(1.0, 1.0));
        let c = rotated_corners(rect, egui::Pos2::ZERO, 90.0);
        // TL(-1,-1) -> (1,-1)
        assert!((c[0].x - 1.0).abs() < 1e-4 && (c[0].y + 1.0).abs() < 1e-4);
        // TR(1,-1) -> (1,1)
        assert!((c[1].x - 1.0).abs() < 1e-4 && (c[1].y - 1.0).abs() < 1e-4);
    }
}
