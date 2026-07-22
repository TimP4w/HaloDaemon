// SPDX-License-Identifier: GPL-3.0-or-later
//! A Home dashboard widget bound to live state, and the painter that draws it.
//! Shared by the dashboard row and the configuration dialog's live preview.

use std::collections::{HashMap, VecDeque};

use egui::{Align2, Color32, Pos2, Rect, Stroke};
use halod_shared::types::{
    battery_ref, split_battery_ref, BatteryStatus, DeviceCapability, HomeWidget, HomeWidgetKind,
};

use crate::domain::models::sensors::SensorView;
use crate::domain::topic_store::TopicStore;
use crate::ui::components as widgets;
use crate::ui::theme;

use super::ellipsize;

pub const TILE_H: f32 = 96.0;

/// One battery line inside a battery widget.
pub struct BatteryRow {
    pub name: String,
    pub level: u8,
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
    Battery(Vec<BatteryRow>),
}

/// A widget bound to the live values it renders this frame.
pub struct Resolved {
    pub label: String,
    pub data: Data,
}

/// A tile's footprint on the dashboard grid, in half-columns and rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub cols: usize,
    pub rows: usize,
}

impl Size {
    /// Charts, batteries and the "Add widget" placeholder: full width, one row.
    pub const FULL: Self = Self { cols: 2, rows: 1 };
    /// A gauge: a square tile — full width, two rows — so the dial reads large.
    pub const GAUGE: Self = Self { cols: 2, rows: 2 };
}

pub fn size(kind: &HomeWidgetKind) -> Size {
    match kind {
        HomeWidgetKind::Gauge { .. } => Size::GAUGE,
        _ => Size::FULL,
    }
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
                data: Data::Gauge {
                    value: s.value,
                    unit: s.unit,
                    fraction: gauge_fraction(s.value, *min, *max),
                },
            })
        }
        HomeWidgetKind::Battery { batteries } => {
            let rows: Vec<BatteryRow> = batteries
                .iter()
                .filter_map(|reference| battery_row(reference, state))
                .collect();
            (!rows.is_empty()).then(|| Resolved {
                label: named(&t!("home.widget_battery")),
                data: Data::Battery(rows),
            })
        }
    }
}

