// SPDX-License-Identifier: GPL-3.0-or-later
//! A Home dashboard widget bound to live state, and the painter that draws it.
//! Shared by the dashboard row and the configuration dialog's live preview.

use std::collections::{HashMap, VecDeque};

use egui::{Align2, Color32, Pos2, Rect, Stroke, Vec2};
use halod_shared::types::{
    battery_ref, split_battery_ref, BatteryStatus, DeviceCapability, HomeWidget, HomeWidgetKind,
};

use crate::domain::models::sensors::SensorView;
use crate::domain::topic_store::TopicStore;
use crate::ui::components as widgets;
use crate::ui::theme;

use super::ellipsize;
use crate::domain::state::HISTORY_LEN;

pub const CELL_H: f32 = 170.0;
const PAD: Vec2 = Vec2::new(20.0, 18.0);
const CONTENT_TOP: f32 = 47.0;
const DIAL: f32 = 104.0;

/// One battery line inside a battery widget.
pub struct BatteryRow {
    pub name: String,
    pub level: Option<u8>,
    pub charging: bool,
}

pub enum Data {
    Chart {
        sensor_id: String,
        value: f64,
        unit: &'static str,
    },
    Gauge {
        value: f64,
        unit: &'static str,
        fraction: f32,
    },
    Meter {
        value: f64,
        unit: &'static str,
        fraction: f32,
    },
    Battery(Vec<BatteryRow>),
}

/// A widget bound to the live values it renders this frame.
pub struct Resolved {
    pub label: String,
    pub source: String,
    pub data: Data,
}

/// Where `value` sits in `[min, max]`, clamped to the unit range.
pub fn gauge_fraction(value: f64, min: f64, max: f64) -> f32 {
    if max <= min {
        return 0.0;
    }
    (((value - min) / (max - min)) as f32).clamp(0.0, 1.0)
}

/// Bind a configured widget to live state. `None` when its source is gone — an
/// unplugged device must not silently delete the user's layout.
pub fn resolve(w: &HomeWidget, sensors: &[SensorView], state: &TopicStore) -> Option<Resolved> {
    let named = |default: &str| {
        if w.label.trim().is_empty() {
            default.to_string()
        } else {
            w.label.clone()
        }
    };
    match &w.kind {
        HomeWidgetKind::Chart { sensor_id } => {
            let s = sensors.iter().find(|s| &s.id == sensor_id)?;
            Some(Resolved {
                label: named(&s.label),
                source: s.device.clone(),
                data: Data::Chart {
                    sensor_id: s.id.clone(),
                    value: s.value,
                    unit: s.unit,
                },
            })
        }
        HomeWidgetKind::Gauge {
            sensor_id,
            min,
            max,
        } => {
            let s = sensors.iter().find(|s| &s.id == sensor_id)?;
            Some(Resolved {
                label: named(&s.label),
                source: s.device.clone(),
                data: Data::Gauge {
                    value: s.value,
                    unit: s.unit,
                    fraction: gauge_fraction(s.value, *min, *max),
                },
            })
        }
        HomeWidgetKind::Meter {
            sensor_id,
            min,
            max,
        } => {
            let s = sensors.iter().find(|s| &s.id == sensor_id)?;
            Some(Resolved {
                label: named(&s.label),
                source: s.device.clone(),
                data: Data::Meter {
                    value: s.value,
                    unit: s.unit,
                    fraction: gauge_fraction(s.value, *min, *max),
                },
            })
        }
        HomeWidgetKind::Battery { batteries } => {
            let rows: Vec<BatteryRow> = batteries
                .iter()
                .map(|reference| battery_row(reference, state))
                .collect();
            (!rows.is_empty()).then(|| Resolved {
                label: named(&t!("home.widget_battery")),
                source: String::new(),
                data: Data::Battery(rows),
            })
        }
    }
}

