// SPDX-License-Identifier: GPL-3.0-or-later
//! Antialiased 2D primitives for the widget canvas, rasterized with tiny-skia.
//!
//! The canvas is straight-alpha `Rgba<u8>`, so shapes go through an 8-bit
//! coverage mask rather than a premultiplied `Pixmap`.

use image::{ImageBuffer, Pixel as _, Rgba};
use tiny_skia::{FillRule, LineCap, LineJoin, Mask, Path, PathBuilder, Stroke, Transform};

pub type Canvas<'a> = ImageBuffer<Rgba<u8>, &'a mut [u8]>;

const RESOLUTION_SCALE: f32 = 1.0;

pub fn fill_path(image: &mut Canvas, path: &Path, color: Rgba<u8>) {
    fill_paths(image, std::slice::from_ref(path), color);
}

/// Coverage accumulates into one mask before any pixel is touched, so
/// overlapping shapes blend into the canvas exactly once instead of darkening
/// every shared edge.
pub fn fill_paths(image: &mut Canvas, paths: &[Path], color: Rgba<u8>) {
    if color.0[3] == 0 || paths.is_empty() {
        return;
    }
    let mut bounds = paths[0].bounds();
    for path in &paths[1..] {
        let other = path.bounds();
        bounds = tiny_skia::Rect::from_ltrb(
            bounds.left().min(other.left()),
            bounds.top().min(other.top()),
            bounds.right().max(other.right()),
            bounds.bottom().max(other.bottom()),
        )
        .unwrap_or(bounds);
    }
    let left = (bounds.left().floor() as i64).clamp(0, i64::from(image.width()));
    let top = (bounds.top().floor() as i64).clamp(0, i64::from(image.height()));
    let right = (bounds.right().ceil() as i64).clamp(0, i64::from(image.width()));
    let bottom = (bounds.bottom().ceil() as i64).clamp(0, i64::from(image.height()));
    let (width, height) = ((right - left) as u32, (bottom - top) as u32);
    if width == 0 || height == 0 {
        return;
    }
    let Some(mut mask) = Mask::new(width, height) else {
        return;
    };
    let offset = Transform::from_translate(-left as f32, -top as f32);
    for path in paths {
        mask.fill_path(path, FillRule::Winding, true, offset);
    }

    let source_alpha = f32::from(color.0[3]);
    for (index, &coverage) in mask.data().iter().enumerate() {
        if coverage == 0 {
            continue;
        }
        let alpha = (source_alpha * f32::from(coverage) / 255.0).round() as u8;
        if alpha == 0 {
            continue;
        }
        let x = (left as u32) + (index as u32 % width);
        let y = (top as u32) + (index as u32 / width);
        image
            .get_pixel_mut(x, y)
            .blend(&Rgba([color.0[0], color.0[1], color.0[2], alpha]));
    }
}

fn stroke_of(width: f32) -> Stroke {
    Stroke {
        width: width.max(1.0),
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    }
}

fn polyline_path(points: &[(f32, f32)], closed: bool) -> Option<Path> {
    let (&first, rest) = points.split_first()?;
    let mut builder = PathBuilder::new();
    builder.move_to(first.0, first.1);
    for &(x, y) in rest {
        builder.line_to(x, y);
    }
    if closed {
        builder.close();
    }
    builder.finish()
}

pub fn fill_rect(image: &mut Canvas, x: f32, y: f32, width: f32, height: f32, color: Rgba<u8>) {
    let Some(rect) = tiny_skia::Rect::from_xywh(x, y, width, height) else {
        return;
    };
    fill_path(image, &PathBuilder::from_rect(rect), color);
}

pub fn fill_circle(image: &mut Canvas, center: (f32, f32), radius: f32, color: Rgba<u8>) {
    let mut builder = PathBuilder::new();
    builder.push_circle(center.0, center.1, radius.max(0.5));
    if let Some(path) = builder.finish() {
        fill_path(image, &path, color);
    }
}

pub fn stroke_circle(
    image: &mut Canvas,
    center: (f32, f32),
    radius: f32,
    width: f32,
    color: Rgba<u8>,
) {
    let mut builder = PathBuilder::new();
    builder.push_circle(center.0, center.1, radius.max(0.5));
    let Some(path) = builder.finish() else {
        return;
    };
    if let Some(stroked) = path.stroke(&stroke_of(width), RESOLUTION_SCALE) {
        fill_path(image, &stroked, color);
    }
}

