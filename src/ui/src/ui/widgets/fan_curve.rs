use std::cell::RefCell;
use std::f64::consts::PI;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::store::Store;
use halod_protocol::types::{FanCurveStatus, FanStatus, Sensor, WireFanCurve, WirePresetCurve};

const T_MIN: f64 = 20.0;
const T_MAX: f64 = 100.0;
const D_MAX: f64 = 100.0;
const PAD_L: f64 = 44.0;
const PAD_T: f64 = 16.0;
const PAD_R: f64 = 16.0;
const PAD_B: f64 = 36.0;
const PT_RADIUS: f64 = 7.0;

fn tx(temp: f64, w: f64) -> f64 {
    PAD_L + (temp - T_MIN) / (T_MAX - T_MIN) * (w - PAD_L - PAD_R)
}
fn ty(duty: f64, h: f64) -> f64 {
    PAD_T + (1.0 - duty / D_MAX) * (h - PAD_T - PAD_B)
}
fn from_x(x: f64, w: f64) -> f64 {
    (T_MIN + (x - PAD_L) / (w - PAD_L - PAD_R) * (T_MAX - T_MIN)).clamp(T_MIN, T_MAX)
}
fn from_y(y: f64, h: f64) -> f64 {
    ((1.0 - (y - PAD_T) / (h - PAD_T - PAD_B)) * D_MAX).clamp(0.0, D_MAX)
}

/// Linearly interpolates duty (%) for a given temperature across a sorted
/// list of `[temp, duty]` control points.  Clamps to the first/last duty
/// outside the defined temperature range.
pub(crate) fn duty_at_temp(points: &[[f64; 2]], temp: f64) -> f64 {
    if points.is_empty() { return 0.0; }
    if temp <= points[0][0] { return points[0][1]; }
    let last = points[points.len() - 1];
    if temp >= last[0] { return last[1]; }
    for i in 0..points.len() - 1 {
        let a = points[i];
        let b = points[i + 1];
        if temp >= a[0] && temp <= b[0] {
            let t = (temp - a[0]) / (b[0] - a[0]);
            return a[1] + t * (b[1] - a[1]);
        }
    }
    last[1]
}

#[derive(Clone)]
struct CurveState {
    points: Vec<[f64; 2]>,
    dragging: Option<usize>,
    current_temp: Option<f64>,
    /// Set when a preset button was the last action; cleared when the user edits points.
    active_preset: Option<String>,
}

impl CurveState {
    fn new(curve: Option<&WireFanCurve>) -> Self {
        let mut points: Vec<[f64; 2]> = curve
            .map(|c| c.points.iter().map(|p| [p[0] as f64, p[1] as f64]).collect())
            .unwrap_or_else(|| {
                vec![
                    [20.0, 25.0],
                    [40.0, 30.0],
                    [55.0, 50.0],
                    [70.0, 80.0],
                    [80.0, 100.0],
                ]
            });
        points.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap());
        CurveState { points, dragging: None, current_temp: None, active_preset: None }
    }

    fn hit_test(&self, mx: f64, my: f64, w: f64, h: f64) -> Option<usize> {
        self.points.iter().enumerate().find_map(|(i, pt)| {
            let d = ((mx - tx(pt[0], w)).powi(2) + (my - ty(pt[1], h)).powi(2)).sqrt();
            if d <= PT_RADIUS + 5.0 { Some(i) } else { None }
        })
    }

    fn in_graph(x: f64, y: f64, w: f64, h: f64) -> bool {
        x >= PAD_L && x <= w - PAD_R && y >= PAD_T && y <= h - PAD_B
    }

    fn duty_at_temp(&self, temp: f64) -> f64 {
        duty_at_temp(&self.points, temp)
    }
}

