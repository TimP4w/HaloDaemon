use std::cell::RefCell;
use std::rc::Rc;

use gtk4::{self as gtk, prelude::WidgetExt};

/// How a zone is being dragged.
#[derive(Clone)]
pub(super) enum DragMode {
    Move { start_zone_x: f32, start_zone_y: f32 },
    Resize { corner: u8, orig_x: f32, orig_y: f32, orig_w: f32, orig_h: f32, orig_rotation: f32 },
    Rotate { zone_cx_screen: f64, zone_cy_screen: f64, orig_rotation: f32 },
}

#[derive(Clone)]
pub(super) struct ZoneDrag {
    pub(super) device_id: String,
    pub(super) zone_id: String,
    pub(super) press_x: f64,
    pub(super) press_y: f64,
    pub(super) mode: DragMode,
}

/// Returns `(scale, ox, oy, cw, ch)` — the scaling factors from canvas-pixel space
/// to drawing-area screen space.
pub(super) fn canvas_dims(
    da: &gtk::DrawingArea,
    canvas_bytes: &Rc<RefCell<Option<(Vec<u8>, u32, u32)>>>,
) -> (f64, f64, f64, f64, f64) {
    let w = da.width() as f64;
    let h = da.height() as f64;
    let (cw, ch) = canvas_bytes
        .borrow()
        .as_ref()
        .map(|(_, cw, ch)| (*cw as f64, *ch as f64))
        .unwrap_or((64.0, 64.0));
    let scale = (w / cw).min(h / ch);
    let ox = (w - cw * scale) / 2.0;
    let oy = (h - ch * scale) / 2.0;
    (scale, ox, oy, cw, ch)
}

/// Convert an LED position (normalised within its zone) to screen pixels.
/// Mirrors the daemon-side `led_canvas_pos` function, including rotation.
pub(super) fn led_canvas_screen(
    lx: f32,
    ly: f32,
    zone: &halod_protocol::types::WirePlacedZone,
    cw: f64,
    ch: f64,
    scale: f64,
    ox: f64,
    oy: f64,
) -> (f64, f64) {
    let cx = zone.x + zone.w / 2.0;
    let cy = zone.y + zone.h / 2.0;
    let mut dx = (lx - 0.5) * zone.w;
    let mut dy = (ly - 0.5) * zone.h;
    if zone.rotation.abs() > 1e-6 {
        let rad = zone.rotation.to_radians();
        let (s, c) = rad.sin_cos();
        (dx, dy) = (dx * c - dy * s, dx * s + dy * c);
    }
    let nx = (cx + dx) as f64;
    let ny = (cy + dy) as f64;
    (nx * cw * scale + ox, ny * ch * scale + oy)
}