fn battery_row(reference: &str, state: &TopicStore) -> Option<BatteryRow> {
    let (device_id, key) = split_battery_ref(reference)?;
    let device = state.devices.iter().find(|d| d.id == device_id)?;
    let batteries = device.capabilities.iter().find_map(|cap| match cap {
        DeviceCapability::Battery(b) => Some(b),
        _ => None,
    })?;
    let battery = batteries.iter().find(|b| b.key == key)?;
    Some(BatteryRow {
        name: battery_name(&device.name, &battery.label, batteries.len()),
        level: battery.level,
        charging: battery.status == BatteryStatus::Charging,
    })
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

/// Draw a widget into `rect`. Pure painting, so the dashboard row and the
/// configuration dialog's preview stay identical.
pub fn paint(
    p: &egui::Painter,
    rect: Rect,
    w: &HomeWidget,
    resolved: Option<&Resolved>,
    history: &HashMap<String, VecDeque<f32>>,
) {
    let color = theme::widget_hue(w.color);
    p.rect_filled(rect, theme::RADIUS_LG, theme::CARD_BG);
    match resolved.map(|r| (r.label.as_str(), &r.data)) {
        Some((
            label,
            Data::Chart {
                sensor_id,
                value,
                unit,
            },
        )) => chart(p, rect, label, *value, unit, color, sensor_id, history),
        Some((
            label,
            Data::Gauge {
                value,
                unit,
                fraction,
            },
        )) => gauge(p, rect, label, *value, unit, *fraction, color),
        Some((label, Data::Battery(rows))) => battery(p, rect, label, rows),
        None => {
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                t!("home.widget_unavailable"),
                theme::body_sm(),
                theme::TEXT_FAINT,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn chart(
    p: &egui::Painter,
    rect: Rect,
    label: &str,
    value: f64,
    unit: &str,
    color: Color32,
    sensor_id: &str,
    history: &HashMap<String, VecDeque<f32>>,
) {
    let samples: Option<Vec<f32>> = history.get(sensor_id).map(|h| h.iter().copied().collect());
    if let Some(samples) = &samples {
        super::sparkline(&p.with_clip_rect(rect), rect, samples, color);
        theme::round_corners(p, rect, theme::RADIUS_LG, theme::MAIN_BG);
    }
    let trend = samples.as_deref().and_then(|s| super::trend_label(s, unit));
    p.circle_filled(Pos2::new(rect.left() + 20.5, rect.top() + 18.0), 2.5, color);
    let label_x = rect.left() + 28.0;
    p.text(
        Pos2::new(label_x, rect.top() + 18.0),
        Align2::LEFT_CENTER,
        ellipsize(p, label, &theme::body_sm(), rect.right() - 18.0 - label_x),
        theme::body_sm(),
        theme::TEXT_MUT,
    );
    let vrect = p.text(
        Pos2::new(rect.left() + 18.0, rect.top() + 34.0),
        Align2::LEFT_TOP,
        format!("{value:.0}"),
        theme::mono_bold(24.0),
        color,
    );
    p.text(
        Pos2::new(vrect.right() + 6.0, vrect.bottom() - 4.0),
        Align2::LEFT_BOTTOM,
        unit,
        theme::body_md(),
        theme::TEXT_FAINT,
    );
    if let Some(trend) = trend {
        p.text(
            Pos2::new(rect.right() - 14.0, vrect.bottom() - 4.0),
            Align2::RIGHT_BOTTOM,
            trend,
            theme::mono(10.0),
            theme::TEXT_FAINT,
        );
    }
}

fn gauge(
    p: &egui::Painter,
    rect: Rect,
    label: &str,
    value: f64,
    unit: &str,
    fraction: f32,
    color: Color32,
) {
    // Square tile: the dial owns the upper block with the label beneath it.
    // Nothing sits beside the ring, so it grows to whatever the tile allows.
    const CAPTION_H: f32 = 30.0;
    let d = (rect.width() - 28.0).min(rect.height() - CAPTION_H - 24.0);
    let top = rect.top() + (rect.height() - d - CAPTION_H) / 2.0;
    let center = Pos2::new(rect.center().x, top + d / 2.0);
    widgets::ring_gauge(p, center, d, fraction, color);

    // Center the value/unit pair on the ring, rather than hanging the value off
    // the center line where it reads high inside the hole.
    let value = format!("{value:.0}");
    let value_font = theme::mono_bold(d * 0.3);
    let unit_font = theme::body_sm();
    let value_h = p
        .layout_no_wrap(value.clone(), value_font.clone(), color)
        .rect
        .height();
    let unit_h = p
        .layout_no_wrap(unit.to_owned(), unit_font.clone(), theme::TEXT_FAINT)
        .rect
        .height();
    let stack_top = center.y - (value_h + unit_h) / 2.0;
    p.text(
        Pos2::new(center.x, stack_top + value_h),
        Align2::CENTER_BOTTOM,
        value,
        value_font,
        color,
    );
    p.text(
        Pos2::new(center.x, stack_top + value_h),
        Align2::CENTER_TOP,
        unit,
        unit_font,
        theme::TEXT_FAINT,
    );
    p.text(
        Pos2::new(center.x, top + d + 18.0),
        Align2::CENTER_CENTER,
        ellipsize(p, label, &theme::body_md(), rect.width() - 24.0),
        theme::body_md(),
        theme::TEXT_DIM,
    );
}

fn battery(p: &egui::Painter, rect: Rect, label: &str, rows: &[BatteryRow]) {
    p.text(
        Pos2::new(rect.left() + 18.0, rect.top() + 18.0),
        Align2::LEFT_CENTER,
        ellipsize(p, label, &theme::body_sm(), rect.width() - 36.0),
        theme::body_sm(),
        theme::TEXT_MUT,
    );
    // Name column shrinks with the tile so the track and readout always fit.
    let name_w = (rect.width() * 0.36).clamp(48.0, 110.0);
    for (i, row) in rows.iter().enumerate() {
        let y = rect.top() + 42.0 + i as f32 * 18.0;
        let color = theme::battery_color(row.level, row.charging);
        p.text(
            Pos2::new(rect.left() + 18.0, y),
            Align2::LEFT_CENTER,
            ellipsize(p, &row.name, &theme::body_sm(), name_w),
            theme::body_sm(),
            theme::TEXT_DIM,
        );
        let track = Rect::from_min_max(
            Pos2::new(rect.left() + 26.0 + name_w, y - 2.5),
            Pos2::new(rect.right() - 52.0, y + 2.5),
        );
        p.rect_filled(track, theme::RADIUS_XS, theme::TRACK);
        let mut fill = track;
        fill.set_right(track.left() + track.width() * row.level as f32 / 100.0);
        p.rect_filled(fill, theme::RADIUS_XS, color);
        p.text(
            Pos2::new(rect.right() - 18.0, y),
            Align2::RIGHT_CENTER,
            format!("{}%", row.level),
            theme::mono(10.0),
            color,
        );
    }
}

/// The tile border, drawn last so nothing bleeds over it.
pub fn frame(p: &egui::Painter, rect: Rect, color: Color32) {
    p.rect_stroke(
        rect,
        theme::RADIUS_LG,
        Stroke::new(1.0, color),
        egui::StrokeKind::Middle,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::{Battery, WireDevice};

    fn sensor(id: &str, label: &str, value: f64) -> SensorView {
        SensorView {
            id: id.into(),
            label: label.into(),
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
            id: id.into(),
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
        assert!(resolve(&no_battery, &sensors, &state).is_none());
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
        assert_eq!(names, vec!["Echo 7.1 · L", "Glide Pro"]);
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