pub struct FanCurveWidget {
    pub root: gtk::Box,
    curve_state: Rc<RefCell<CurveState>>,
    drawing_area: gtk::DrawingArea,
    rpm_label: gtk::Label,
    duty_label: gtk::Label,
    temp_label: gtk::Label,
    cur_duty_label: gtk::Label,
    warning_revealer: gtk::Revealer,
    warning_label: gtk::Label,
    /// Sensor ID currently selected by the user.
    selected_sensor_id: Rc<RefCell<Option<String>>>,
}

impl FanCurveWidget {
    /// Build the complete editor widget.
    ///
    /// `sensors` is a list of `(device_name, Sensor)` pairs from all devices.
    pub fn build(
        fan_id: &str,
        fan_status: &FanStatus,
        curve: Option<&WireFanCurve>,
        sensors: &[(String, Sensor)],
        preset_curves: &[WirePresetCurve],
        store: &Store,
    ) -> Self {
        let initial_sensor_id: Option<String> = curve.and_then(|c| c.sensor_id.clone());
        let initial_temp = initial_sensor_id.as_ref().and_then(|sid| {
            sensors.iter().find(|(_, s)| &s.id == sid).map(|(_, s)| s.value)
        });

        let curve_state = Rc::new(RefCell::new({
            let mut cs = CurveState::new(curve);
            cs.current_temp = initial_temp;
            cs
        }));

        let selected_sensor_id: Rc<RefCell<Option<String>>> =
            Rc::new(RefCell::new(initial_sensor_id.clone()));

        let committed_points: Rc<RefCell<Vec<[f64; 2]>>> = Rc::new(RefCell::new({
            let mut pts: Vec<[f64; 2]> = curve
                .map(|c| c.points.iter().map(|p| [p[0] as f64, p[1] as f64]).collect())
                .unwrap_or_else(|| {
                    vec![[20.0, 25.0], [40.0, 30.0], [55.0, 50.0], [70.0, 80.0], [80.0, 100.0]]
                });
            pts.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap());
            pts
        }));
        let committed_sensor_id: Rc<RefCell<Option<String>>> =
            Rc::new(RefCell::new(initial_sensor_id.clone()));

        let apply_btn = gtk::Button::builder()
            .label("Apply Curve")
            .css_classes(["suggested-action"])
            .sensitive(false)
            .build();

        let refresh_apply: Rc<dyn Fn()> = {
            let cs = curve_state.clone();
            let cp = committed_points.clone();
            let csi = committed_sensor_id.clone();
            let sel = selected_sensor_id.clone();
            let btn = apply_btn.clone();
            Rc::new(move || {
                let has_changes = cs.borrow().points != *cp.borrow()
                    || *sel.borrow() != *csi.borrow();
                btn.set_sensitive(has_changes);
            })
        };

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();

        // The card wraps only the editor controls; stats chips live above it.
        let editor_inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_start(12)
            .margin_end(12)
            .margin_top(8)
            .margin_bottom(10)
            .build();

        // ── Warning bar ────────────────────────────────────────────────────────
        let (initial_warning_text, initial_warning_visible) = curve
            .map(|c| match &c.status {
                FanCurveStatus::NoSensor =>
                    ("No temperature sensor assigned — fan held at 75%", true),
                FanCurveStatus::SensorMalfunction =>
                    ("Sensor malfunction detected — fan held at 75%", true),
                FanCurveStatus::WriteError(e) if e.contains("Permission denied") =>
                    ("Permission denied writing PWM — run: sudo udevadm trigger --subsystem-match=hwmon", true),
                FanCurveStatus::WriteError(_) =>
                    ("Failed to set fan speed — check daemon logs", true),
                FanCurveStatus::FanStalled =>
                    ("Fan is not spinning (0 RPM) — check connections or replace the fan", true),
                FanCurveStatus::Ok => ("", false),
            })
            .unwrap_or(("", false));

