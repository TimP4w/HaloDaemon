// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure geometry: canvas ⇄ normalized-zone coordinate mapping, drag/resize
//! math, and hit-shape helpers shared by the viewport and zone renderer.

use egui::{Pos2, Rect, Vec2};
use halod_shared::types::{LedPosition, PlacedZone};

use super::{DragState, Handle, MarqueeState};

/// The largest rect of the given `aspect` (w/h) that fits inside `outer`,
/// centred — i.e. an aspect-preserving letterbox.
pub(super) fn letterbox(outer: Rect, aspect: f32) -> Rect {
    if aspect <= 0.0 || !aspect.is_finite() {
        return outer;
    }
    let (ow, oh) = (outer.width(), outer.height());
    if oh <= 0.0 {
        return outer;
    }
    let (w, h) = if ow / oh > aspect {
        (oh * aspect, oh)
    } else {
        (ow, ow / aspect)
    };
    Rect::from_center_size(outer.center(), Vec2::new(w, h))
}

/// Pre-computed-sin/cos variant; call when sin/cos are already available to
/// avoid recomputing the transcendentals.
pub(super) fn rounded_zone_outline_sc(
    zone: &PlacedZone,
    canvas_rect: Rect,
    radius: f32,
    sin: f32,
    cos: f32,
) -> Vec<Pos2> {
    let cx = zone.x + zone.w / 2.0;
    let cy = zone.y + zone.h / 2.0;
    let hw = zone.w / 2.0;
    let hh = zone.h / 2.0;
    let rx = (radius / canvas_rect.width()).min(hw).max(0.0);
    let ry = (radius / canvas_rect.height()).min(hh).max(0.0);
    let map = |lx: f32, ly: f32| {
        norm_to_screen(
            Pos2::new(cx + lx * cos - ly * sin, cy + lx * sin + ly * cos),
            canvas_rect,
        )
    };
    const SEG: usize = 4;
    let arcs = [
        ((-hw + rx, -hh + ry), 180.0_f32), // TL
        ((hw - rx, -hh + ry), 270.0),      // TR
        ((hw - rx, hh - ry), 0.0),         // BR
        ((-hw + rx, hh - ry), 90.0),       // BL
    ];
    let mut pts = Vec::with_capacity(4 * (SEG + 1));
    for ((ax, ay), start) in arcs {
        for i in 0..=SEG {
            let ang = (start + 90.0 * i as f32 / SEG as f32).to_radians();
            pts.push(map(ax + rx * ang.cos(), ay + ry * ang.sin()));
        }
    }
    pts
}

/// Screen position of an LED at normalized in-zone coords `(lx, ly)` ∈ [0,1],
/// applying the zone's rotation. Both this and the daemon's canvas sampler
/// delegate to `halod_shared::zone_transform::led_canvas_norm` for the
/// placement math; only the final scaling (screen vs. pixel) differs.
pub(super) fn led_screen_pos(lx: f32, ly: f32, zone: &PlacedZone, canvas_rect: Rect) -> Pos2 {
    // Inset the LED toward the zone centre by a fixed screen margin so dots keep
    // a consistent gap from the box border regardless of zone size (a fractional
    // inset pushes LEDs far from the edge on large zones).
    let lx = inset_axis(lx, zone.w * canvas_rect.width());
    let ly = inset_axis(ly, zone.h * canvas_rect.height());
    let (nx, ny) = halod_shared::zone_transform::led_canvas_norm(
        lx,
        ly,
        zone.x,
        zone.y,
        zone.w,
        zone.h,
        zone.rotation,
    );
    norm_to_screen(Pos2::new(nx, ny), canvas_rect)
}

/// Bounding box `(min_x, max_x, min_y, max_y)` of a zone's LED cloud in the
/// descriptor's own coordinate space. Empty zones report a centred degenerate box.
pub(super) fn led_bounds(leds: &[LedPosition]) -> (f32, f32, f32, f32) {
    leds.iter().fold(
        (
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ),
        |(minx, maxx, miny, maxy), l| (minx.min(l.x), maxx.max(l.x), miny.min(l.y), maxy.max(l.y)),
    )
}

