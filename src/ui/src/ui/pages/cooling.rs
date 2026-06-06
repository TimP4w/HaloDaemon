use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::store::{Store, NavTarget};
use crate::state::AppState;
use halod_protocol::types::{
    DeviceCapability, FanCurveStatus, Sensor, WireFanCurve, WirePresetCurve,
};

// Coordinate helpers — same temp/duty domain as fan_curve.rs
const T_MIN: f64 = 20.0;
const T_MAX: f64 = 100.0;
const D_MAX: f64 = 100.0;
const PAD_L: f64 = 4.0;
const PAD_T: f64 = 4.0;
const PAD_R: f64 = 4.0;
const PAD_B: f64 = 4.0;

fn tx(temp: f64, w: f64) -> f64 {
    PAD_L + (temp - T_MIN) / (T_MAX - T_MIN) * (w - PAD_L - PAD_R)
}
fn ty(duty: f64, h: f64) -> f64 {
    PAD_T + (1.0 - duty / D_MAX) * (h - PAD_T - PAD_B)
}

fn duty_at_temp(points: &[[f32; 2]], temp: f64) -> f64 {
    let pts: Vec<[f64; 2]> = points.iter().map(|p| [p[0] as f64, p[1] as f64]).collect();
    crate::ui::widgets::fan_curve::duty_at_temp(&pts, temp)
}