        let warning_bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["fan-warning-bar"])
            .build();
        let warning_icon = gtk::Image::builder()
            .icon_name("dialog-warning-symbolic")
            .build();
        let warning_label = gtk::Label::builder()
            .label(initial_warning_text)
            .halign(gtk::Align::Start)
            .hexpand(true)
            .wrap(true)
            .build();
        warning_bar.append(&warning_icon);
        warning_bar.append(&warning_label);

        let warning_revealer = gtk::Revealer::builder()
            .child(&warning_bar)
            .reveal_child(initial_warning_visible)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .build();
        editor_inner.append(&warning_revealer);

        // ── Drawing area (built first so preset/sensor callbacks can use it) ──
        let drawing_area = gtk::DrawingArea::builder()
            .content_width(440)
            .content_height(320)
            .hexpand(true)
            .build();

        {
            let cs = curve_state.clone();
            drawing_area.set_draw_func(move |_area, cr, w, h| {
                draw_curve(cr, w as f64, h as f64, &cs.borrow());
            });
        }

        // ── Graph section: sensor + presets on one row, then drawing area ────────
        let graph_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();

        // Top controls row: sensor dropdown (left, expands) + preset chips (right)
        let controls_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();

        if sensors.is_empty() {
            let none_lbl = gtk::Label::builder()
                .label("No sensors")
                .css_classes(["dim-label", "caption"])
                .hexpand(true)
                .halign(gtk::Align::Start)
                .build();
            controls_row.append(&none_lbl);
        } else {
            // Index 0 is a sentinel "No sensor" entry; real sensors start at index 1.
            // This ensures an unassigned curve doesn't show a real sensor as selected,
            // which would make that sensor un-selectable (no selection-changed signal).
            let mut sensor_labels: Vec<String> = vec!["No sensor".to_string()];
            sensor_labels.extend(
                sensors
                    .iter()
                    .map(|(dev, s)| format!("{} · {}  {:.1}°C", dev, s.name, s.value)),
            );
            let sensor_ids: Vec<String> = sensors.iter().map(|(_, s)| s.id.clone()).collect();
            let selected_idx = initial_sensor_id
                .as_ref()
                .and_then(|id| sensor_ids.iter().position(|s| s == id))
                .map(|i| i + 1)
                .unwrap_or(0) as u32;

            let label_refs: Vec<&str> = sensor_labels.iter().map(|s| s.as_str()).collect();
            let sensor_model = gtk::StringList::new(&label_refs);
            let sensor_drop = gtk::DropDown::builder()
                .model(&sensor_model)
                .selected(selected_idx)
                .hexpand(true)
                .css_classes(["preset-chip"])
                .build();

            let da = drawing_area.clone();
            let cs = curve_state.clone();
            let sel = selected_sensor_id.clone();
            let sensors_vec = sensors.to_vec();
            let ra_sd = refresh_apply.clone();
            sensor_drop.connect_selected_notify(move |drop| {
                let idx = drop.selected() as usize;
                if idx == 0 {
                    // "No sensor" selected.
                    *sel.borrow_mut() = None;
                    cs.borrow_mut().current_temp = None;
                    da.queue_draw();
                } else if let Some((_, sensor)) = sensors_vec.get(idx - 1) {
                    *sel.borrow_mut() = Some(sensor.id.clone());
                    cs.borrow_mut().current_temp = Some(sensor.value);
                    da.queue_draw();
                }
                ra_sd();
            });

            controls_row.append(&sensor_drop);
        }

        if !preset_curves.is_empty() {
            let presets_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(4)
                .build();

            for preset in preset_curves {
                let btn = gtk::Button::builder()
                    .label(&preset.name)
                    .css_classes(["pill", "preset-chip"])
                    .build();
                let pts: Vec<[f64; 2]> = preset.points.iter()
                    .map(|p| [p[0] as f64, p[1] as f64])
                    .collect();
                let preset_id = preset.id.clone();
                let cs = curve_state.clone();
                let da = drawing_area.clone();
                let ra_p = refresh_apply.clone();
                btn.connect_clicked(move |_| {
                    {
                        let mut s = cs.borrow_mut();
                        s.points = pts.clone();
                        s.active_preset = Some(preset_id.clone());
                    }
                    da.queue_draw();
                    ra_p();
                });
                presets_box.append(&btn);
            }

            controls_row.append(&presets_box);
        }

        graph_section.append(&controls_row);

        // ── Drawing area ───────────────────────────────────────────────────────
        // Gesture: left-click = add/drag, right-click = remove
        let gesture_click = gtk::GestureClick::new();
        gesture_click.set_button(0);
        {
            let cs = curve_state.clone();
            let da = drawing_area.clone();
            let ra_gcp = refresh_apply.clone();
            gesture_click.connect_pressed(move |g, _n, mx, my| {
                let w = da.width() as f64;
                let h = da.height() as f64;
                {
                    let mut s = cs.borrow_mut();
                    if g.current_button() == 3 {
                        if s.points.len() > 2 {
                            if let Some(idx) = s.hit_test(mx, my, w, h) {
                                s.points.remove(idx);
                                s.active_preset = None;
                                da.queue_draw();
                            }
                        }
                    } else if let Some(idx) = s.hit_test(mx, my, w, h) {
                        s.dragging = Some(idx);
                        s.active_preset = None;
                    } else if CurveState::in_graph(mx, my, w, h) {
                        let temp = from_x(mx, w);
                        let duty = from_y(my, h);
                        s.points.push([temp, duty]);
                        s.points.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap());
                        s.active_preset = None;
                        da.queue_draw();
                    }
                }
                ra_gcp();
            });
        }
        {
            let cs = curve_state.clone();
            let da = drawing_area.clone();
            let ra_gcr = refresh_apply.clone();
            gesture_click.connect_released(move |_, _, _, _| {
                cs.borrow_mut().dragging = None;
                da.queue_draw();
                ra_gcr();
            });
        }
        drawing_area.add_controller(gesture_click);

        let motion = gtk::EventControllerMotion::new();
        {
            let cs = curve_state.clone();
            let da = drawing_area.clone();
            let ra_m = refresh_apply.clone();
            motion.connect_motion(move |_, mx, my| {
                let w = da.width() as f64;
                let h = da.height() as f64;
                let was_dragging = {
                    let mut s = cs.borrow_mut();
                    if let Some(idx) = s.dragging {
                        let temp = from_x(mx, w);
                        let duty = from_y(my, h);
                        s.points[idx] = [temp, duty];
                        s.points.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap());
                        s.dragging = s.points.iter().position(|p| {
                            (p[0] - temp).abs() < 0.5 && (p[1] - duty).abs() < 0.5
                        });
                        da.queue_draw();
                        true
                    } else {
                        false
                    }
                };
                if was_dragging { ra_m(); }
            });
        }
        drawing_area.add_controller(motion);

        graph_section.append(&drawing_area);
        editor_inner.append(&graph_section);

        // ── Bottom row: hint + Apply ───────────────────────────────────────────
        let bottom_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(4)
            .build();

        let hint = gtk::Label::builder()
            .label("Left-click to add · Right-click to remove · Drag to move")
            .css_classes(["dim-label", "caption"])
            .halign(gtk::Align::Start)
            .hexpand(true)
            .build();
        bottom_row.append(&hint);

        {
            let cs = curve_state.clone();
            let fan_id = fan_id.to_string();
            let sel = selected_sensor_id.clone();
            let store = store.clone();
            let cp = committed_points.clone();
            let csi = committed_sensor_id.clone();
            let ra = refresh_apply.clone();
            apply_btn.connect_clicked(move |_| {
                let (points, sensor_id, preset_id) = {
                    let s = cs.borrow();
                    (s.points.clone(), sel.borrow().clone(), s.active_preset.clone())
                };
                *cp.borrow_mut() = points.clone();
                *csi.borrow_mut() = sensor_id.clone();
                ra();
                if let Some(pid) = preset_id {
                    store.dispatch(crate::commands::Command::SetFanCurvePreset {
                        fan_id: fan_id.clone(),
                        preset_id: pid,
                        sensor_id,
                    });
                } else {
                    store.dispatch(crate::commands::Command::SetFanCurvePoints {
                        fan_id: fan_id.clone(),
                        points,
                        sensor_id,
                    });
                }
            });
        }
        bottom_row.append(&apply_btn);
        editor_inner.append(&bottom_row);

        // ── Stat chips (above the card) ────────────────────────────────────────
        let initial_curve_duty = initial_temp
            .map(|t| format!("{:.0}%", curve_state.borrow().duty_at_temp(t)))
            .unwrap_or_else(|| "—".to_string());

        let (temp_box, temp_label) = stat_card(
            "Sensor Temp",
            initial_temp.map(|t| format!("{:.1}°C", t)).as_deref().unwrap_or("—"),
        );
        let (rpm_box, rpm_label) = stat_card("Speed", &format!("{} RPM", fan_status.rpm));
        let (duty_box, duty_label) = stat_card("Duty", &format!("{}%", fan_status.duty));
        let (cur_duty_box, cur_duty_label) = stat_card("Curve Duty", &initial_curve_duty);

        let stats_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .homogeneous(true)
            .build();
        stats_row.append(&temp_box);
        stats_row.append(&rpm_box);
        stats_row.append(&duty_box);
        stats_row.append(&cur_duty_box);
        root.append(&stats_row);

        // ── Card wrapping the editor ───────────────────────────────────────────
        let editor_card = gtk::Box::builder()
            .css_classes(["card"])
            .build();
        editor_card.append(&editor_inner);
        root.append(&editor_card);

        Self {
            root,
            curve_state,
            drawing_area,
            rpm_label,
            duty_label,
            temp_label,
            cur_duty_label,
            warning_revealer,
            warning_label,
            selected_sensor_id,
        }
    }

    /// Called on every state broadcast — updates live readings without rebuilding.
    pub fn apply_fan_state(&self, curve: &WireFanCurve, fan_status: &FanStatus, all_sensors: &[(String, Sensor)]) {
        match &curve.status {
            FanCurveStatus::NoSensor => {
                self.warning_label.set_text("No temperature sensor assigned — fan held at 75%");
                self.warning_revealer.set_reveal_child(true);
            }
            FanCurveStatus::SensorMalfunction => {
                self.warning_label.set_text("Sensor malfunction detected — fan held at 75%");
                self.warning_revealer.set_reveal_child(true);
            }
            FanCurveStatus::WriteError(e) if e.contains("Permission denied") => {
                self.warning_label.set_text("Permission denied writing PWM — run: sudo udevadm trigger --subsystem-match=hwmon");
                self.warning_revealer.set_reveal_child(true);
            }
            FanCurveStatus::WriteError(_) => {
                self.warning_label.set_text("Failed to set fan speed — check daemon logs");
                self.warning_revealer.set_reveal_child(true);
            }
            FanCurveStatus::FanStalled => {
                self.warning_label.set_text("Fan is not spinning (0 RPM) — check connections or replace the fan");
                self.warning_revealer.set_reveal_child(true);
            }
            FanCurveStatus::Ok => {
                self.warning_revealer.set_reveal_child(false);
            }
        }

        self.rpm_label.set_text(&format!("{} RPM", fan_status.rpm));
        self.duty_label.set_text(&format!("{}%", fan_status.duty));

        if let Some(sid) = self.selected_sensor_id.borrow().as_deref() {
            if let Some((_, sensor)) = all_sensors.iter().find(|(_, s)| s.id == sid) {
                let temp = sensor.value;
                self.temp_label.set_text(&format!("{:.1}°C", temp));
                let curve_duty = self.curve_state.borrow().duty_at_temp(temp);
                self.cur_duty_label.set_text(&format!("{:.0}%", curve_duty));
                self.curve_state.borrow_mut().current_temp = Some(temp);
                self.drawing_area.queue_draw();
            }
        }
    }
}