/// Remap an LED into `[0,1]` against its cloud's extent so the outermost LEDs
/// reach the box edge. A degenerate axis stays centred.
pub(super) fn fill_coords(
    led: &LedPosition,
    (minx, maxx, miny, maxy): (f32, f32, f32, f32),
) -> (f32, f32) {
    let fill = |v: f32, min: f32, max: f32| {
        if max > min {
            (v - min) / (max - min)
        } else {
            0.5
        }
    };
    (fill(led.x, minx, maxx), fill(led.y, miny, maxy))
}

/// Screen-pixel margin reserved between edge LEDs and the box border. Kept in
/// absolute px (not a fraction of the zone) so the gap reads the same on small
/// and large zones and edge LEDs can sit close to the border.
const LED_PAD_PX: f32 = 4.0;

/// Map an in-zone coord `[0,1]` into `[pad, 1-pad]` (`pad` = `LED_PAD_PX` as a
/// fraction of `span_px`), clamped so the inset never crosses the zone centre.
fn inset_axis(t: f32, span_px: f32) -> f32 {
    let pad = if span_px > 0.0 {
        (LED_PAD_PX / span_px).min(0.5)
    } else {
        0.0
    };
    0.5 + (t - 0.5) * (1.0 - 2.0 * pad)
}

/// Zones whose rotation-aware screen bounding box intersects the marquee rect.
pub(super) fn zones_in_marquee(
    zones: &[PlacedZone],
    mq: &MarqueeState,
    canvas_rect: Rect,
) -> std::collections::HashSet<(String, String)> {
    let m = Rect::from_two_pos(
        norm_to_screen(mq.start_norm, canvas_rect),
        norm_to_screen(mq.cur_norm, canvas_rect),
    );
    let mut out = std::collections::HashSet::new();
    for z in zones {
        let corners = zone_corners(z, canvas_rect);
        let zb = Rect::from_points(&corners);
        if m.intersects(zb) {
            out.insert((z.device_id.clone(), z.zone_id.clone()));
        }
    }
    out
}

// ── Drag application ──────────────────────────────────────────────────────────
/// Apply a drag to the dragged zone. `delta` is the normalized-canvas drag
/// vector (cur − press); `cur_screen` is the live pointer in screen pixels
/// (used for rotation, which is angle-based and aspect-sensitive).
pub(super) fn apply_drag(
    drag: &DragState,
    delta: Vec2,
    cur_screen: Pos2,
    canvas_rect: Rect,
) -> PlacedZone {
    let orig = &drag.orig;
    match drag.handle {
        Handle::Body => body_move(orig, delta),
        Handle::Corner(c) => resize_from_corner(orig, c, delta),
        Handle::Rotation => {
            // Angle delta between press and current cursor about the zone centre,
            // measured in screen pixels so the canvas aspect ratio is respected.
            let center = norm_to_screen(
                Pos2::new(orig.x + orig.w / 2.0, orig.y + orig.h / 2.0),
                canvas_rect,
            );
            let angle_now = (cur_screen.y - center.y)
                .atan2(cur_screen.x - center.x)
                .to_degrees();
            let angle_start = (drag.press_screen.y - center.y)
                .atan2(drag.press_screen.x - center.x)
                .to_degrees();
            let rotation = snap_rotation(orig.rotation + (angle_now - angle_start));
            PlacedZone {
                rotation,
                ..orig.clone()
            }
        }
    }
}

/// Translate a zone by `delta` (normalized-canvas units), clamping so it stays
/// on-canvas. `max()` guards oversized zones, where `1.0 - w` goes negative.
pub(super) fn body_move(orig: &PlacedZone, delta: Vec2) -> PlacedZone {
    PlacedZone {
        x: (orig.x + delta.x).clamp(0.0, (1.0 - orig.w).max(0.0)),
        y: (orig.y + delta.y).clamp(0.0, (1.0 - orig.h).max(0.0)),
        ..orig.clone()
    }
}