fn battery_row(reference: &str, state: &TopicStore) -> BatteryRow {
    let unavailable = BatteryRow {
        name: reference.to_owned(),
        level: None,
        charging: false,
    };
    let Some((device_id, key)) = split_battery_ref(reference) else {
        return unavailable;
    };
    let Some(device) = state.devices.iter().find(|d| d.id == device_id) else {
        return unavailable;
    };
    let Some(batteries) = device.capabilities.iter().find_map(|cap| match cap {
        DeviceCapability::Battery(b) => Some(b),
        _ => None,
    }) else {
        return unavailable;
    };
    match batteries.iter().find(|b| b.key == key) {
        Some(battery) => BatteryRow {
            name: battery_name(&device.name, &battery.label, batteries.len()),
            level: Some(battery.level),
            charging: battery.status == BatteryStatus::Charging,
        },
        None => BatteryRow {
            name: battery_name(&device.name, key, 2),
            level: None,
            charging: false,
        },
    }
}

/// A battery's display name: the device alone when it has a single cell, else
/// the device qualified by the cell's own label.
fn battery_name(device: &str, label: &str, cells: usize) -> String {
    if cells > 1 {
        format!("{device} · {label}")
    } else {
        device.to_string()
    }
}

/// Every battery on the system, as `(reference, display name)` pairs — the
/// battery widget's option list.
pub fn battery_options(state: &TopicStore) -> Vec<(String, String)> {
    state
        .devices
        .iter()
        .flat_map(|d| {
            d.capabilities.iter().filter_map(move |cap| match cap {
                DeviceCapability::Battery(b) => Some((d, b)),
                _ => None,
            })
        })
        .flat_map(|(d, batteries)| {
            batteries.iter().map(move |b| {
                (
                    battery_ref(&d.id, &b.key),
                    battery_name(&d.name, &b.label, batteries.len()),
                )
            })
        })
        .collect()
}

/// Draw a widget's content into its strip cell. Pure painting, so the
/// dashboard row and the configuration dialog's preview stay identical.
pub fn paint(
    p: &egui::Painter,
    rect: Rect,
    w: &HomeWidget,
    resolved: Option<&Resolved>,
    history: &HashMap<String, VecDeque<f32>>,
) {
    let Some(resolved) = resolved else {
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            t!("home.widget_unavailable"),
            theme::body_sm(),
            theme::TEXT_FAINT,
        );
        return;
    };
    let color = theme::widget_hue(w.color);
    let inner = rect.shrink2(PAD);
    header(p, inner, &resolved.label, &resolved.source);
    let top = Pos2::new(inner.left(), rect.top() + CONTENT_TOP);
    let samples = |id: &str| -> Vec<f32> {
        history
            .get(id)
            .map(|h| h.iter().copied().collect())
            .unwrap_or_default()
    };
    match &resolved.data {
        Data::Chart {
            sensor_id,
            value,
            unit,
        } => chart(p, inner, top, (*value, unit), &samples(sensor_id), color),
        Data::Gauge {
            value,
            unit,
            fraction,
        } => gauge(p, top, (*value, unit), *fraction, color),
        Data::Meter {
            value,
            unit,
            fraction,
        } => meter(p, inner, top, (*value, unit), *fraction, color),
        Data::Battery(rows) => battery(p, inner, top, rows),
    }
}

fn header(p: &egui::Painter, inner: Rect, label: &str, source: &str) {
    let y = inner.top() + 8.0;
    let font = theme::body_md();
    let source = p.text(
        Pos2::new(inner.right(), y),
        Align2::RIGHT_CENTER,
        ellipsize(p, source, &font, inner.width() * 0.5),
        font.clone(),
        theme::TEXT_MUT,
    );
    let label_font = theme::medium(13.0);
    p.text(
        Pos2::new(inner.left(), y),
        Align2::LEFT_CENTER,
        ellipsize(p, label, &label_font, source.left() - 12.0 - inner.left()),
        label_font,
        theme::TEXT,
    );
}