pub fn fill_polygon(image: &mut Canvas, points: &[(f32, f32)], color: Rgba<u8>) {
    if points.len() < 3 {
        return;
    }
    if let Some(path) = polyline_path(points, true) {
        fill_path(image, &path, color);
    }
}

pub fn stroke_polyline(
    image: &mut Canvas,
    points: &[(f32, f32)],
    width: f32,
    closed: bool,
    color: Rgba<u8>,
) {
    if points.len() < 2 {
        return;
    }
    let Some(path) = polyline_path(points, closed) else {
        return;
    };
    if let Some(stroked) = path.stroke(&stroke_of(width), RESOLUTION_SCALE) {
        fill_path(image, &stroked, color);
    }
}

/// A ring segment of `thickness` centered on `radius`, sweeping `sweep_degrees`
/// from `start_degrees` (0° = up, positive = clockwise), with round caps of
/// `cap_radius`.
pub fn fill_arc(
    image: &mut Canvas,
    center: (f32, f32),
    radius: f32,
    thickness: f32,
    start_degrees: f32,
    sweep_degrees: f32,
    cap_radius: f32,
    color: Rgba<u8>,
) {
    if sweep_degrees.abs() < f32::EPSILON {
        return;
    }
    let half_thickness = (thickness / 2.0).clamp(0.5, radius * 0.9);
    let start = start_degrees.to_radians();
    let sweep = sweep_degrees.to_radians();
    let outer = radius + half_thickness;
    let inner = (radius - half_thickness).max(0.0);
    let steps = (radius * sweep.abs() / 2.0).ceil().clamp(1.0, 360.0) as usize;
    let at = |angle: f32, distance: f32| {
        (
            center.0 + angle.sin() * distance,
            center.1 - angle.cos() * distance,
        )
    };

    let mut paths = Vec::new();
    let mut band = PathBuilder::new();
    let first = at(start, outer);
    band.move_to(first.0, first.1);
    for step in 1..=steps {
        let angle = start + sweep * step as f32 / steps as f32;
        let (x, y) = at(angle, outer);
        band.line_to(x, y);
    }
    for step in (0..=steps).rev() {
        let angle = start + sweep * step as f32 / steps as f32;
        let (x, y) = at(angle, inner);
        band.line_to(x, y);
    }
    band.close();
    paths.extend(band.finish());

    // Caps extend past the sweep, so a rounded end reads as an overshoot rather
    // than eating into the value the arc represents.
    let cap_radius = cap_radius.clamp(0.0, half_thickness);
    if cap_radius >= 0.5 && sweep_degrees.abs() < 359.99 {
        let direction = sweep.signum();
        for (angle, tangent_sign) in [(start, -direction), (start + sweep, direction)] {
            let endpoint = at(angle, radius);
            let radial = (angle.sin(), -angle.cos());
            let tangent = (angle.cos() * tangent_sign, angle.sin() * tangent_sign);
            let inset = half_thickness - cap_radius;
            let offset = |normal: f32| {
                (
                    endpoint.0 + radial.0 * inset * normal,
                    endpoint.1 + radial.1 * inset * normal,
                )
            };
            let (upper, lower) = (offset(1.0), offset(-1.0));
            let mut caps = PathBuilder::new();
            caps.push_circle(upper.0, upper.1, cap_radius);
            caps.push_circle(lower.0, lower.1, cap_radius);
            paths.extend(caps.finish());

            if inset >= 0.5 {
                let mut bridge = PathBuilder::new();
                bridge.move_to(upper.0, upper.1);
                bridge.line_to(lower.0, lower.1);
                bridge.line_to(
                    lower.0 + tangent.0 * cap_radius,
                    lower.1 + tangent.1 * cap_radius,
                );
                bridge.line_to(
                    upper.0 + tangent.0 * cap_radius,
                    upper.1 + tangent.1 * cap_radius,
                );
                bridge.close();
                paths.extend(bridge.finish());
            }
        }
    }

    fill_paths(image, &paths, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas(size: u32) -> (Vec<u8>, u32) {
        (vec![0u8; (size * size * 4) as usize], size)
    }

    fn with_canvas(size: u32, draw: impl FnOnce(&mut Canvas)) -> Vec<u8> {
        let (mut bytes, size) = canvas(size);
        let mut image: Canvas = ImageBuffer::from_raw(size, size, bytes.as_mut_slice()).unwrap();
        draw(&mut image);
        bytes
    }

    fn alpha_at(bytes: &[u8], size: u32, x: u32, y: u32) -> u8 {
        bytes[((y * size + x) * 4 + 3) as usize]
    }

    const RED: Rgba<u8> = Rgba([255, 0, 0, 255]);

    #[test]
    fn a_filled_rect_covers_exactly_its_bounds() {
        let bytes = with_canvas(16, |image| fill_rect(image, 4.0, 4.0, 8.0, 8.0, RED));
        assert_eq!(alpha_at(&bytes, 16, 4, 4), 255);
        assert_eq!(alpha_at(&bytes, 16, 11, 11), 255);
        assert_eq!(alpha_at(&bytes, 16, 3, 4), 0);
        assert_eq!(alpha_at(&bytes, 16, 12, 11), 0);
    }

    #[test]
    fn a_filled_circle_is_opaque_at_the_center_and_clear_at_the_corners() {
        let bytes = with_canvas(32, |image| fill_circle(image, (16.0, 16.0), 10.0, RED));
        assert_eq!(alpha_at(&bytes, 32, 16, 16), 255);
        assert_eq!(alpha_at(&bytes, 32, 0, 0), 0);
        assert_eq!(alpha_at(&bytes, 32, 31, 31), 0);
    }

    #[test]
    fn a_stroked_circle_leaves_its_center_untouched() {
        let bytes = with_canvas(32, |image| {
            stroke_circle(image, (16.0, 16.0), 10.0, 3.0, RED)
        });
        assert_eq!(alpha_at(&bytes, 32, 16, 16), 0);
        assert!(alpha_at(&bytes, 32, 16, 6) > 0);
    }

    #[test]
    fn an_arc_leaves_its_center_untouched_and_covers_its_sweep() {
        let bytes = with_canvas(64, |image| {
            fill_arc(image, (32.0, 32.0), 20.0, 6.0, 0.0, 90.0, 0.0, RED)
        });
        assert_eq!(alpha_at(&bytes, 64, 32, 32), 0);
        // 0° is up, sweeping clockwise to 90° (right).
        assert!(alpha_at(&bytes, 64, 32, 12) > 0, "12 o'clock covered");
        assert!(alpha_at(&bytes, 64, 52, 30) > 0, "3 o'clock covered");
        assert_eq!(alpha_at(&bytes, 64, 12, 32), 0, "9 o'clock untouched");
    }

    #[test]
    fn drawing_outside_the_canvas_is_clipped_not_panicking() {
        let bytes = with_canvas(8, |image| {
            fill_rect(image, -50.0, -50.0, 10.0, 10.0, RED);
            fill_circle(image, (500.0, 500.0), 20.0, RED);
            stroke_polyline(image, &[(-100.0, -100.0), (900.0, 900.0)], 4.0, false, RED);
        });
        assert!(bytes.chunks_exact(4).any(|px| px[3] > 0), "diagonal drawn");
    }

    #[test]
    fn a_fully_transparent_color_draws_nothing() {
        let bytes = with_canvas(16, |image| {
            fill_rect(image, 0.0, 0.0, 16.0, 16.0, Rgba([255, 0, 0, 0]))
        });
        assert!(bytes.iter().all(|&b| b == 0));
    }

    #[test]
    fn overlapping_sub_shapes_do_not_double_blend_the_seam() {
        let half = with_canvas(64, |image| {
            stroke_polyline(
                image,
                &[(10.0, 10.0), (54.0, 10.0), (32.0, 50.0)],
                6.0,
                true,
                Rgba([255, 0, 0, 128]),
            )
        });
        let max_alpha = half
            .chunks_exact(4)
            .map(|px| px[3])
            .max()
            .expect("non-empty");
        assert_eq!(max_alpha, 128, "a seam blended the stroke over itself");
    }
}