/// Snap an angle to the nearest multiple of 90° when within ±5° of it.
fn snap_rotation(rotation: f32) -> f32 {
    let snapped = (rotation / 90.0).round() * 90.0;
    if (rotation - snapped).abs() < 5.0 {
        snapped
    } else {
        rotation
    }
}

/// Resize keeping the corner opposite the dragged one pinned in canvas space,
/// working in the zone's local (un-rotated) axes so rotation is preserved.
/// `delta` is the normalized-canvas drag vector (cur − press).
fn resize_from_corner(orig: &PlacedZone, c: usize, delta: Vec2) -> PlacedZone {
    const MIN: f32 = super::MIN_ZONE;
    let (s, cos) = orig.rotation.to_radians().sin_cos();

    let ldx = delta.x * cos + delta.y * s;
    let ldy = -delta.x * s + delta.y * cos;

    // Local sign of the dragged corner (egui order: 0=TL 1=TR 2=BR 3=BL).
    let (sx, sy) = match c {
        0 => (-1.0_f32, -1.0_f32),
        1 => (1.0, -1.0),
        2 => (1.0, 1.0),
        _ => (-1.0, 1.0),
    };
    let (ow, oh) = (orig.w, orig.h);
    let new_w = (ow + sx * ldx).max(MIN);
    let new_h = (oh + sy * ldy).max(MIN);

    let ocx = orig.x + ow / 2.0;
    let ocy = orig.y + oh / 2.0;
    let (alx, aly) = (-sx * ow / 2.0, -sy * oh / 2.0);
    let anchor_x = ocx + alx * cos - aly * s;
    let anchor_y = ocy + alx * s + aly * cos;

    let (nlx, nly) = (-sx * new_w / 2.0, -sy * new_h / 2.0);
    let ncx = anchor_x - (nlx * cos - nly * s);
    let ncy = anchor_y - (nlx * s + nly * cos);

    PlacedZone {
        x: ncx - new_w / 2.0,
        y: ncy - new_h / 2.0,
        w: new_w,
        h: new_h,
        rotation: orig.rotation,
        ..orig.clone()
    }
}

// ── Coordinate mapping ────────────────────────────────────────────────────────
pub(super) fn zone_key(device_id: &str, zone_id: &str) -> String {
    format!("{device_id}:{zone_id}")
}

pub(super) fn norm_to_screen(norm: Pos2, canvas_rect: Rect) -> Pos2 {
    Pos2::new(
        canvas_rect.left() + norm.x * canvas_rect.width(),
        canvas_rect.top() + norm.y * canvas_rect.height(),
    )
}

pub(super) fn screen_to_norm(screen: Pos2, canvas_rect: Rect) -> Pos2 {
    Pos2::new(
        ((screen.x - canvas_rect.left()) / canvas_rect.width()).clamp(0.0, 1.0),
        ((screen.y - canvas_rect.top()) / canvas_rect.height()).clamp(0.0, 1.0),
    )
}

pub(super) fn zone_corners(zone: &PlacedZone, canvas_rect: Rect) -> [Pos2; 4] {
    // Rotate the local corner offsets in *normalized* canvas space (then map to
    // screen) so the box tracks the LEDs and the daemon sampler, which both
    // rotate via `zone_transform::led_canvas_norm`. Rotating in screen space
    // here would shear the box away from the LEDs whenever the canvas isn't
    // square. Matches `point_in_zone`'s normalized hit math.
    let (sin, cos) = zone.rotation.to_radians().sin_cos();
    zone_corners_sc(zone, canvas_rect, sin, cos)
}

