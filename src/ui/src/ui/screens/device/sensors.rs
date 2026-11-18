// SPDX-License-Identifier: GPL-3.0-or-later

use std::borrow::Cow;
use std::collections::VecDeque;

use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::SensorType;

use super::TabCtx;
use crate::domain::models::sensors::{range, views, SensorView};
use crate::domain::state::HISTORY_LEN;
use crate::ui::components as widgets;
use crate::ui::screens::home::sparkline_points;
use crate::ui::theme;

const TILE_MIN_W: f32 = 260.0;
const TILE_H: f32 = 184.0;
const GAP: f32 = 12.0;
const SPARK_H: f32 = 44.0;

fn tile_columns(avail: f32) -> usize {
    (((avail + GAP) / (TILE_MIN_W + GAP)).floor() as usize).max(1)
}

fn tile_width(avail: f32, cols: usize) -> f32 {
    ((avail - GAP * (cols - 1) as f32) / cols as f32).max(0.0)
}

fn format_number(value: f64) -> String {
    if value.abs() >= 100.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn kind_label(kind: &SensorType) -> Cow<'static, str> {
    match kind {
        SensorType::Temperature => t!("devtabs.sensor_kind_temperature"),
        SensorType::Load => t!("devtabs.sensor_kind_load"),
        SensorType::Memory => t!("devtabs.sensor_kind_memory"),
        SensorType::Frequency => t!("devtabs.sensor_kind_frequency"),
        SensorType::Uptime => t!("devtabs.sensor_kind_uptime"),
        SensorType::FanSpeed => t!("devtabs.sensor_kind_fan_speed"),
        SensorType::FanDuty => t!("devtabs.sensor_kind_fan_duty"),
    }
}

fn reading(value: f32, unit: &str) -> String {
    format!("{} {unit}", format_number(value as f64))
}

