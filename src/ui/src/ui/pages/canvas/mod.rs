mod zone_overlay;
mod header_bar;

use zone_overlay::{DragMode, ZoneDrag, canvas_dims, led_canvas_screen};
use header_bar::FrameStats;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use base64::Engine as _;
use gdk_pixbuf::Pixbuf;
use gtk4 as gtk;
use libadwaita as adw;
use std::collections::HashMap;

use halod_protocol::types::{
    Animation, CanvasFrame, DeviceCapability, EffectParamValue, ParamKind, RgbColor, VisibilityState, WireDevice,
    WirePlacedZone,
};
use serde_json::json;

use crate::store::Store;
use crate::state::AppState;

/// Default normalized size of a newly added zone square.
const DEFAULT_ZONE_SIZE: f64 = 0.15;

// ── CanvasPage ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct CanvasPage {
    pub root: gtk::Overlay,
    /// Widgets to place in the header bar when the canvas page is active.
    pub header_box: gtk::Box,
    drawing_area: gtk::DrawingArea,
    effect_dropdown: gtk::DropDown,
    param_box: gtk::Box,
    stats_label: gtk::Label,
    // Shared state — mutated by update(), read by signal handlers.
    canvas_bytes: Rc<RefCell<Option<(Vec<u8>, u32, u32)>>>,
    placed_zones: Rc<RefCell<Vec<WirePlacedZone>>>,
    available_effects: Rc<RefCell<Vec<Animation>>>,
    /// The device list used by the Add Zone dialog — updated in update().
    devices: Rc<RefCell<Vec<WireDevice>>>,
    drag_zone: Rc<RefCell<Option<ZoneDrag>>>,
    selected_zone: Rc<RefCell<Option<(String, String)>>>,
    frame_stats: Rc<RefCell<FrameStats>>,
    /// LED colors from the last received canvas frame, keyed by (device_id, zone_id, led_id).
    led_colors_map: Rc<RefCell<HashMap<(String, String, u32), RgbColor>>>,
    /// Sampling radius in pixmap pixels, mirrored from daemon config.
    sample_radius: Rc<RefCell<f32>>,
    /// Prevents the dropdown's notify from firing while we set it programmatically.
    updating_dropdown: Rc<RefCell<bool>>,
    /// Tracks the last applied effect id so param controls are only rebuilt on change.
    last_active_effect_id: Rc<RefCell<Option<String>>>,
    /// Set of (device_id, zone_id) currently placed — used to detect add/remove without
    /// overwriting local positions that the user may have dragged.
    known_zone_keys: Rc<RefCell<std::collections::HashSet<(String, String)>>>,
}

impl CanvasPage {
    pub fn new(store: &Store) -> Self {
        // ── Canvas ────────────────────────────────────────────────────────────

        let drawing_area = gtk::DrawingArea::builder()
            .vexpand(true)
            .hexpand(true)
            .focusable(true)
            .build();

        // ── Header bar widgets (effect + add zone) ────────────────────────────

        let effect_dropdown = gtk::DropDown::builder()
            .model(&gtk::StringList::new(&[]))
            .build();

        let add_zone_btn = gtk::Button::builder()
            .label("+ Zone")
            .css_classes(["suggested-action"])
            .build();

        let header_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        header_box.append(&effect_dropdown);
        header_box.append(&add_zone_btn);

        // ── Floating toolbar controls ─────────────────────────────────────────

        let param_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .build();

        let radius_label = gtk::Label::builder()
            .label("Sampling Radius")
            .css_classes(["dim-label"])
            .build();

        let radius_adj = gtk::Adjustment::new(3.0, 0.5, 32.0, 0.5, 2.0, 0.0);
        let radius_spin = gtk::SpinButton::builder()
            .adjustment(&radius_adj)
            .digits(1)
            .build();

        let stats_label = gtk::Label::builder()
            .label("— fps")
            .css_classes(["dim-label"])
            .xalign(1.0)
            .width_request(52)
            .build();

        // ── Floating bar ──────────────────────────────────────────────────────

        let make_sep = || {
            let s = gtk::Separator::new(gtk::Orientation::Vertical);
            s.set_margin_top(4);
            s.set_margin_bottom(4);
            s
        };

        let floating_bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .margin_top(10)
            .css_classes(["canvas-floating-bar"])
            .build();

        floating_bar.append(&param_box);
        floating_bar.append(&make_sep());
        floating_bar.append(&radius_label);
        floating_bar.append(&radius_spin);
        floating_bar.append(&make_sep());
        floating_bar.append(&stats_label);

        // ── Overlay root ──────────────────────────────────────────────────────

        let root = gtk::Overlay::new();
        root.set_child(Some(&drawing_area));
        root.add_overlay(&floating_bar);

        // ── Shared state ──────────────────────────────────────────────────────

        let canvas_bytes: Rc<RefCell<Option<(Vec<u8>, u32, u32)>>> = Rc::new(RefCell::new(None));
        let placed_zones: Rc<RefCell<Vec<WirePlacedZone>>> = Rc::new(RefCell::new(Vec::new()));
        let available_effects: Rc<RefCell<Vec<Animation>>> = Rc::new(RefCell::new(Vec::new()));
        let devices: Rc<RefCell<Vec<WireDevice>>> = Rc::new(RefCell::new(Vec::new()));
        let drag_zone: Rc<RefCell<Option<ZoneDrag>>> = Rc::new(RefCell::new(None));
        let selected_zone: Rc<RefCell<Option<(String, String)>>> = Rc::new(RefCell::new(None));
        let frame_stats = Rc::new(RefCell::new(FrameStats::new()));
        let updating_dropdown = Rc::new(RefCell::new(false));
        let last_active_effect_id: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let led_colors_map: Rc<RefCell<HashMap<(String, String, u32), RgbColor>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let sample_radius: Rc<RefCell<f32>> = Rc::new(RefCell::new(3.0));
        let known_zone_keys: Rc<RefCell<std::collections::HashSet<(String, String)>>> =
            Rc::new(RefCell::new(std::collections::HashSet::new()));

        let page = Self {
            root,
            header_box,
            drawing_area,
            effect_dropdown,
            param_box,
            stats_label,
            canvas_bytes,
            placed_zones,
            available_effects,
            devices,
            drag_zone,
            selected_zone,
            frame_stats,
            updating_dropdown,
            last_active_effect_id,
            led_colors_map,
            sample_radius,
            known_zone_keys,
        };

        // Wire all signals ONCE here — they close over the Rc<RefCell<...>> refs.
        page.wire_draw_func();
        page.wire_drag_gesture(store.clone());
        page.wire_effect_dropdown(store.clone());
        page.wire_add_zone_button(store.clone(), add_zone_btn);
        page.wire_key_events(store.clone());
        page.wire_radius_scale(store.clone(), radius_spin);
        page
    }