/// The large readout with its unit; returns the number's rect.
fn big_value(p: &egui::Painter, top: Pos2, value: f64, unit: &str) -> Rect {
    let v = p.text(
        top,
        Align2::LEFT_TOP,
        format!("{value:.0}"),
        theme::semibold(44.0),
        theme::TEXT,
    );
    p.text(
        Pos2::new(v.right() + 4.0, v.bottom() - 10.0),
        Align2::LEFT_BOTTOM,
        unit,
        theme::body(16.0),
        theme::TEXT_MUT,
    );
    v
}

fn chart(
    p: &egui::Painter,
    inner: Rect,
    top: Pos2,
    (value, unit): (f64, &str),
    samples: &[f32],
    color: Color32,
) {
    let v = big_value(p, top, value, unit);
    let spark = Rect::from_min_max(
        Pos2::new(inner.right() - 150.0, v.bottom() - 52.0),
        Pos2::new(inner.right(), v.bottom() - 8.0),
    );
    let pts = super::sparkline_points(spark, samples);
    if pts.len() >= 2 {
        p.add(egui::Shape::line(pts, Stroke::new(1.8, color)));
    }
    let y = v.bottom() + 4.0;
    p.text(
        Pos2::new(inner.right(), y),
        Align2::RIGHT_TOP,
        t!("home.widget_window", secs = HISTORY_LEN),
        theme::body_sm(),
        theme::TEXT_MUT,
    );
}

fn gauge(p: &egui::Painter, top: Pos2, (value, unit): (f64, &str), fraction: f32, color: Color32) {
    let center = top + Vec2::splat(DIAL / 2.0);
    widgets::arc_gauge(p, center, DIAL, fraction, color);
    let degrees = unit.starts_with('°');
    let readout = if degrees {
        format!("{value:.0}°")
    } else {
        format!("{value:.0}")
    };
    let font = theme::semibold(if readout.len() > 3 { 20.0 } else { 26.0 });
    if degrees {
        p.text(center, Align2::CENTER_CENTER, readout, font, theme::TEXT);
    } else {
        p.text(
            center - Vec2::new(0.0, 6.0),
            Align2::CENTER_CENTER,
            readout,
            font,
            theme::TEXT,
        );
        p.text(
            center + Vec2::new(0.0, 14.0),
            Align2::CENTER_CENTER,
            unit,
            theme::caption(),
            theme::TEXT_MUT,
        );
    }
}

const METER_SEGMENTS: usize = 20;
/// The top quarter of a meter's range reads as a warning.
const METER_WARN_FROM: f32 = 0.75;

/// Color of meter segment `i`: lit up to `fraction`, amber in the warning band.
fn meter_segment_color(i: usize, fraction: f32, color: Color32) -> Color32 {
    let at = i as f32 / (METER_SEGMENTS - 1) as f32;
    if at > fraction || fraction <= 0.0 {
        theme::BORDER
    } else if at >= METER_WARN_FROM {
        theme::STAT_AMBER
    } else {
        color
    }
}

fn meter(
    p: &egui::Painter,
    inner: Rect,
    top: Pos2,
    (value, unit): (f64, &str),
    fraction: f32,
    color: Color32,
) {
    let v = big_value(p, top, value, unit);
    let gap = 3.0;
    let seg_w = (inner.width() - gap * (METER_SEGMENTS - 1) as f32) / METER_SEGMENTS as f32;
    let bar_y = v.bottom() + 4.0;
    for i in 0..METER_SEGMENTS {
        let seg = Rect::from_min_size(
            Pos2::new(inner.left() + i as f32 * (seg_w + gap), bar_y),
            Vec2::new(seg_w, 8.0),
        );
        p.rect_filled(seg, 1.0, meter_segment_color(i, fraction, color));
    }
}