fn tile(ui: &mut egui::Ui, s: &SensorView, history: Option<&VecDeque<f32>>, width: f32) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, TILE_H), Sense::hover());
    let p = ui.painter();
    theme::paint_surface(p, rect, theme::RADIUS_LG, theme::CARD_BG, theme::BORDER);
    let inner = rect.shrink2(Vec2::new(18.0, 16.0));

    let head_y = inner.top() + 12.0;
    let tag = widgets::paint_tag(
        p,
        Pos2::new(inner.right(), head_y),
        &kind_label(&s.sensor_type),
        widgets::TagTone::Neutral,
    );
    let name = widgets::truncate_to_width(&s.label, tag.left() - inner.left() - 8.0, |t| {
        p.layout_no_wrap(t.to_owned(), theme::body_md(), theme::TEXT_DIM)
            .size()
            .x
    });
    let truncated = name != s.label;
    p.text(
        Pos2::new(inner.left(), head_y),
        Align2::LEFT_CENTER,
        name,
        theme::body_md(),
        theme::TEXT_DIM,
    );

    let value = p.text(
        Pos2::new(inner.left(), inner.top() + 34.0),
        Align2::LEFT_TOP,
        format_number(s.value),
        theme::semibold(34.0),
        theme::TEXT,
    );
    p.text(
        Pos2::new(value.right() + 4.0, value.bottom() - 6.0),
        Align2::LEFT_BOTTOM,
        s.unit,
        theme::body(14.0),
        theme::TEXT_MUT,
    );

    let samples: Vec<f32> = history
        .map(|h| h.iter().copied().collect())
        .unwrap_or_default();
    let spark = Rect::from_min_size(
        Pos2::new(inner.left(), value.bottom() + 10.0),
        Vec2::new(inner.width(), SPARK_H),
    );
    let pts = sparkline_points(spark, &samples);
    if let Some(last) = pts.last().copied() {
        widgets::fill_under_line(p, &pts, spark.bottom(), theme::a(theme::CYAN, 0.12));
        p.add(egui::Shape::line(pts, Stroke::new(1.6, theme::CYAN)));
        p.circle_filled(last, 3.0, theme::CYAN);
    }

    let now = s.value as f32;
    let (lo, hi) = range(samples).unwrap_or((now, now));
    p.text(
        inner.left_bottom(),
        Align2::LEFT_BOTTOM,
        t!("devtabs.sensor_min", v = reading(lo, s.unit)),
        theme::body_sm(),
        theme::TEXT_DIM,
    );
    p.text(
        inner.right_bottom(),
        Align2::RIGHT_BOTTOM,
        t!("devtabs.sensor_max", v = reading(hi, s.unit)),
        theme::body_sm(),
        theme::TEXT_DIM,
    );

    if truncated {
        resp.on_hover_text(&s.label);
    }
}

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx) {
    let sensors: Vec<SensorView> = views(ctx.dev).collect();
    if sensors.is_empty() {
        widgets::empty_state(
            ui,
            &t!("devtabs.sensors_empty"),
            Some(&t!("devtabs.sensors_empty_desc")),
        );
        return;
    }

    egui::Sides::new().show(
        ui,
        |ui| {
            ui.label(
                egui::RichText::new(t!("devtabs.sensors_title"))
                    .font(theme::heading())
                    .color(theme::TEXT),
            );
        },
        |ui| {
            ui.label(
                egui::RichText::new(t!(
                    "devtabs.sensors_summary",
                    n = sensors.len(),
                    secs = HISTORY_LEN
                ))
                .font(theme::body_md())
                .color(theme::TEXT_DIM),
            );
        },
    );
    ui.add_space(theme::SPACE_6);

    let avail = ui.available_width();
    let cols = tile_columns(avail);
    let width = tile_width(avail, cols);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(GAP);
        for row in sensors.chunks(cols) {
            ui.horizontal(|ui| {
                for s in row {
                    tile(ui, s, ctx.sensor_history.get(&s.id), width);
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::topic_store::TopicStore;
    use crate::ui::screens::device::{empty_led_colors, empty_plugin_assets};
    use halod_shared::types::{DeviceCapability, DeviceType, Sensor, SensorUnit, WireDevice};
    use proptest::prelude::*;
    use std::collections::HashMap;

    fn texts(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(t) => out.push(t.galley.text().to_owned()),
            egui::Shape::Vec(v) => v.iter().for_each(|s| texts(s, out)),
            _ => {}
        }
    }

    fn render(dev: &WireDevice, history: &HashMap<String, VecDeque<f32>>) -> Vec<String> {
        let state = TopicStore::default();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = egui::Context::default();
        crate::ui::theme::install_fonts(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 700.0))),
            ..Default::default()
        };
        let out = crate::ui::run_headless(&ctx, input, |ui| {
            let tab = TabCtx {
                state: &state,
                dev,
                cmd: &tx,
                time: 0.0,
                debug: None,
                lcd_images: &[],
                lcd_preview: None,
                lcd_upload: None,
                lcd_upload_terminal: None,
                lcd_template: None,
                lcd_editor_render: None,
                led_colors: empty_led_colors(),
                write_rate_history: None,
                plugin_assets: empty_plugin_assets(),
                sensor_history: history,
            };
            show(ui, &tab);
        });
        let mut found = Vec::new();
        for clipped in &out.shapes {
            texts(&clipped.shape, &mut found);
        }
        found
    }

    fn device(sensors: Vec<Sensor>) -> WireDevice {
        WireDevice {
            name: "Board".into(),
            device_type: DeviceType::Motherboard,
            capabilities: vec![DeviceCapability::Sensors(sensors)],
            ..Default::default()
        }
    }

    fn temp(id: &str, name: &str, value: f64) -> Sensor {
        Sensor {
            id: id.into(),
            name: name.into(),
            value,
            unit: SensorUnit::Celsius,
            sensor_type: SensorType::Temperature,
        }
    }

    #[test]
    fn tiles_show_value_and_history_range() {
        let dev = device(vec![temp("cpu", "CPU", 46.5)]);
        let history = HashMap::from([("cpu".to_string(), VecDeque::from([41.0, 52.3, 46.5]))]);
        let found = render(&dev, &history);
        for want in ["CPU", "46.5", "Min 41.0 °C", "Max 52.3 °C", "Temperature"] {
            assert!(
                found.iter().any(|t| t == want),
                "missing {want:?} in {found:?}"
            );
        }
    }

    #[test]
    fn range_falls_back_to_current_value_without_history() {
        let dev = device(vec![temp("pch", "PCH", 52.0)]);
        let found = render(&dev, &HashMap::new());
        assert!(found.iter().any(|t| t == "Min 52.0 °C"), "{found:?}");
        assert!(found.iter().any(|t| t == "Max 52.0 °C"), "{found:?}");
    }

    #[test]
    fn no_readings_shows_empty_state() {
        let found = render(&device(vec![]), &HashMap::new());
        assert!(found.iter().any(|t| t == "No readings yet"), "{found:?}");
    }

    #[test]
    fn large_values_drop_the_decimal() {
        assert_eq!(format_number(2100.4), "2100");
        assert_eq!(format_number(-120.0), "-120");
        assert_eq!(format_number(46.54), "46.5");
    }

    #[test]
    fn narrow_width_still_gets_one_column() {
        assert_eq!(tile_columns(100.0), 1);
        assert_eq!(tile_columns(TILE_MIN_W * 2.0 + GAP), 2);
    }

    proptest! {
        #[test]
        fn tiles_fill_the_row_without_overflow(avail in 0.0f32..4000.0) {
            let cols = tile_columns(avail);
            let w = tile_width(avail, cols);
            prop_assert!(cols >= 1);
            prop_assert!(cols as f32 * w + (cols - 1) as f32 * GAP <= avail.max(w) + 0.01);
            if avail >= TILE_MIN_W {
                prop_assert!(w >= TILE_MIN_W - 0.01);
            }
        }
    }
}