fn status_icon_name(status: &FanCurveStatus) -> Option<&'static str> {
    match status {
        FanCurveStatus::Ok => None,
        FanCurveStatus::NoSensor | FanCurveStatus::SensorMalfunction => {
            Some("dialog-warning-symbolic")
        }
        FanCurveStatus::FanStalled | FanCurveStatus::WriteError(_) => Some("dialog-error-symbolic"),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Mini curve drawing
// ──────────────────────────────────────────────────────────────────────────────

fn draw_mini_curve(
    cr: &cairo::Context,
    w: f64,
    h: f64,
    points: &[[f32; 2]],
    current_temp: Option<f64>,
) {
    if points.is_empty() {
        return;
    }

    let bottom = h - PAD_B;
    let right = w - PAD_R;

    // Y-axis duty ticks — faint horizontal lines at 25 / 50 / 75 %
    cr.set_line_width(1.0);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.07);
    for duty_pct in [25.0, 50.0, 75.0_f64] {
        let y = ty(duty_pct, h).round() + 0.5;
        cr.move_to(PAD_L, y);
        cr.line_to(right, y);
        cr.stroke().ok();
    }

    // X-axis temperature ticks — short marks at bottom for 0, 25, 50, 75, 100 °C
    cr.set_line_width(1.0);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.15);
    for temp in [0.0, 25.0, 50.0, 75.0, 100.0_f64] {
        if temp < T_MIN || temp > T_MAX {
            continue;
        }
        let x = tx(temp, w).round() + 0.5;
        cr.move_to(x, bottom - 5.0);
        cr.line_to(x, bottom);
        cr.stroke().ok();
    }

    let screen: Vec<(f64, f64)> = points
        .iter()
        .map(|p| (tx(p[0] as f64, w), ty(p[1] as f64, h)))
        .collect();
    let last = *screen.last().unwrap();

    // Gradient fill
    let fill_path = |cr: &cairo::Context| {
        cr.move_to(screen[0].0, bottom);
        for &(sx, sy) in &screen {
            cr.line_to(sx, sy);
        }
        cr.line_to(right, last.1);
        cr.line_to(right, bottom);
        cr.close_path();
    };
    let grad = cairo::LinearGradient::new(0.0, PAD_T, 0.0, bottom);
    grad.add_color_stop_rgba(0.0, 0.18, 0.52, 1.0, 0.30);
    grad.add_color_stop_rgba(1.0, 0.05, 0.10, 0.30, 0.02);
    cr.set_source(&grad).ok();
    fill_path(cr);
    cr.fill().ok();

    // Glow
    cr.move_to(screen[0].0, screen[0].1);
    for &(sx, sy) in &screen[1..] {
        cr.line_to(sx, sy);
    }
    cr.line_to(right, last.1);
    cr.set_line_width(5.0);
    cr.set_source_rgba(0.28, 0.65, 1.0, 0.15);
    cr.stroke().ok();

    // Curve line
    cr.move_to(screen[0].0, screen[0].1);
    for &(sx, sy) in &screen[1..] {
        cr.line_to(sx, sy);
    }
    cr.line_to(right, last.1);
    cr.set_line_width(1.5);
    cr.set_source_rgb(0.30, 0.65, 1.0);
    cr.stroke().ok();

    // Current temp marker
    if let Some(ct) = current_temp {
        let x = tx(ct.clamp(T_MIN, T_MAX), w);
        let duty = duty_at_temp(points, ct);
        let dy = ty(duty, h);

        cr.set_source_rgba(1.0, 0.60, 0.20, 0.70);
        cr.set_line_width(1.0);
        cr.move_to(x, PAD_T);
        cr.line_to(x, bottom);
        cr.stroke().ok();

        cr.arc(x, dy, 3.0, 0.0, 2.0 * PI);
        cr.set_source_rgba(1.0, 0.65, 0.25, 1.0);
        cr.fill().ok();
        cr.arc(x, dy, 3.0, 0.0, 2.0 * PI);
        cr.set_line_width(1.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.6);
        cr.stroke().ok();
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// FanRow — one ExpanderRow per managed fan
// ──────────────────────────────────────────────────────────────────────────────

struct FanRow {
    fan_id: String,
    expander: adw::ExpanderRow,
    status_icon: gtk::Image,
    stat_lbl: gtk::Label,
    val_rpm: gtk::Label,
    val_duty: gtk::Label,
    val_sensor: gtk::Label,
    val_curve: gtk::Label,
    drawing_area: gtk::DrawingArea,
    curve_points: Rc<RefCell<Vec<[f32; 2]>>>,
    current_temp: Rc<Cell<Option<f64>>>,
    sensor_drop: gtk::DropDown,
    sensor_ids: Vec<String>,
    preset_drop: gtk::DropDown,
    preset_points: Vec<Vec<[f32; 2]>>,
    current_sensor_id: Rc<RefCell<Option<String>>>,
}

fn make_info_row(key: &str, value: &str) -> (gtk::Box, gtk::Label) {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let key_lbl = gtk::Label::builder()
        .label(key)
        .css_classes(["dim-label"])
        .halign(gtk::Align::Start)
        .width_chars(10)
        .build();
    let val_lbl = gtk::Label::builder()
        .label(value)
        .halign(gtk::Align::Start)
        .build();
    row.append(&key_lbl);
    row.append(&val_lbl);
    (row, val_lbl)
}

fn build_fan_row(
    fan: &WireFanCurve,
    device_name: &str,
    presets: &[WirePresetCurve],
    sensors: &[(String, Sensor)],
    current_temp_val: Option<f64>,
    rpm: u32,
    duty: u8,
    store: Store,
    ctx: &Store,
) -> FanRow {
    let fan_id = fan.fan_id.clone();

    let expander = adw::ExpanderRow::builder().title(device_name).build();

    // Status icon suffix — hidden when OK, shows warning/error otherwise
    let status_icon = gtk::Image::builder()
        .pixel_size(16)
        .valign(gtk::Align::Center)
        .build();
    if let Some(icon) = status_icon_name(&fan.status) {
        status_icon.set_icon_name(Some(icon));
        status_icon.set_visible(true);
    } else {
        status_icon.set_visible(false);
    }
    expander.add_suffix(&status_icon);

    // Live stats suffix
    let stat_lbl = gtk::Label::builder()
        .label(format!("{} rpm · {}%", rpm, duty))
        .css_classes(["dim-label"])
        .valign(gtk::Align::Center)
        .build();
    expander.add_suffix(&stat_lbl);

    // ── Inline controls in expander suffix ───────────────────────────────────
    let preset_model = gtk::StringList::new(&[]);
    for p in presets {
        preset_model.append(&p.name);
    }
    let preset_drop = gtk::DropDown::builder()
        .model(&preset_model)
        .valign(gtk::Align::Center)
        .build();
    let preset_selected = presets
        .iter()
        .position(|p| p.points == fan.points)
        .map(|i| i as u32)
        .unwrap_or(gtk::INVALID_LIST_POSITION);
    preset_drop.set_selected(preset_selected);

    // Index 0 = "No sensor"; real sensors start at index 1.
    let sensor_ids: Vec<String> = sensors.iter().map(|(_, s)| s.id.clone()).collect();
    let sensor_model = gtk::StringList::new(&["No sensor"]);
    for (dev_name, s) in sensors {
        sensor_model.append(&format!("{} — {}", dev_name, s.name));
    }
    let sensor_drop = gtk::DropDown::builder()
        .model(&sensor_model)
        .valign(gtk::Align::Center)
        .build();
    let sensor_selected = fan
        .sensor_id
        .as_ref()
        .and_then(|sid| sensor_ids.iter().position(|id| id == sid))
        .map(|i| (i + 1) as u32)
        .unwrap_or(0);
    sensor_drop.set_selected(sensor_selected);

    // ── Curve points + temp state ─────────────────────────────────────────────
    let curve_points: Rc<RefCell<Vec<[f32; 2]>>> = Rc::new(RefCell::new(fan.points.clone()));
    let current_temp: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(current_temp_val));
    let current_sensor_id: Rc<RefCell<Option<String>>> =
        Rc::new(RefCell::new(fan.sensor_id.clone()));

    // ── Preset signal (needs curve_points + drawing_area ref — wired below) ──
    // Stored here so we can connect after building drawing_area.

    // ── Detail row content ────────────────────────────────────────────────────
    // Info rows (will be placed in overlay)
    let curve_duty_at_temp = current_temp_val.map(|t| duty_at_temp(&fan.points, t));

    let sensor_display = fan
        .sensor_id
        .as_ref()
        .and_then(|sid| sensors.iter().find(|(_, s)| &s.id == sid));
    let sensor_text = match (sensor_display, current_temp_val) {
        (Some((_, s)), Some(t)) => format!("{} · {:.0}°C", s.name, t),
        (Some((_, s)), None) => s.name.clone(),
        _ => "—".to_string(),
    };
    let curve_text = curve_duty_at_temp
        .zip(current_temp_val)
        .map(|(d, t)| format!("{:.0}% at {:.0}°C", d, t))
        .unwrap_or_else(|| "—".to_string());

    let (row_rpm, val_rpm) = make_info_row("Speed", &format!("{} rpm", rpm));
    let (row_duty, val_duty) = make_info_row("Duty", &format!("{}%", duty));
    let (row_sensor, val_sensor) = make_info_row("Sensor", &sensor_text);
    let (row_curve, val_curve) = make_info_row("Curve", &curve_text);

    let stats_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .css_classes(["cooling-stats-overlay"])
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .margin_top(8)
        .margin_start(10)
        .build();
    stats_box.append(&row_rpm);
    stats_box.append(&row_duty);
    stats_box.append(&row_sensor);
    stats_box.append(&row_curve);

    // Curve drawing area — tall, fills full expanded height
    let drawing_area = gtk::DrawingArea::builder()
        .height_request(200)
        .hexpand(true)
        .build();

    {
        let pts = curve_points.clone();
        let ct = current_temp.clone();
        drawing_area.set_draw_func(move |_area, cr, w, h| {
            draw_mini_curve(cr, w as f64, h as f64, &pts.borrow(), ct.get());
        });
    }

    // Edit button floating bottom-right of curve
    let edit_btn = gtk::Button::builder()
        .label("Edit curve →")
        .css_classes(["pill"])
        .halign(gtk::Align::End)
        .valign(gtk::Align::End)
        .margin_bottom(8)
        .margin_end(8)
        .build();
    {
        let fan_id_btn = fan_id.clone();
        let store_btn = ctx.clone();
        edit_btn.connect_clicked(move |_| {
            store_btn.navigate(NavTarget::Device(fan_id_btn.clone()));
        });
    }

    let curve_overlay = gtk::Overlay::builder()
        .margin_top(4)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    curve_overlay.set_child(Some(&drawing_area));
    curve_overlay.add_overlay(&stats_box);
    curve_overlay.add_overlay(&edit_btn);

    // ── Wire preset signal now that drawing_area exists ───────────────────────
    {
        let fan_id_p = fan_id.clone();
        let current_sensor_id_p = current_sensor_id.clone();
        let presets_p: Vec<WirePresetCurve> = presets.to_vec();
        let pts = curve_points.clone();
        let da = drawing_area.clone();
        let store_p = store.clone();
        preset_drop.connect_selected_notify(move |d| {
            if let Some(preset) = presets_p.get(d.selected() as usize) {
                let current_pts = store_p.state().fan_curves.iter()
                    .find(|c| c.fan_id == fan_id_p)
                    .map(|c| c.points.clone());
                if current_pts.as_ref() == Some(&preset.points) { return; }
                *pts.borrow_mut() = preset.points.clone();
                da.queue_draw();
                let sensor_id = current_sensor_id_p.borrow().clone();
                store_p.dispatch(crate::commands::Command::SetFanCurvePreset {
                    fan_id: fan_id_p.clone(),
                    preset_id: preset.id.clone(),
                    sensor_id,
                });
            }
        });
    }
    expander.add_suffix(&preset_drop);

    // ── Wire sensor signal ────────────────────────────────────────────────────
    {
        let fan_id_s = fan_id.clone();
        let store_s = store.clone();
        let pts = curve_points.clone();
        let ids = sensor_ids.clone();
        sensor_drop.connect_selected_notify(move |d| {
            let idx = d.selected() as usize;
            let sensor_id = if idx > 0 { ids.get(idx - 1).cloned() } else { None };
            let current = store_s.state().fan_curves.iter()
                .find(|c| c.fan_id == fan_id_s)
                .and_then(|c| c.sensor_id.clone());
            if current == sensor_id { return; }
            let points: Vec<[f64; 2]> = pts.borrow().iter().map(|p| [p[0] as f64, p[1] as f64]).collect();
            store_s.dispatch(crate::commands::Command::SetFanCurvePoints {
                fan_id: fan_id_s.clone(),
                points,
                sensor_id,
            });
        });
    }
    expander.add_suffix(&sensor_drop);

    // Wrap in ListBoxRow for adw::ExpanderRow::add_row
    let detail_row = gtk::ListBoxRow::builder()
        .selectable(false)
        .activatable(false)
        .build();
    detail_row.set_child(Some(&curve_overlay));
    expander.add_row(&detail_row);

    FanRow {
        fan_id,
        expander,
        status_icon,
        stat_lbl,
        val_rpm,
        val_duty,
        val_sensor,
        val_curve,
        drawing_area,
        curve_points,
        current_temp,
        sensor_drop,
        sensor_ids,
        preset_drop,
        preset_points: presets.iter().map(|p| p.points.clone()).collect(),
        current_sensor_id,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// CoolingPage
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct CoolingPage {
    pub root: gtk::Box,
    failsafe_scale: gtk::Scale,
    fan_list: gtk::ListBox,
    initialized: Rc<Cell<bool>>,
    fan_rows: Rc<RefCell<Vec<FanRow>>>,
}

fn section_heading(title: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(title)
        .halign(gtk::Align::Start)
        .css_classes(["home-section-label"])
        .build()
}

impl CoolingPage {
    pub fn new(store: &Store) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_start(32)
            .margin_end(32)
            .margin_top(28)
            .margin_bottom(32)
            .build();

        // ── Fan failsafe duty section ──────────────────────────────────────────
        let failsafe_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        failsafe_section.append(&section_heading("Fan Curve Engine"));

        let failsafe_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        let failsafe_row = adw::ActionRow::builder()
            .title("Fan Failsafe Duty")
            .subtitle(
                "Duty cycle applied when a fan's temperature sensor is absent or malfunctioning",
            )
            .activatable(false)
            .build();

        let failsafe_adj = gtk::Adjustment::new(75.0, 0.0, 100.0, 1.0, 10.0, 0.0);
        let failsafe_scale = gtk::Scale::builder()
            .adjustment(&failsafe_adj)
            .orientation(gtk::Orientation::Horizontal)
            .draw_value(true)
            .value_pos(gtk::PositionType::Right)
            .width_request(180)
            .valign(gtk::Align::Center)
            .build();
        failsafe_scale.set_digits(0);
        failsafe_scale.add_mark(0.0, gtk::PositionType::Bottom, None);
        failsafe_scale.add_mark(50.0, gtk::PositionType::Bottom, None);
        failsafe_scale.add_mark(100.0, gtk::PositionType::Bottom, None);

        let store_fs = store.clone();
        failsafe_scale.connect_value_changed(move |s| {
            store_fs.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({
                "type": "set_fan_failsafe_duty",
                "duty": s.value() as u8,
            })));
        });

        failsafe_row.add_suffix(&failsafe_scale);
        failsafe_list.append(&failsafe_row);
        failsafe_section.append(&failsafe_list);
        content.append(&failsafe_section);

        // ── Managed fans section ───────────────────────────────────────────────
        let fans_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        fans_section.append(&section_heading("Managed Fans"));

        let fan_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        fans_section.append(&fan_list);
        content.append(&fans_section);

        scroll.set_child(Some(&content));
        root.append(&scroll);

        CoolingPage {
            root,
            failsafe_scale,
            fan_list,
            initialized: Rc::new(Cell::new(false)),
            fan_rows: Rc::new(RefCell::new(vec![])),
        }
    }

    pub fn update_live(&self, state: &AppState, store: &Store) {
        if !self.initialized.get() {
            self.initialized.set(true);
            self.failsafe_scale
                .set_value(state.global_config.fan_failsafe_duty as f64);
        }

        let sensors = state.all_sensors();

        let current_sensor_ids: Vec<String> =
            sensors.iter().map(|(_, s)| s.id.clone()).collect();
        let new_ids: Vec<&str> = state.fan_curves.iter().map(|f| f.fan_id.as_str()).collect();
        let rows_ref = self.fan_rows.borrow();
        let old_ids: Vec<&str> = rows_ref.iter().map(|r| r.fan_id.as_str()).collect();
        let needs_rebuild = new_ids != old_ids
            || rows_ref.iter().any(|r| r.sensor_ids != current_sensor_ids);
        drop(rows_ref);

        if needs_rebuild {
            while let Some(child) = self.fan_list.first_child() {
                self.fan_list.remove(&child);
            }
            let mut rows = self.fan_rows.borrow_mut();
            rows.clear();

            for fan in &state.fan_curves {
                let device_name = state
                    .devices
                    .iter()
                    .find(|d| d.id == fan.fan_id)
                    .map(|d| d.name.as_str())
                    .unwrap_or("Unknown fan");

                let temp = fan.sensor_id.as_ref().and_then(|sid| {
                    sensors
                        .iter()
                        .find(|(_, s)| &s.id == sid)
                        .map(|(_, s)| s.value)
                });

                let (rpm, duty) = fan_rpm_duty(fan, state);

                let row = build_fan_row(
                    fan,
                    device_name,
                    &state.preset_curves,
                    &sensors,
                    temp,
                    rpm,
                    duty,
                    store.clone(),
                    store,
                );
                self.fan_list.append(&row.expander);
                rows.push(row);
            }
            return;
        }

        // Live refresh only
        let mut rows = self.fan_rows.borrow_mut();
        for (row, fan) in rows.iter_mut().zip(state.fan_curves.iter()) {
            let (rpm, duty) = fan_rpm_duty(fan, state);

            row.stat_lbl.set_text(&format!("{} rpm · {}%", rpm, duty));

            if let Some(icon) = status_icon_name(&fan.status) {
                row.status_icon.set_icon_name(Some(icon));
                row.status_icon.set_visible(true);
            } else {
                row.status_icon.set_visible(false);
            }

            let temp = fan.sensor_id.as_ref().and_then(|sid| {
                sensors
                    .iter()
                    .find(|(_, s)| &s.id == sid)
                    .map(|(_, s)| s.value)
            });
            row.current_temp.set(temp);
            *row.curve_points.borrow_mut() = fan.points.clone();

            row.val_rpm.set_text(&format!("{} rpm", rpm));
            row.val_duty.set_text(&format!("{}%", duty));

            let sensor_info = fan
                .sensor_id
                .as_ref()
                .and_then(|sid| sensors.iter().find(|(_, s)| &s.id == sid));
            let sensor_text = match (sensor_info, temp) {
                (Some((_, s)), Some(t)) => format!("{} · {:.0}°C", s.name, t),
                (Some((_, s)), None) => s.name.clone(),
                _ => "—".to_string(),
            };
            row.val_sensor.set_text(&sensor_text);

            let curve_text = temp
                .map(|t| duty_at_temp(&fan.points, t))
                .zip(temp)
                .map(|(d, t)| format!("{:.0}% at {:.0}°C", d, t))
                .unwrap_or_else(|| "—".to_string());
            row.val_curve.set_text(&curve_text);

            // Update sensor dropdown if the assigned sensor changed.
            *row.current_sensor_id.borrow_mut() = fan.sensor_id.clone();

            let sensor_selected = fan
                .sensor_id
                .as_ref()
                .and_then(|sid| row.sensor_ids.iter().position(|id| id == sid))
                .map(|i| (i + 1) as u32)
                .unwrap_or(0);
            if row.sensor_drop.selected() != sensor_selected {
                row.sensor_drop.set_selected(sensor_selected);
            }

            // Update preset dropdown if the active curve changed.
            let preset_selected = row
                .preset_points
                .iter()
                .position(|pts| *pts == fan.points)
                .map(|i| i as u32)
                .unwrap_or(gtk::INVALID_LIST_POSITION);
            if row.preset_drop.selected() != preset_selected {
                row.preset_drop.set_selected(preset_selected);
            }

            row.drawing_area.queue_draw();
        }
    }

    pub fn reset_init(&self) {
        self.initialized.set(false);
    }

    pub fn on_profile_switch(&self, state: &AppState, store: &Store) {
        while let Some(child) = self.fan_list.first_child() {
            self.fan_list.remove(&child);
        }
        self.fan_rows.borrow_mut().clear();
        self.initialized.set(false);
        self.update_live(state, store);
    }
}