fn battery(p: &egui::Painter, inner: Rect, top: Pos2, rows: &[BatteryRow]) {
    let name_w = (inner.width() * 0.4).clamp(48.0, 150.0);
    for (i, row) in rows.iter().enumerate() {
        let y = top.y + 10.0 + i as f32 * 28.0;
        let name_color = if row.level.is_some() {
            theme::TEXT_DIM
        } else {
            theme::TEXT_FAINT
        };
        p.text(
            Pos2::new(inner.left(), y),
            Align2::LEFT_CENTER,
            ellipsize(p, &row.name, &theme::body_md(), name_w),
            theme::body_md(),
            name_color,
        );
        let track = Rect::from_min_max(
            Pos2::new(inner.left() + name_w + 12.0, y - 3.0),
            Pos2::new(inner.right() - 48.0, y + 3.0),
        );
        p.rect_filled(track, 1.0, theme::BORDER);
        let Some(level) = row.level else {
            p.text(
                Pos2::new(inner.right(), y),
                Align2::RIGHT_CENTER,
                t!("home.widget_battery_unavailable"),
                theme::caption(),
                theme::TEXT_FAINT,
            );
            continue;
        };
        let mut fill = track;
        fill.set_right(track.left() + track.width() * level as f32 / 100.0);
        p.rect_filled(fill, 1.0, theme::battery_color(level, row.charging));
        p.text(
            Pos2::new(inner.right(), y),
            Align2::RIGHT_CENTER,
            format!("{level}%"),
            theme::medium(13.0),
            theme::TEXT,
        );
    }
}

