use std::f64::consts::PI;

use cairo;
use halod_protocol::types::{RgbDescriptor, ZoneTopology};

use super::{WState, Mode};

pub(super) const LED_R: f64 = 10.0;
pub(super) const CANVAS_PAD: f64 = 18.0;
const CANVAS_RADIUS: f64 = 14.0;

fn rounded_rect_path(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r,     r, -PI / 2.0,  0.0);
    cr.arc(x + w - r, y + h - r, r,  0.0,       PI / 2.0);
    cr.arc(x + r,     y + h - r, r,  PI / 2.0,  PI);
    cr.arc(x + r,     y + r,     r,  PI,        3.0 * PI / 2.0);
    cr.close_path();
}

/// A `Ring` zone is laid out on a circle in normalized space, so it must be
/// drawn in a square viewport — otherwise a non-square canvas flattens it into
/// an ellipse. Returns `(origin_x, origin_y, width, height)` of the viewport.
pub(super) fn zone_viewport(w: f64, h: f64, topology: &ZoneTopology) -> (f64, f64, f64, f64) {
    match topology {
        ZoneTopology::Ring => {
            let side = w.min(h);
            ((w - side) / 2.0, (h - side) / 2.0, side, side)
        }
        _ => (0.0, 0.0, w, h),
    }
}

pub(super) fn led_pos(x: f64, y: f64, w: f64, h: f64, topology: &ZoneTopology) -> (f64, f64) {
    let (ox, oy, vw, vh) = zone_viewport(w, h, topology);
    (ox + CANVAS_PAD + x * (vw - CANVAS_PAD * 2.0),
     oy + CANVAS_PAD + (1.0 - y) * (vh - CANVAS_PAD * 2.0))
}

pub(super) fn stroke_ellipse_guide(cr: &cairo::Context, cx: f64, cy: f64, rx: f64, ry: f64) {
    let _ = cr.save();
    cr.translate(cx, cy);
    cr.scale(rx, ry);
    cr.arc(0.0, 0.0, 1.0, 0.0, 2.0 * PI);
    cr.set_line_width(1.0 / rx.max(ry));
    let _ = cr.stroke();
    let _ = cr.restore();
}

pub(super) fn draw_leds(cr: &cairo::Context, w: i32, h: i32, state: &WState, descriptor: &RgbDescriptor) {
    use halod_protocol::types::RgbColor;

    let w = w as f64;
    let h = h as f64;

    // Clip to a rounded rectangle so the dark fill (and any subsequent paint,
    // including the rubber-band selection) respects the canvas card's radius.
    rounded_rect_path(cr, 0.0, 0.0, w, h, CANVAS_RADIUS);
    cr.clip();

    cr.set_source_rgba(0.05, 0.05, 0.07, 1.0);
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();

    let Some(zone) = descriptor.zones.get(state.selected_zone_idx) else { return };
    let is_per_led = matches!(state.mode, Mode::PerLed);

    cr.set_source_rgba(1.0, 1.0, 1.0, 0.07);
    let x_scale = w - CANVAS_PAD * 2.0;
    let y_scale = h - CANVAS_PAD * 2.0;
    match &zone.topology {
        ZoneTopology::Ring => {
            // Square viewport keeps the ring circular on a non-square canvas.
            let (ox, oy, vw, vh) = zone_viewport(w, h, &zone.topology);
            let r = 0.42 * (vw - CANVAS_PAD * 2.0);
            stroke_ellipse_guide(cr, ox + vw / 2.0, oy + vh / 2.0, r, r);
        }
        ZoneTopology::Rings { count } => {
            let n = *count as usize;
            let rx = (0.42 / n as f64) * x_scale;
            let ry = 0.42 * y_scale;
            for i in 0..n {
                let cx_norm = (i as f64 + 0.5) / n as f64;
                let cx_px = CANVAS_PAD + cx_norm * x_scale;
                stroke_ellipse_guide(cr, cx_px, h / 2.0, rx, ry);
            }
        }
        _ => {}
    }

    let zone_colors = state.current_zone_colors(descriptor);
    let fallback = RgbColor { r: 0, g: 0, b: 0 };
    let uniform = state.current_rgb();

    for led in &zone.leds {
        let (lx, ly) = led_pos(led.x as f64, led.y as f64, w, h, &zone.topology);
        let color = if is_per_led {
            zone_colors.and_then(|m| m.get(&led.id)).copied().unwrap_or(fallback)
        } else {
            uniform
        };
        let (r, g, b) =
            (color.r as f64 / 255.0, color.g as f64 / 255.0, color.b as f64 / 255.0);

        let glow = cairo::RadialGradient::new(lx, ly, 0.0, lx, ly, LED_R * 2.5);
        glow.add_color_stop_rgba(0.0, r, g, b, 0.5);
        glow.add_color_stop_rgba(1.0, r, g, b, 0.0);
        let _ = cr.set_source(&glow);
        cr.arc(lx, ly, LED_R * 2.5, 0.0, 2.0 * PI);
        let _ = cr.fill();

        cr.set_source_rgb(r, g, b);
        cr.arc(lx, ly, LED_R, 0.0, 2.0 * PI);
        let _ = cr.fill();

        if state.selected.contains(&led.id) {
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
            cr.set_line_width(2.0);
            cr.arc(lx, ly, LED_R + 3.5, 0.0, 2.0 * PI);
            let _ = cr.stroke();
        }
    }

    if let Some((sx, sy)) = state.rubber_start {
        let (ex, ey) = state.rubber_end;
        let rx = sx.min(ex);
        let ry = sy.min(ey);
        let rw = (sx - ex).abs();
        let rh = (sy - ey).abs();
        cr.set_source_rgba(0.3, 0.6, 1.0, 0.12);
        cr.rectangle(rx, ry, rw, rh);
        let _ = cr.fill();
        cr.set_source_rgba(0.4, 0.7, 1.0, 0.75);
        cr.set_line_width(1.0);
        cr.rectangle(rx, ry, rw, rh);
        let _ = cr.stroke();
    }
}