fn fan_rpm_duty(fan: &WireFanCurve, state: &AppState) -> (u32, u8) {
    state
        .devices
        .iter()
        .find(|d| d.id == fan.fan_id)
        .and_then(|dev| {
            dev.capabilities.iter().find_map(|c| match c {
                DeviceCapability::Fan(fs) => Some((fs.rpm, fs.duty)),
                DeviceCapability::Pump(ps) => Some((ps.rpm, ps.duty)),
                _ => None,
            })
        })
        .unwrap_or((0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the f32→f64 adapter produces the same result as calling the
    /// shared function directly with f64 points.
    #[test]
    fn duty_at_temp_adapter_matches_f64_reference() {
        let pts_f32: Vec<[f32; 2]> = vec![[20.0, 10.0], [80.0, 100.0]];
        let pts_f64: Vec<[f64; 2]> = vec![[20.0, 10.0], [80.0, 100.0]];

        for temp in [10.0_f64, 20.0, 50.0, 80.0, 90.0] {
            let via_adapter = duty_at_temp(&pts_f32, temp);
            let direct = crate::ui::widgets::fan_curve::duty_at_temp(&pts_f64, temp);
            assert!(
                (via_adapter - direct).abs() < 1e-6,
                "mismatch at {temp}°: adapter={via_adapter}, direct={direct}"
            );
        }
    }

    #[test]
    fn duty_at_temp_adapter_empty_returns_zero() {
        assert_eq!(duty_at_temp(&[], 50.0), 0.0);
    }
}