fn stat_card(title: &str, value: &str) -> (gtk::Box, gtk::Label) {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .css_classes(["card"])
        .build();
    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_start(10)
        .margin_end(10)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    let title_lbl = gtk::Label::builder()
        .label(title)
        .halign(gtk::Align::Start)
        .css_classes(["caption", "dim-label"])
        .build();
    let val_lbl = gtk::Label::builder()
        .label(value)
        .halign(gtk::Align::Start)
        .css_classes(["title-4"])
        .build();
    inner.append(&title_lbl);
    inner.append(&val_lbl);
    card.append(&inner);
    (card, val_lbl)
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    cr.new_sub_path();
    cr.arc(x + r, y + r, r, PI, 3.0 * PI / 2.0);
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.close_path();
}

fn draw_curve(cr: &cairo::Context, w: f64, h: f64, state: &CurveState) {
    let gx = PAD_L;
    let gy = PAD_T;
    let gw = w - PAD_L - PAD_R;
    let gh = h - PAD_T - PAD_B;
    let corner = 10.0;
    let bottom = h - PAD_B;
    let right = w - PAD_R;

    // Background — subtle dark, blends with card
    rounded_rect(cr, gx, gy, gw, gh, corner);
    cr.clip();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.25);
    cr.paint().ok();

    // Subtle grid — horizontal duty lines only
    cr.set_line_width(1.0);
    for d_val in [25u32, 50, 75] {
        let y = ty(d_val as f64, h);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.04);
        cr.move_to(gx, y);
        cr.line_to(right, y);
        cr.stroke().ok();
    }
    // Vertical temp lines — very subtle
    for t_val in [40u32, 60, 80] {
        let x = tx(t_val as f64, w);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.04);
        cr.move_to(x, gy);
        cr.line_to(x, bottom);
        cr.stroke().ok();
    }

    // Axis labels
    cr.reset_clip();
    cr.select_font_face("sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(9.5);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.30);
    for t_val in [20u32, 40, 60, 80, 100] {
        let x = tx(t_val as f64, w);
        let label = format!("{}°", t_val);
        let ext = cr.text_extents(&label).ok();
        let ew = ext.as_ref().map_or(0.0, |e| e.width());
        cr.move_to(x - ew / 2.0, h - PAD_B + 15.0);
        cr.show_text(&label).ok();
    }
    for d_val in [0u32, 50, 100] {
        let y = ty(d_val as f64, h);
        let label = format!("{}%", d_val);
        let ext = cr.text_extents(&label).ok();
        let ew = ext.as_ref().map_or(0.0, |e| e.width());
        let eh = ext.as_ref().map_or(0.0, |e| e.height());
        cr.move_to(gx - ew - 6.0, y + eh / 2.0);
        cr.show_text(&label).ok();
    }

    if state.points.is_empty() {
        return;
    }

    // Re-clip for curve drawing
    rounded_rect(cr, gx, gy, gw, gh, corner);
    cr.clip();

    let pts = &state.points;
    let screen: Vec<(f64, f64)> = pts.iter().map(|p| (tx(p[0] as f64, w), ty(p[1] as f64, h))).collect();
    let last = *screen.last().unwrap();

    // Build the filled path shape (left edge down, across bottom, up to first point, along curve)
    let fill_path = |cr: &cairo::Context| {
        cr.move_to(screen[0].0, bottom);
        for &(sx, sy) in &screen { cr.line_to(sx, sy); }
        cr.line_to(right, last.1);
        cr.line_to(right, bottom);
        cr.close_path();
    };

    // Gradient fill — deep blue fade
    let grad = cairo::LinearGradient::new(0.0, gy, 0.0, bottom);
    grad.add_color_stop_rgba(0.0, 0.18, 0.52, 1.0, 0.35);
    grad.add_color_stop_rgba(0.6, 0.10, 0.30, 0.70, 0.12);
    grad.add_color_stop_rgba(1.0, 0.05, 0.10, 0.30, 0.02);
    cr.set_source(&grad).ok();
    fill_path(cr);
    cr.fill().ok();

    // Glow pass — wide, faint blue stroke for the soft glow effect
    cr.move_to(screen[0].0, screen[0].1);
    for &(sx, sy) in &screen[1..] { cr.line_to(sx, sy); }
    cr.line_to(right, last.1);
    cr.set_line_width(8.0);
    cr.set_source_rgba(0.28, 0.65, 1.0, 0.15);
    cr.stroke().ok();

    // Curve line — crisp blue
    cr.move_to(screen[0].0, screen[0].1);
    for &(sx, sy) in &screen[1..] { cr.line_to(sx, sy); }
    cr.line_to(right, last.1);
    cr.set_line_width(2.0);
    cr.set_source_rgb(0.30, 0.65, 1.0);
    cr.stroke().ok();

    // Current temp indicator
    if let Some(ct) = state.current_temp {
        let x = tx(ct.clamp(T_MIN, T_MAX), w);
        let duty = state.duty_at_temp(ct);
        let dy = ty(duty, h);

        // Solid vertical line — warm amber
        cr.set_source_rgba(1.0, 0.60, 0.20, 0.70);
        cr.set_line_width(1.5);
        cr.move_to(x, gy);
        cr.line_to(x, bottom);
        cr.stroke().ok();

        // Intersection dot on the curve
        cr.arc(x, dy, 4.0, 0.0, 2.0 * PI);
        cr.set_source_rgba(1.0, 0.65, 0.25, 1.0);
        cr.fill().ok();
        cr.arc(x, dy, 4.0, 0.0, 2.0 * PI);
        cr.set_line_width(1.5);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.6);
        cr.stroke().ok();

        // Compact label — anchored near the top of the line
        let label = format!("{:.0}°  {:.0}%", ct, duty);
        cr.set_font_size(9.5);
        let ext = cr.text_extents(&label).ok();
        let ew = ext.as_ref().map_or(0.0, |e| e.width());
        let eh = ext.as_ref().map_or(0.0, |e| e.height());
        let lx = (x + 6.0).min(right - ew - 4.0);
        let ly = gy + 13.0;
        // small pill background
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.45);
        rounded_rect(cr, lx - 3.0, ly - eh - 1.0, ew + 6.0, eh + 4.0, 3.0);
        cr.fill().ok();
        cr.set_source_rgba(1.0, 0.70, 0.30, 0.90);
        cr.move_to(lx, ly);
        cr.show_text(&label).ok();
    }

    // Control points — white fill, blue ring; label only for dragged point
    for (i, &(sx, sy)) in screen.iter().enumerate() {
        let is_drag = state.dragging == Some(i);
        let r = if is_drag { PT_RADIUS + 1.5 } else { PT_RADIUS };

        // Outer glow for dragged point
        if is_drag {
            cr.arc(sx, sy, r + 5.0, 0.0, 2.0 * PI);
            cr.set_source_rgba(0.30, 0.65, 1.0, 0.20);
            cr.fill().ok();
        }

        // White fill
        cr.arc(sx, sy, r, 0.0, 2.0 * PI);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.fill().ok();

        // Blue ring
        cr.arc(sx, sy, r, 0.0, 2.0 * PI);
        cr.set_line_width(if is_drag { 2.5 } else { 1.8 });
        cr.set_source_rgb(0.25, 0.60, 1.0);
        cr.stroke().ok();

        // Label only for the actively dragged point
        if is_drag {
            let label = format!("{:.0}°  {:.0}%", pts[i][0], pts[i][1]);
            cr.set_font_size(10.0);
            let ext = cr.text_extents(&label).ok();
            let ew = ext.as_ref().map_or(0.0, |e| e.width());
            let eh = ext.as_ref().map_or(0.0, |e| e.height());
            let lx = (sx - ew / 2.0).clamp(gx + 2.0, right - ew - 2.0);
            let ly = if sy - r - 8.0 > gy + eh + 2.0 {
                sy - r - 8.0
            } else {
                sy + r + eh + 6.0
            };
            // pill background
            cr.set_source_rgba(0.10, 0.10, 0.18, 0.80);
            rounded_rect(cr, lx - 4.0, ly - eh - 2.0, ew + 8.0, eh + 6.0, 4.0);
            cr.fill().ok();
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
            cr.move_to(lx, ly);
            cr.show_text(&label).ok();
        }
    }

    // Border — very subtle, drawn last so it's on top
    cr.reset_clip();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.07);
    cr.set_line_width(1.0);
    rounded_rect(cr, gx, gy, gw, gh, corner);
    cr.stroke().ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_point_curve() -> Vec<[f64; 2]> {
        vec![[20.0, 10.0], [80.0, 100.0]]
    }

    #[test]
    fn duty_at_temp_empty_returns_zero() {
        assert_eq!(duty_at_temp(&[], 50.0), 0.0);
    }

    #[test]
    fn duty_at_temp_below_first_point_clamps_to_first_duty() {
        let pts = two_point_curve();
        assert_eq!(duty_at_temp(&pts, 10.0), 10.0);
        assert_eq!(duty_at_temp(&pts, 20.0), 10.0);
    }

    #[test]
    fn duty_at_temp_above_last_point_clamps_to_last_duty() {
        let pts = two_point_curve();
        assert_eq!(duty_at_temp(&pts, 80.0), 100.0);
        assert_eq!(duty_at_temp(&pts, 90.0), 100.0);
    }

    #[test]
    fn duty_at_temp_interpolates_linearly() {
        let pts = two_point_curve(); // 20→10%, 80→100%: slope = 1.5%/°C
        let result = duty_at_temp(&pts, 50.0); // midpoint: (50-20)/(80-20)*90 + 10 = 55%
        assert!((result - 55.0).abs() < 1e-9);
    }

    #[test]
    fn duty_at_temp_single_point_always_returns_that_duty() {
        let pts = vec![[40.0, 60.0]];
        assert_eq!(duty_at_temp(&pts, 20.0), 60.0);
        assert_eq!(duty_at_temp(&pts, 40.0), 60.0);
        assert_eq!(duty_at_temp(&pts, 80.0), 60.0);
    }

    #[test]
    fn curve_state_hit_test_miss_far_from_points() {
        let state = CurveState {
            points: vec![[50.0, 50.0]],
            dragging: None,
            current_temp: None,
            active_preset: None,
        };
        // Place widget 400×300; the point lands at tx(50,400)/ty(50,300).
        let w = 400.0_f64;
        let h = 300.0_f64;
        let px = tx(50.0, w);
        let py = ty(50.0, h);
        assert!(state.hit_test(px + 50.0, py, w, h).is_none());
        assert!(state.hit_test(px, py + 50.0, w, h).is_none());
    }

    #[test]
    fn curve_state_hit_test_finds_point_within_radius() {
        let state = CurveState {
            points: vec![[20.0, 25.0], [80.0, 75.0]],
            dragging: None,
            current_temp: None,
            active_preset: None,
        };
        let w = 400.0_f64;
        let h = 300.0_f64;
        // Exactly on the second point's canvas position → should hit index 1.
        let px = tx(80.0, w);
        let py = ty(75.0, h);
        assert_eq!(state.hit_test(px, py, w, h), Some(1));
    }
}