    fn wire_radius_scale(&self, store: Store, spin: gtk::SpinButton) {
        let sample_radius = self.sample_radius.clone();
        let da = self.drawing_area.clone();
        spin.connect_value_changed(move |s| {
            let r = s.value() as f32;
            *sample_radius.borrow_mut() = r;
            store.dispatch(crate::commands::Command::CanvasOp(json!({"type": "canvas_set_sample_radius", "radius": r})));
            da.queue_draw();
        });
    }

    // ── Drawing ───────────────────────────────────────────────────────────────

    fn wire_draw_func(&self) {
        let canvas_bytes = self.canvas_bytes.clone();
        let placed_zones = self.placed_zones.clone();
        let selected_zone = self.selected_zone.clone();
        let devices = self.devices.clone();
        let led_colors_map = self.led_colors_map.clone();
        let sample_radius = self.sample_radius.clone();

        self.drawing_area.set_draw_func(move |_area, cr, w, h| {
            cr.set_source_rgb(0.08, 0.08, 0.08);
            let _ = cr.paint();

            let (cw, ch, scale, ox, oy) = if let Some((_, cw, ch)) = *canvas_bytes.borrow() {
                let scale = (w as f64 / cw as f64).min(h as f64 / ch as f64);
                let ox = (w as f64 - cw as f64 * scale) / 2.0;
                let oy = (h as f64 - ch as f64 * scale) / 2.0;
                (cw as f64, ch as f64, scale, ox, oy)
            } else {
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.25);
                cr.move_to(w as f64 / 2.0 - 60.0, h as f64 / 2.0);
                let _ = cr.show_text("Waiting for canvas frames…");
                return;
            };

            // Draw canvas pixmap.
            if let Some((ref bytes, bcw, bch)) = *canvas_bytes.borrow() {
                let pixbuf = Pixbuf::from_bytes(
                    &glib::Bytes::from(bytes.as_slice()),
                    gdk_pixbuf::Colorspace::Rgb,
                    true,
                    8,
                    bcw as i32,
                    bch as i32,
                    bcw as i32 * 4,
                );
                let dw = (bcw as f64 * scale) as i32;
                let dh = (bch as f64 * scale) as i32;
                if let Some(scaled) =
                    pixbuf.scale_simple(dw.max(1), dh.max(1), gdk_pixbuf::InterpType::Nearest)
                {
                    cr.set_source_pixbuf(&scaled, ox, oy);
                    let _ = cr.paint();
                }
            }

            let sel = selected_zone.borrow();
            let devs = devices.borrow();
            let colors = led_colors_map.borrow();
            let sr = *sample_radius.borrow() as f64 * scale;

            for zone in placed_zones.borrow().iter() {
                let is_selected = sel
                    .as_ref()
                    .map_or(false, |(d, z)| d == &zone.device_id && z == &zone.zone_id);

                // Zone center in screen coords.
                let cx_s = (zone.x as f64 + zone.w as f64 / 2.0) * cw * scale + ox;
                let cy_s = (zone.y as f64 + zone.h as f64 / 2.0) * ch * scale + oy;
                let sw = zone.w as f64 * cw * scale;
                let sh = zone.h as f64 * ch * scale;
                let angle = zone.rotation.to_radians() as f64;

                // Draw box with Cairo rotation applied (translate→rotate→draw→restore).
                let _ = cr.save();
                cr.translate(cx_s, cy_s);
                cr.rotate(angle);

                // Semi-transparent fill.
                cr.set_source_rgba(0.2, 0.5, 1.0, 0.15);
                cr.rectangle(-sw / 2.0, -sh / 2.0, sw, sh);
                let _ = cr.fill();

                // Border: dim unselected, bright yellow when selected.
                if is_selected {
                    cr.set_source_rgba(1.0, 1.0, 0.0, 0.9);
                    cr.set_line_width(2.0);
                } else {
                    cr.set_source_rgba(0.4, 0.6, 1.0, 0.6);
                    cr.set_line_width(1.0);
                }
                cr.rectangle(-sw / 2.0, -sh / 2.0, sw, sh);
                let _ = cr.stroke();

                // Zone label (top-left corner in rotated space): device name
                // above, zone id below.
                let device_name = devs
                    .iter()
                    .find(|d| d.id == zone.device_id)
                    .map_or(zone.device_id.as_str(), |d| d.name.as_str());
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.85);
                cr.move_to(-sw / 2.0 + 3.0, -sh / 2.0 + 13.0);
                let _ = cr.show_text(device_name);
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.55);
                cr.move_to(-sw / 2.0 + 3.0, -sh / 2.0 + 25.0);
                let _ = cr.show_text(&zone.zone_id);

                // Resize corner handles and rotation handle (selected zone only).
                if is_selected {
                    let handle = 5.0_f64;
                    cr.set_source_rgba(1.0, 1.0, 0.0, 0.9);
                    for (dx, dy) in [
                        (-sw / 2.0, -sh / 2.0),
                        (sw / 2.0, -sh / 2.0),
                        (-sw / 2.0, sh / 2.0),
                        (sw / 2.0, sh / 2.0),
                    ] {
                        cr.rectangle(dx - handle, dy - handle, handle * 2.0, handle * 2.0);
                        let _ = cr.fill();
                    }

                    // Rotation handle: circle above top-center connected by a line.
                    let rh_y = -sh / 2.0 - 25.0;
                    cr.set_source_rgba(0.0, 1.0, 0.8, 0.9);
                    cr.set_line_width(1.5);
                    cr.move_to(0.0, -sh / 2.0);
                    cr.line_to(0.0, rh_y);
                    let _ = cr.stroke();
                    cr.arc(0.0, rh_y, 6.0, 0.0, std::f64::consts::TAU);
                    let _ = cr.fill();
                }

                let _ = cr.restore();

                // Draw LED dots and sampling-radius squares (led_canvas_screen already applies rotation).
                if let Some(device) = devs.iter().find(|d| d.id == zone.device_id) {
                    for cap in &device.capabilities {
                        if let DeviceCapability::Rgb(rgb) = cap {
                            if let Some(rgb_zone) =
                                rgb.descriptor.zones.iter().find(|z| z.id == zone.zone_id)
                            {
                                for led in &rgb_zone.leds {
                                    let (lx, ly) =
                                        led_canvas_screen(led.x, led.y, zone, cw, ch, scale, ox, oy);

                                    // Sampling radius square.
                                    cr.set_source_rgba(1.0, 1.0, 1.0, 0.18);
                                    cr.set_line_width(1.0);
                                    cr.rectangle(lx - sr, ly - sr, sr * 2.0, sr * 2.0);
                                    let _ = cr.stroke();

                                    // LED dot.
                                    let key = (zone.device_id.clone(), zone.zone_id.clone(), led.id);
                                    if let Some(c) = colors.get(&key) {
                                        cr.set_source_rgb(
                                            c.r as f64 / 255.0,
                                            c.g as f64 / 255.0,
                                            c.b as f64 / 255.0,
                                        );
                                    } else {
                                        cr.set_source_rgba(0.8, 0.8, 0.8, 0.5);
                                    }
                                    cr.arc(lx, ly, 3.0, 0.0, std::f64::consts::TAU);
                                    let _ = cr.fill();
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // ── Drag ─────────────────────────────────────────────────────────────────

    fn wire_drag_gesture(&self, store: Store) {
        let gesture = gtk::GestureDrag::new();

        let placed_zones = self.placed_zones.clone();
        let canvas_bytes = self.canvas_bytes.clone();
        let drag_zone = self.drag_zone.clone();
        let selected_zone = self.selected_zone.clone();
        let da = self.drawing_area.clone();

        gesture.connect_drag_begin(move |_, press_x, press_y| {
            let (scale, ox, oy, cw, ch) = canvas_dims(&da, &canvas_bytes);

            let zones = placed_zones.borrow();
            let sel = selected_zone.borrow().clone();
            const HANDLE_R: f64 = 10.0;
            let dist = |ax: f64, ay: f64, bx: f64, by: f64| {
                ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt()
            };

            let mut hit: Option<ZoneDrag> = None;

            // Helper: rotate a local offset around zone center into screen coords.
            let rot_pt = |cx_s: f64, cy_s: f64, dx: f64, dy: f64, angle: f64| -> (f64, f64) {
                let (s, c) = angle.sin_cos();
                (cx_s + dx * c - dy * s, cy_s + dx * s + dy * c)
            };
            // Helper: transform a screen point into zone-local coords (unrotate).
            let to_local = |cx_s: f64, cy_s: f64, px: f64, py: f64, angle: f64| -> (f64, f64) {
                let rel_x = px - cx_s;
                let rel_y = py - cy_s;
                let (s, c) = (-angle).sin_cos();
                (rel_x * c - rel_y * s, rel_x * s + rel_y * c)
            };

            // Check handles on the currently selected zone first.
            if let Some((did, zid)) = &sel {
                if let Some(z) = zones.iter().find(|z| &z.device_id == did && &z.zone_id == zid) {
                    let cx_s = (z.x as f64 + z.w as f64 / 2.0) * cw * scale + ox;
                    let cy_s = (z.y as f64 + z.h as f64 / 2.0) * ch * scale + oy;
                    let sw = z.w as f64 * cw * scale;
                    let sh = z.h as f64 * ch * scale;
                    let angle = z.rotation.to_radians() as f64;

                    // Rotation handle (above top-center in rotated space).
                    let (rh_x, rh_y) = rot_pt(cx_s, cy_s, 0.0, -sh / 2.0 - 25.0, angle);
                    if dist(press_x, press_y, rh_x, rh_y) <= HANDLE_R + 4.0 {
                        hit = Some(ZoneDrag {
                            device_id: did.clone(),
                            zone_id: zid.clone(),
                            press_x,
                            press_y,
                            mode: DragMode::Rotate {
                                zone_cx_screen: cx_s,
                                zone_cy_screen: cy_s,
                                orig_rotation: z.rotation,
                            },
                        });
                    }

                    // Corner resize handles (rotated corners).
                    if hit.is_none() {
                        let corner_offsets = [
                            (-sw / 2.0, -sh / 2.0, 0u8),
                            (sw / 2.0,  -sh / 2.0, 1),
                            (-sw / 2.0,  sh / 2.0, 2),
                            (sw / 2.0,   sh / 2.0, 3),
                        ];
                        for (dx, dy, corner) in corner_offsets {
                            let (cx, cy) = rot_pt(cx_s, cy_s, dx, dy, angle);
                            if dist(press_x, press_y, cx, cy) <= HANDLE_R {
                                hit = Some(ZoneDrag {
                                    device_id: did.clone(),
                                    zone_id: zid.clone(),
                                    press_x,
                                    press_y,
                                    mode: DragMode::Resize {
                                        corner,
                                        orig_x: z.x,
                                        orig_y: z.y,
                                        orig_w: z.w,
                                        orig_h: z.h,
                                        orig_rotation: z.rotation,
                                    },
                                });
                                break;
                            }
                        }
                    }
                }
            }

            // Otherwise check zone body using rotation-aware AABB (any zone, topmost first).
            if hit.is_none() {
                for z in zones.iter().rev() {
                    let cx_s = (z.x as f64 + z.w as f64 / 2.0) * cw * scale + ox;
                    let cy_s = (z.y as f64 + z.h as f64 / 2.0) * ch * scale + oy;
                    let sw = z.w as f64 * cw * scale;
                    let sh = z.h as f64 * ch * scale;
                    let angle = z.rotation.to_radians() as f64;
                    let (lx, ly) = to_local(cx_s, cy_s, press_x, press_y, angle);
                    if lx.abs() <= sw / 2.0 && ly.abs() <= sh / 2.0 {
                        hit = Some(ZoneDrag {
                            device_id: z.device_id.clone(),
                            zone_id: z.zone_id.clone(),
                            press_x,
                            press_y,
                            mode: DragMode::Move {
                                start_zone_x: z.x,
                                start_zone_y: z.y,
                            },
                        });
                        break;
                    }
                }
            }

            if let Some(drag) = hit {
                *selected_zone.borrow_mut() = Some((drag.device_id.clone(), drag.zone_id.clone()));
                *drag_zone.borrow_mut() = Some(drag);
                da.grab_focus();
                da.queue_draw();
            }
        });

        let placed_zones2 = self.placed_zones.clone();
        let canvas_bytes2 = self.canvas_bytes.clone();
        let drag_zone2 = self.drag_zone.clone();
        let da2 = self.drawing_area.clone();

        gesture.connect_drag_update(move |_, dx, dy| {
            let (scale, _, _, cw, ch) = canvas_dims(&da2, &canvas_bytes2);
            let drag = drag_zone2.borrow().clone();
            if let Some(drag) = drag {
                let mut zones = placed_zones2.borrow_mut();
                if let Some(z) =
                    zones.iter_mut().find(|z| z.device_id == drag.device_id && z.zone_id == drag.zone_id)
                {
                    match &drag.mode {
                        DragMode::Move { start_zone_x, start_zone_y } => {
                            z.x = start_zone_x + (dx / scale / cw) as f32;
                            z.y = start_zone_y + (dy / scale / ch) as f32;
                        }
                        DragMode::Resize { corner, orig_x, orig_y, orig_w, orig_h, orig_rotation } => {
                            // Resize in the zone's *local* (un-rotated) axes and keep the
                            // corner opposite the dragged one pinned in canvas space.
                            const MIN: f64 = 0.02;
                            let (s, c) = (*orig_rotation as f64).to_radians().sin_cos();

                            // Drag delta: screen pixels → normalized canvas → zone-local.
                            let dnx = dx / scale / cw;
                            let dny = dy / scale / ch;
                            let ldx = dnx * c + dny * s;
                            let ldy = -dnx * s + dny * c;

                            // Local sign of the dragged corner: TL/TR/BL/BR.
                            let (sx, sy) = match corner {
                                0 => (-1.0_f64, -1.0_f64),
                                1 => (1.0, -1.0),
                                2 => (-1.0, 1.0),
                                _ => (1.0, 1.0),
                            };
                            let (ow, oh) = (*orig_w as f64, *orig_h as f64);
                            let new_w = (ow + sx * ldx).max(MIN);
                            let new_h = (oh + sy * ldy).max(MIN);

                            // Anchor = opposite corner, pinned in canvas space.
                            let ocx = *orig_x as f64 + ow / 2.0;
                            let ocy = *orig_y as f64 + oh / 2.0;
                            let (alx, aly) = (-sx * ow / 2.0, -sy * oh / 2.0);
                            let anchor_x = ocx + alx * c - aly * s;
                            let anchor_y = ocy + alx * s + aly * c;

                            // New center so the anchor corner stays put after the resize.
                            let (nlx, nly) = (-sx * new_w / 2.0, -sy * new_h / 2.0);
                            let ncx = anchor_x - (nlx * c - nly * s);
                            let ncy = anchor_y - (nlx * s + nly * c);

                            z.w = new_w as f32;
                            z.h = new_h as f32;
                            z.x = (ncx - new_w / 2.0) as f32;
                            z.y = (ncy - new_h / 2.0) as f32;
                        }
                        DragMode::Rotate { zone_cx_screen, zone_cy_screen, orig_rotation } => {
                            let cursor_x = drag.press_x + dx;
                            let cursor_y = drag.press_y + dy;
                            let angle_now = (cursor_y - zone_cy_screen)
                                .atan2(cursor_x - zone_cx_screen)
                                .to_degrees() as f32;
                            let angle_start = (drag.press_y - zone_cy_screen)
                                .atan2(drag.press_x - zone_cx_screen)
                                .to_degrees() as f32;
                            let mut new_rot = orig_rotation + (angle_now - angle_start);
                            // Snap to multiples of 90° within ±5°.
                            let snapped = (new_rot / 90.0).round() * 90.0;
                            if (new_rot - snapped).abs() < 5.0 {
                                new_rot = snapped;
                            }
                            z.rotation = new_rot;
                        }
                    }
                }
                da2.queue_draw();
            }
        });

        let placed_zones3 = self.placed_zones.clone();
        let drag_zone3 = self.drag_zone.clone();

        gesture.connect_drag_end(move |_, _, _| {
            if let Some(drag) = drag_zone3.borrow_mut().take() {
                let zones = placed_zones3.borrow();
                if let Some(z) = zones.iter().find(|z| z.device_id == drag.device_id && z.zone_id == drag.zone_id) {
                    store.dispatch(crate::commands::Command::CanvasOp(json!({
                        "type": "canvas_move_zone",
                        "device_id": z.device_id,
                        "zone_id": z.zone_id,
                        "x": z.x,
                        "y": z.y,
                        "w": z.w,
                        "h": z.h,
                        "rotation": z.rotation,
                    })));
                }
            }
        });

        self.drawing_area.add_controller(gesture);
    }

    // ── Effect dropdown ───────────────────────────────────────────────────────

    fn wire_effect_dropdown(&self, store: Store) {
        let available_effects = self.available_effects.clone();
        let updating = self.updating_dropdown.clone();

        self.effect_dropdown.connect_selected_notify(move |dd| {
            if *updating.borrow() { return; }
            let idx = dd.selected() as usize;
            let effects = available_effects.borrow();
            if let Some(effect) = effects.get(idx) {
                let params: serde_json::Map<String, serde_json::Value> = effect
                    .params
                    .iter()
                    .map(|p| (p.id.clone(), serde_json::to_value(&p.default).unwrap()))
                    .collect();
                store.dispatch(crate::commands::Command::CanvasOp(json!({
                    "type": "canvas_set_effect",
                    "effect_id": effect.id,
                    "params": params,
                })));
            }
        });
    }

    // ── Add Zone button ───────────────────────────────────────────────────────

    fn wire_add_zone_button(&self, store: Store, btn: gtk::Button) {
        let devices = self.devices.clone();
        let placed_zones = self.placed_zones.clone();
        let root = self.root.clone();

        btn.connect_clicked(move |_| {
            let window = root.root().and_downcast::<gtk::Window>();

            let dialog = adw::AlertDialog::builder()
                .heading("Add Zones")
                .body("Select one or more device zones to place on the canvas.")
                .build();
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("add", "Add");
            dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("add"));

            let list = gtk::ListBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .css_classes(["boxed-list"])
                .build();

            let already_placed: std::collections::HashSet<(String, String)> = placed_zones
                .borrow()
                .iter()
                .map(|p| (p.device_id.clone(), p.zone_id.clone()))
                .collect();

            let mut rows: Vec<(String, String)> = Vec::new();
            // Lowercased "device zone" haystacks, parallel to `rows`, used by the filter.
            let mut haystacks: Vec<String> = Vec::new();
            // Per-row checkboxes, parallel to `rows`; clicking anywhere on a row toggles them.
            let mut checks: Vec<gtk::CheckButton> = Vec::new();
            for device in devices.borrow().iter() {
                if device.active_state != VisibilityState::Visible {
                    continue;
                }
                for cap in &device.capabilities {
                    if let DeviceCapability::Rgb(rgb) = cap {
                        for zone in &rgb.descriptor.zones {
                            if already_placed.contains(&(device.id.clone(), zone.id.clone())) {
                                continue;
                            }
                            let check = gtk::CheckButton::builder()
                                .valign(gtk::Align::Center)
                                .css_classes(["zone-check"])
                                .build();
                            let row = adw::ActionRow::builder()
                                .title(&device.name)
                                .subtitle(&zone.name)
                                .activatable(true)
                                .build();
                            row.add_prefix(&check);
                            row.set_activatable_widget(Some(&check));
                            list.append(&row);
                            haystacks.push(
                                format!("{} {}", device.name, zone.name).to_lowercase(),
                            );
                            checks.push(check);
                            rows.push((device.id.clone(), zone.id.clone()));
                        }
                    }
                }
            }

            // Placeholder shown when the list is empty or every row is filtered out.
            let placeholder = gtk::Label::builder()
                .label("No matching zones")
                .css_classes(["dim-label"])
                .margin_top(18)
                .margin_bottom(18)
                .build();
            list.set_placeholder(Some(&placeholder));

            // Search entry filters the list live by device/zone name.
            let search = gtk::SearchEntry::builder()
                .placeholder_text("Search devices and zones…")
                .build();
            let query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
            let haystacks = Rc::new(haystacks);

            {
                let query = query.clone();
                let haystacks = haystacks.clone();
                list.set_filter_func(move |row| {
                    let q = query.borrow();
                    if q.is_empty() {
                        return true;
                    }
                    haystacks
                        .get(row.index() as usize)
                        .map_or(true, |h| h.contains(q.as_str()))
                });
            }
            {
                let query = query.clone();
                let list = list.clone();
                search.connect_search_changed(move |e| {
                    *query.borrow_mut() = e.text().to_lowercase();
                    list.invalidate_filter();
                });
            }

            let scroll = gtk::ScrolledWindow::builder()
                .child(&list)
                .min_content_height(360)
                .min_content_width(380)
                .propagate_natural_height(true)
                .build();

            let content = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(8)
                .build();
            content.append(&search);
            content.append(&scroll);
            dialog.set_extra_child(Some(&content));

            let rows = Rc::new(rows);
            let checks = Rc::new(checks);
            let store2 = store.clone();

            dialog.connect_response(None, move |_, resp| {
                if resp == "add" {
                    // Cascade placement so multiple new zones don't fully overlap.
                    let mut i = 0usize;
                    for (idx, check) in checks.iter().enumerate() {
                        if !check.is_active() {
                            continue;
                        }
                        if let Some((device_id, zone_id)) = rows.get(idx) {
                            let pos = ((i as f64) * 0.05 % 0.5) as f32;
                            i += 1;
                            store2.dispatch(crate::commands::Command::CanvasOp(json!({
                                "type": "canvas_place_zone",
                                "device_id": device_id,
                                "zone_id": zone_id,
                                "x": pos,
                                "y": pos,
                                "w": DEFAULT_ZONE_SIZE,
                                "h": DEFAULT_ZONE_SIZE,
                                "rotation": 0.0,
                            })));
                        }
                    }
                }
            });

            if let Some(w) = window {
                dialog.present(Some(&w));
                search.grab_focus();
            }
        });
    }

    // ── Key events ────────────────────────────────────────────────────────────

    fn wire_key_events(&self, store: Store) {
        let selected_zone = self.selected_zone.clone();
        let da = self.drawing_area.clone();

        let key_ctl = gtk::EventControllerKey::new();
        key_ctl.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Delete || key == gtk::gdk::Key::BackSpace {
                if let Some((did, zid)) = selected_zone.borrow_mut().take() {
                    store.dispatch(crate::commands::Command::CanvasOp(json!({
                        "type": "canvas_remove_zone",
                        "device_id": did,
                        "zone_id": zid,
                    })));
                    da.queue_draw();
                }
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
        self.drawing_area.add_controller(key_ctl);
    }

    // ── Param controls ────────────────────────────────────────────────────────

    fn rebuild_param_controls(&self, effect: &Animation, store: &Store) {
        while let Some(child) = self.param_box.first_child() {
            self.param_box.remove(&child);
        }

        for param in &effect.params {
            let label = gtk::Label::builder()
                .label(&param.label)
                .css_classes(["dim-label"])
                .build();
            self.param_box.append(&label);

            match &param.kind {
                ParamKind::Color => {
                    let default_color = match &param.default {
                        EffectParamValue::Color(c) => gdk4::RGBA::new(
                            c.r as f32 / 255.0,
                            c.g as f32 / 255.0,
                            c.b as f32 / 255.0,
                            1.0,
                        ),
                        _ => gdk4::RGBA::new(1.0, 0.0, 0.0, 1.0),
                    };
                    let color_dialog = gtk::ColorDialog::builder()
                        .with_alpha(false)
                        .build();
                    let btn = gtk::ColorDialogButton::builder()
                        .dialog(&color_dialog)
                        .rgba(&default_color)
                        .build();
                    let store_c = store.clone();
                    let effects = self.available_effects.clone();
                    let dd = self.effect_dropdown.clone();
                    let param_id = param.id.clone();
                    btn.connect_rgba_notify(move |b| {
                        let rgba = b.rgba();
                        let idx = dd.selected() as usize;
                        let effects = effects.borrow();
                        if let Some(effect) = effects.get(idx) {
                            let mut params: serde_json::Map<String, serde_json::Value> = effect
                                .params
                                .iter()
                                .map(|p| {
                                    (p.id.clone(), serde_json::to_value(&p.default).unwrap())
                                })
                                .collect();
                            params.insert(
                                param_id.clone(),
                                json!({
                                    "r": (rgba.red()   * 255.0) as u8,
                                    "g": (rgba.green() * 255.0) as u8,
                                    "b": (rgba.blue()  * 255.0) as u8,
                                }),
                            );
                            store_c.dispatch(crate::commands::Command::CanvasOp(json!({
                                "type": "canvas_set_effect",
                                "effect_id": effect.id,
                                "params": params,
                            })));
                        }
                    });
                    self.param_box.append(&btn);
                }
                ParamKind::Range { min, max, step } => {
                    let default_val = match &param.default {
                        EffectParamValue::Float(f) => *f,
                        _ => *min,
                    };
                    let digits = if *step < 1.0 { 2u32 } else { 0u32 };
                    let adj = gtk::Adjustment::new(default_val, *min, *max, *step, *step * 10.0, 0.0);
                    let spin = gtk::SpinButton::builder()
                        .adjustment(&adj)
                        .digits(digits)
                        .build();
                    let store_c = store.clone();
                    let effects = self.available_effects.clone();
                    let dd = self.effect_dropdown.clone();
                    let param_id = param.id.clone();
                    spin.connect_value_changed(move |s| {
                        let idx = dd.selected() as usize;
                        let effects = effects.borrow();
                        if let Some(effect) = effects.get(idx) {
                            let mut params: serde_json::Map<String, serde_json::Value> = effect
                                .params
                                .iter()
                                .map(|p| {
                                    (p.id.clone(), serde_json::to_value(&p.default).unwrap())
                                })
                                .collect();
                            params.insert(param_id.clone(), json!(s.value()));
                            store_c.dispatch(crate::commands::Command::CanvasOp(json!({
                                "type": "canvas_set_effect",
                                "effect_id": effect.id,
                                "params": params,
                            })));
                        }
                    });
                    self.param_box.append(&spin);
                }
                ParamKind::Enum { options } => {
                    let default_str = match &param.default {
                        EffectParamValue::Str(s) => s.clone(),
                        _ => String::new(),
                    };
                    let opts: Vec<&str> = options.iter().map(|s| s.as_str()).collect();
                    let model = gtk::StringList::new(&opts);
                    let dd = gtk::DropDown::builder().model(&model).build();
                    let init = options.iter().position(|o| *o == default_str).unwrap_or(0);
                    dd.set_selected(init as u32);
                    let store_c = store.clone();
                    let effects = self.available_effects.clone();
                    let effect_dd = self.effect_dropdown.clone();
                    let param_id = param.id.clone();
                    let options = options.clone();
                    dd.connect_selected_notify(move |d| {
                        let idx = effect_dd.selected() as usize;
                        let effects = effects.borrow();
                        if let Some(effect) = effects.get(idx) {
                            let mut params: serde_json::Map<String, serde_json::Value> = effect
                                .params
                                .iter()
                                .map(|p| {
                                    (p.id.clone(), serde_json::to_value(&p.default).unwrap())
                                })
                                .collect();
                            if let Some(opt) = options.get(d.selected() as usize) {
                                params.insert(param_id.clone(), json!(opt));
                            }
                            store_c.dispatch(crate::commands::Command::CanvasOp(json!({
                                "type": "canvas_set_effect",
                                "effect_id": effect.id,
                                "params": params,
                            })));
                        }
                    });
                    self.param_box.append(&dd);
                }
                _ => {}
            }
        }
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Called by MainWindow on each state broadcast.
    /// Only mutates shared Rc<RefCell<...>> data — no signal connections here.
    pub fn update(&self, state: &AppState, store: &Store) {
        // Sync sampling radius from daemon config.
        *self.sample_radius.borrow_mut() = state.canvas.sample_radius;

        // Update device list for the Add Zone dialog.
        *self.devices.borrow_mut() = state.devices.clone();

        // Sync placed zones only when the set of zones changes (add/remove).
        // Do NOT overwrite positions on every broadcast — the user may have dragged zones.
        let incoming_keys: std::collections::HashSet<(String, String)> = state
            .canvas
            .placed_zones
            .iter()
            .map(|z| (z.device_id.clone(), z.zone_id.clone()))
            .collect();
        let zone_set_changed = *self.known_zone_keys.borrow() != incoming_keys;
        if zone_set_changed {
            *self.known_zone_keys.borrow_mut() = incoming_keys.clone();
            // Add new zones from daemon state; remove zones no longer present.
            let mut zones = self.placed_zones.borrow_mut();
            zones.retain(|z| incoming_keys.contains(&(z.device_id.clone(), z.zone_id.clone())));
            for incoming in &state.canvas.placed_zones {
                let key = (incoming.device_id.clone(), incoming.zone_id.clone());
                if !zones.iter().any(|z| z.device_id == incoming.device_id && z.zone_id == incoming.zone_id) {
                    zones.push(incoming.clone());
                }
                let _ = key;
            }
        }

        // Sync effect dropdown model (only rebuild if the effect list changed).
        let new_effects = &state.canvas.available_effects;
        let model_needs_rebuild = {
            let cur = self.available_effects.borrow();
            cur.len() != new_effects.len()
                || cur.iter().zip(new_effects.iter()).any(|(a, b)| a.id != b.id)
        };
        if model_needs_rebuild {
            *self.updating_dropdown.borrow_mut() = true;
            let model = gtk::StringList::new(&[]);
            for e in new_effects {
                model.append(&e.name);
            }
            self.effect_dropdown.set_model(Some(&model));
            *self.updating_dropdown.borrow_mut() = false;
            *self.available_effects.borrow_mut() = new_effects.clone();
        }

        // Select the active effect in the dropdown.
        let active_id = state.canvas.active_effect_id.as_deref();
        let last_id = self.last_active_effect_id.borrow().clone();
        if active_id.map(|s| s.to_string()) != last_id {
            *self.last_active_effect_id.borrow_mut() = active_id.map(|s| s.to_string());

            if let Some(active_id) = active_id {
                let idx = new_effects.iter().position(|e| e.id == active_id);
                if let Some(idx) = idx {
                    *self.updating_dropdown.borrow_mut() = true;
                    self.effect_dropdown.set_selected(idx as u32);
                    *self.updating_dropdown.borrow_mut() = false;
                    if let Some(effect) = new_effects.get(idx) {
                        self.rebuild_param_controls(effect, store);
                    }
                }
            }
        }

        self.drawing_area.queue_draw();
    }

    pub fn on_profile_switch(&self, state: &AppState, store: &Store) {
        *self.known_zone_keys.borrow_mut() = std::collections::HashSet::new();
        *self.last_active_effect_id.borrow_mut() = None;
        *self.available_effects.borrow_mut() = vec![];
        self.update(state, store);
    }

    /// Clear the current canvas pixmap so "Waiting for canvas frames…" reappears after disconnect.
    pub fn reset_frame(&self) {
        *self.canvas_bytes.borrow_mut() = None;
        self.drawing_area.queue_draw();
    }

    /// Called by main.rs when a canvas frame arrives from the dedicated frame channel.
    pub fn on_canvas_frame(&self, frame: Arc<CanvasFrame>) {
        let Ok(bytes) =
            base64::engine::general_purpose::STANDARD.decode(&frame.canvas_srgb_b64)
        else {
            return;
        };

        *self.canvas_bytes.borrow_mut() = Some((bytes, frame.canvas_w, frame.canvas_h));

        // Populate LED color map from frame data.
        let mut colors = self.led_colors_map.borrow_mut();
        colors.clear();
        for entry in &frame.led_colors {
            colors.insert(
                (entry.device_id.clone(), entry.zone_id.clone(), entry.led_id),
                entry.color,
            );
        }
        drop(colors);

        let mut stats = self.frame_stats.borrow_mut();
        stats.push(&frame);
        let fps = stats.fps();
        let _dropped = stats.total_dropped;
        drop(stats);

        self.stats_label.set_text(&format!("{fps:.0} fps"));

        self.drawing_area.queue_draw();
    }
}