/// Pre-computed-sin/cos variant; avoids recomputing transcendentals when
/// sin/cos are already available from the same frame.
pub(super) fn zone_corners_sc(
    zone: &PlacedZone,
    canvas_rect: Rect,
    sin: f32,
    cos: f32,
) -> [Pos2; 4] {
    let cx = zone.x + zone.w / 2.0;
    let cy = zone.y + zone.h / 2.0;
    let hw = zone.w / 2.0;
    let hh = zone.h / 2.0;
    let r = |dx: f32, dy: f32| {
        norm_to_screen(
            Pos2::new(cx + dx * cos - dy * sin, cy + dx * sin + dy * cos),
            canvas_rect,
        )
    };
    [r(-hw, -hh), r(hw, -hh), r(hw, hh), r(-hw, hh)]
}

pub(super) fn point_in_zone(norm: Pos2, zone: &PlacedZone) -> bool {
    let cx = zone.x + zone.w / 2.0;
    let cy = zone.y + zone.h / 2.0;
    let angle = -zone.rotation.to_radians();
    let dx = norm.x - cx;
    let dy = norm.y - cy;
    let lx = dx * angle.cos() - dy * angle.sin();
    let ly = dx * angle.sin() + dy * angle.cos();
    lx.abs() <= zone.w / 2.0 && ly.abs() <= zone.h / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screens::canvas::test_fixtures::{drag, r, z};

    #[test]
    fn point_inside_unrotated_zone() {
        let zone = z(0.1, 0.1, 0.3, 0.3);
        assert!(point_in_zone(Pos2::new(0.25, 0.25), &zone));
        assert!(!point_in_zone(Pos2::new(0.5, 0.5), &zone));
    }

    #[test]
    fn rounded_outline_centroid_matches_center() {
        let zone = z(0.2, 0.3, 0.4, 0.2);
        let rect = r();
        let (sin, cos) = zone.rotation.to_radians().sin_cos();
        let pts = rounded_zone_outline_sc(&zone, rect, 7.0, sin, cos);
        assert!(!pts.is_empty());
        let cx: f32 = pts.iter().map(|p| p.x).sum::<f32>() / pts.len() as f32;
        let cy: f32 = pts.iter().map(|p| p.y).sum::<f32>() / pts.len() as f32;
        let ecx = (zone.x + zone.w / 2.0) * rect.width();
        let ecy = (zone.y + zone.h / 2.0) * rect.height();
        assert!((cx - ecx).abs() < 1.0, "cx {cx} vs {ecx}");
        assert!((cy - ecy).abs() < 1.0, "cy {cy} vs {ecy}");
    }

    #[test]
    fn letterbox_preserves_aspect_and_fits() {
        // Wide box, 1:1 content → square centred inside.
        let outer = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 400.0));
        let lb = letterbox(outer, 1.0);
        assert!((lb.width() - lb.height()).abs() < 1e-3);
        assert!(lb.width() <= outer.width() + 1e-3 && lb.height() <= outer.height() + 1e-3);
        assert!((lb.center() - outer.center()).length() < 1e-3);
    }

    #[test]
    fn led_pos_is_inset_from_zone_edge() {
        // An LED at the zone's right edge (lx=1) is pulled inward by exactly
        // LED_PAD_PX screen pixels, not by a fraction of the zone.
        let zone = z(0.2, 0.2, 0.4, 0.4); // spans x∈[0.2,0.6], width 0.4*800=320px
        let p = led_screen_pos(1.0, 0.5, &zone, r());
        let edge_x = 0.6 * r().width();
        assert!(p.x < edge_x, "LED should sit inside the right edge");
        assert!(
            (p.x - (edge_x - LED_PAD_PX)).abs() < 0.5,
            "x={} vs {}",
            p.x,
            edge_x - LED_PAD_PX
        );
    }

    #[test]
    fn fill_coords_expands_cloud_to_edges() {
        use halod_shared::types::LedPosition;
        // Ambiglow-style cloud: x inset from the edges (0.02..0.98), y spanning
        // the full height (0.0..1.0). Fill must push the x extremes to 0/1 while
        // leaving y untouched — killing the left/right-vs-top/bottom asymmetry.
        let leds = vec![
            LedPosition {
                id: 0,
                x: 0.02,
                y: 0.0,
            },
            LedPosition {
                id: 1,
                x: 0.98,
                y: 1.0,
            },
            LedPosition {
                id: 2,
                x: 0.5,
                y: 0.5,
            },
        ];
        let b = led_bounds(&leds);
        assert_eq!(fill_coords(&leds[0], b), (0.0, 0.0));
        assert_eq!(fill_coords(&leds[1], b), (1.0, 1.0));
        let (cx, cy) = fill_coords(&leds[2], b);
        assert!((cx - 0.5).abs() < 1e-6 && (cy - 0.5).abs() < 1e-6);
    }

    #[test]
    fn fill_coords_degenerate_axis_centers() {
        use halod_shared::types::LedPosition;
        // Single-row strip: y is constant, so the y axis stays centred rather
        // than dividing by a zero range.
        let leds = vec![
            LedPosition {
                id: 0,
                x: 0.0,
                y: 0.5,
            },
            LedPosition {
                id: 1,
                x: 1.0,
                y: 0.5,
            },
        ];
        let b = led_bounds(&leds);
        assert_eq!(fill_coords(&leds[0], b), (0.0, 0.5));
        assert_eq!(fill_coords(&leds[1], b), (1.0, 0.5));
    }

    #[test]
    fn inset_axis_keeps_center_and_is_constant_px() {
        // The zone centre is unaffected by the inset; edges contract by a fixed
        // pixel margin, so a zone twice as wide gets half the fractional pad.
        assert_eq!(inset_axis(0.5, 320.0), 0.5);
        let narrow = inset_axis(1.0, 320.0);
        let wide = inset_axis(1.0, 640.0);
        assert!((narrow - (1.0 - LED_PAD_PX / 320.0)).abs() < 1e-6);
        assert!((wide - (1.0 - LED_PAD_PX / 640.0)).abs() < 1e-6);
        // Pad clamps to half so tiny zones don't invert.
        assert_eq!(inset_axis(1.0, 4.0), 0.5);
    }

    #[test]
    fn corners_center_matches_zone_center() {
        let zone = z(0.1, 0.2, 0.4, 0.3);
        let rect = r();
        let corners = zone_corners(&zone, rect);
        let cx: f32 = corners.iter().map(|p| p.x).sum::<f32>() / 4.0;
        let cy: f32 = corners.iter().map(|p| p.y).sum::<f32>() / 4.0;
        let ecx = (zone.x + zone.w / 2.0) * rect.width();
        let ecy = (zone.y + zone.h / 2.0) * rect.height();
        assert!((cx - ecx).abs() < 0.01, "cx mismatch: {cx} vs {ecx}");
        assert!((cy - ecy).abs() < 0.01, "cy mismatch: {cy} vs {ecy}");
    }

    #[test]
    fn body_drag_clamps() {
        let zone = z(0.0, 0.0, 0.2, 0.2);
        let result = apply_drag(
            &drag(&zone, Handle::Body),
            Vec2::new(-0.5, -0.5),
            Pos2::ZERO,
            r(),
        );
        assert!(result.x >= 0.0 && result.y >= 0.0);
    }

    #[test]
    fn body_drag_oversized_zone_does_not_panic() {
        // A zone wider/taller than the canvas makes `1.0 - w` negative; the drag
        // must clamp to 0 instead of panicking in clamp(min > max).
        let zone = z(0.1, 0.1, 1.2, 1.3);
        let result = apply_drag(
            &drag(&zone, Handle::Body),
            Vec2::new(0.5, 0.5),
            Pos2::ZERO,
            r(),
        );
        assert_eq!(result.x, 0.0);
        assert_eq!(result.y, 0.0);
    }

    #[test]
    fn resize_corner_enforces_min_size() {
        // Drag BR corner far past the opposite corner — clamps to MIN_ZONE.
        let zone = z(0.4, 0.4, 0.2, 0.2);
        let result = resize_from_corner(&zone, 2, Vec2::new(-0.5, -0.5));
        assert!(result.w >= super::super::MIN_ZONE);
        assert!(result.h >= super::super::MIN_ZONE);
    }

    #[test]
    fn resize_unrotated_pins_opposite_corner() {
        // Dragging BR by (+0.1,+0.1) keeps TL at (0.4,0.4) and grows by 0.1.
        let zone = z(0.4, 0.4, 0.2, 0.2);
        let result = resize_from_corner(&zone, 2, Vec2::new(0.1, 0.1));
        assert!((result.x - 0.4).abs() < 1e-5, "x={}", result.x);
        assert!((result.y - 0.4).abs() < 1e-5, "y={}", result.y);
        assert!((result.w - 0.3).abs() < 1e-5, "w={}", result.w);
        assert!((result.h - 0.3).abs() < 1e-5, "h={}", result.h);
    }

    #[test]
    fn resize_preserves_rotation() {
        let mut zone = z(0.4, 0.4, 0.2, 0.2);
        zone.rotation = 90.0;
        let result = resize_from_corner(&zone, 2, Vec2::new(0.05, 0.05));
        assert!((result.rotation - 90.0).abs() < 1e-5);
    }

    #[test]
    fn body_move_shifts_group_by_a_shared_delta() {
        // A group move applies the same delta to every selected zone, so their
        // relative offset is preserved.
        let a = z(0.1, 0.1, 0.2, 0.2);
        let mut b = z(0.5, 0.4, 0.2, 0.2);
        b.zone_id = "z2".into();
        let d = Vec2::new(0.1, 0.05);
        let (ma, mb) = (body_move(&a, d), body_move(&b, d));
        assert!((ma.x - 0.2).abs() < 1e-5 && (ma.y - 0.15).abs() < 1e-5);
        assert!((mb.x - 0.6).abs() < 1e-5 && (mb.y - 0.45).abs() < 1e-5);
        // Relative offset unchanged.
        assert!((mb.x - ma.x - (b.x - a.x)).abs() < 1e-5);
        assert!((mb.y - ma.y - (b.y - a.y)).abs() < 1e-5);
    }

    #[test]
    fn screen_norm_roundtrip() {
        let rect = r();
        let norm = Pos2::new(0.5, 0.3);
        let back = screen_to_norm(norm_to_screen(norm, rect), rect);
        assert!((back.x - norm.x).abs() < 1e-5);
        assert!((back.y - norm.y).abs() < 1e-5);
    }

    #[test]
    fn rotation_snaps_within_five_degrees() {
        // Within ±5° of a multiple of 90° → snaps exactly.
        assert_eq!(snap_rotation(92.0), 90.0);
        assert_eq!(snap_rotation(-3.0), 0.0);
        assert_eq!(snap_rotation(184.0), 180.0);
        assert_eq!(snap_rotation(86.0), 90.0);
    }

    #[test]
    fn rotation_does_not_snap_outside_five_degrees() {
        // Outside the ±5° window → left untouched.
        assert_eq!(snap_rotation(80.0), 80.0);
        assert_eq!(snap_rotation(96.0), 96.0);
        assert_eq!(snap_rotation(45.0), 45.0);
    }

    #[test]
    fn zone_corners_sc_matches_zone_corners() {
        // The _sc variant with pre-computed sin/cos must produce the same result
        // as the wrapper that recomputes them internally.
        let zone = z(0.1, 0.2, 0.4, 0.3);
        let rect = r();
        let (sin, cos) = zone.rotation.to_radians().sin_cos();
        let expected = zone_corners(&zone, rect);
        let got = zone_corners_sc(&zone, rect, sin, cos);
        for (e, g) in expected.iter().zip(got.iter()) {
            assert!((e.x - g.x).abs() < 1e-4, "x mismatch: {} vs {}", e.x, g.x);
            assert!((e.y - g.y).abs() < 1e-4, "y mismatch: {} vs {}", e.y, g.y);
        }
    }
}