/// The tile border, drawn last so nothing bleeds over it.
pub fn frame(p: &egui::Painter, rect: Rect, color: Color32) {
    p.rect_stroke(
        rect,
        theme::RADIUS_MD,
        Stroke::new(1.0, color),
        egui::StrokeKind::Middle,
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn meter_lights_segments_up_to_the_value_and_warns_at_the_top() {
        let lit = |f: f32| {
            (0..METER_SEGMENTS)
                .filter(|&i| meter_segment_color(i, f, theme::CYAN) != theme::BORDER)
                .count()
        };
        assert_eq!(lit(0.0), 0);
        assert_eq!(lit(1.0), METER_SEGMENTS);
        assert!(lit(0.3) < lit(0.6));
        assert_eq!(
            meter_segment_color(METER_SEGMENTS - 1, 1.0, theme::CYAN),
            theme::STAT_AMBER
        );
        assert_eq!(meter_segment_color(0, 1.0, theme::CYAN), theme::CYAN);
    }

    use super::*;
    use halod_shared::types::{Battery, WireDevice};

    fn sensor(id: &str, label: &str, value: f64) -> SensorView {
        SensorView {
            id: id.into(),
            label: label.into(),
            device: String::new(),
            value,
            unit: "°C",
        }
    }

    fn widget(kind: HomeWidgetKind, label: &str) -> HomeWidget {
        HomeWidget {
            id: "w1".into(),
            kind,
            color: 0,
            label: label.into(),
        }
    }

    fn battery_device(id: &str, name: &str, batteries: Vec<Battery>) -> WireDevice {
        WireDevice {
            id: id.parse().unwrap(),
            name: name.into(),
            capabilities: vec![DeviceCapability::Battery(batteries)],
            ..Default::default()
        }
    }

    fn battery(key: &str, label: &str, level: u8) -> Battery {
        Battery {
            key: key.into(),
            label: label.into(),
            level,
            status: BatteryStatus::Discharging,
        }
    }

    #[test]
    fn resolve_chart_uses_the_sensor_name_unless_overridden() {
        let state = TopicStore::default();
        let sensors = vec![sensor("cpu", "CPU temp", 48.0)];
        let kind = HomeWidgetKind::Chart {
            sensor_id: "cpu".into(),
        };
        assert_eq!(
            resolve(&widget(kind.clone(), ""), &sensors, &state)
                .unwrap()
                .label,
            "CPU temp"
        );
        assert_eq!(
            resolve(&widget(kind, "Package"), &sensors, &state)
                .unwrap()
                .label,
            "Package"
        );
    }

    #[test]
    fn resolve_returns_none_for_a_vanished_source() {
        let state = TopicStore::default();
        let sensors = vec![sensor("cpu", "CPU temp", 48.0)];
        let gone = widget(
            HomeWidgetKind::Chart {
                sensor_id: "gpu".into(),
            },
            "",
        );
        assert!(resolve(&gone, &sensors, &state).is_none());
        let no_battery = widget(
            HomeWidgetKind::Battery {
                batteries: vec!["mouse/main".into()],
            },
            "",
        );
        let Data::Battery(rows) = resolve(&no_battery, &sensors, &state).unwrap().data else {
            panic!("a battery widget keeps its rows");
        };
        assert_eq!(rows[0].level, None);
        assert_eq!(rows[0].name, "mouse/main");
    }

    #[test]
    fn resolve_battery_names_multi_cell_devices_per_cell() {
        let state = TopicStore {
            devices: vec![
                battery_device(
                    "hs",
                    "Echo 7.1",
                    vec![battery("l", "L", 54), battery("r", "R", 91)],
                ),
                battery_device("ms", "Glide Pro", vec![battery("main", "Battery", 78)]),
            ],
            ..Default::default()
        };
        let w = widget(
            HomeWidgetKind::Battery {
                batteries: vec!["hs/l".into(), "ms/main".into(), "hs/gone".into()],
            },
            "",
        );
        let Data::Battery(rows) = resolve(&w, &[], &state).unwrap().data else {
            panic!("expected a battery widget");
        };
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["Echo 7.1 · L", "Glide Pro", "Echo 7.1 · gone"]);
        assert_eq!(
            rows.iter().map(|r| r.level).collect::<Vec<_>>(),
            [Some(54), Some(78), None]
        );
    }

    #[test]
    fn battery_rows_survive_a_device_id_change() {
        let before = TopicStore {
            devices: vec![battery_device(
                "logitech_1234",
                "Glide Pro",
                vec![battery("battery", "Battery", 60)],
            )],
            ..Default::default()
        };
        let w = widget(
            HomeWidgetKind::Battery {
                batteries: vec!["logitech_1234/battery".into()],
            },
            "",
        );
        let rows = |state: &TopicStore| match resolve(&w, &[], state).unwrap().data {
            Data::Battery(rows) => rows,
            _ => panic!("battery"),
        };
        assert_eq!(rows(&before)[0].level, Some(60));
        let after = TopicStore {
            devices: vec![battery_device(
                "logitech-1234",
                "Glide Pro",
                vec![battery("battery", "Battery", 60)],
            )],
            ..Default::default()
        };
        let unavailable = rows(&after);
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].level, None);
        assert_eq!(unavailable[0].name, "logitech_1234/battery");
    }

    #[test]
    fn battery_options_lists_every_cell() {
        let state = TopicStore {
            devices: vec![battery_device(
                "hs",
                "Echo 7.1",
                vec![battery("l", "L", 54), battery("r", "R", 91)],
            )],
            ..Default::default()
        };
        let refs: Vec<_> = battery_options(&state)
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        assert_eq!(refs, vec!["hs/l".to_string(), "hs/r".to_string()]);
    }

    #[test]
    fn gauge_fraction_clamps_outside_the_configured_range() {
        assert_eq!(gauge_fraction(50.0, 0.0, 100.0), 0.5);
        assert_eq!(gauge_fraction(-10.0, 0.0, 100.0), 0.0);
        assert_eq!(gauge_fraction(140.0, 0.0, 100.0), 1.0);
        assert_eq!(gauge_fraction(30.0, 20.0, 40.0), 0.5);
        assert_eq!(gauge_fraction(30.0, 40.0, 40.0), 0.0);
    }
}
