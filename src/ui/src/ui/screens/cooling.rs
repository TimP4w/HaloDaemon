// SPDX-License-Identifier: GPL-3.0-or-later
//! Cooling overview — temperature sensors across the top, then a 2-column grid
//! of cooler cards (rotating fan icon, preset pills, curve sensor, mini curve
//! preview) for every fan/pump device.

use crate::ui::components as widgets;
use std::collections::{HashMap, VecDeque};

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{
    AppState, DeviceCapability, FanCurveStatus, Sensor, SensorType, WireDevice, WireFanCurve,
    WirePresetCurve,
};

use crate::domain::models::device as model;
use crate::domain::state::Page;
use crate::runtime::ipc::CommandTx;
use crate::ui::components::ButtonKind;
use crate::ui::theme::{self, a};

pub(crate) fn preset_display_name(preset: &WirePresetCurve) -> String {
    t!(format!("cooling.preset_name.{}", preset.id)).to_string()
}

/// The preset whose points match `curve`, if any — used to preselect the active
/// preset in selectors instead of a generic placeholder.
pub(crate) fn matching_preset<'a>(
    presets: &'a [WirePresetCurve],
    curve: &[[f32; 2]],
) -> Option<&'a WirePresetCurve> {
    presets.iter().find(|p| {
        p.points.len() == curve.len()
            && p.points
                .iter()
                .zip(curve)
                .all(|(a, b)| (a[0] - b[0]).abs() < 0.01 && (a[1] - b[1]).abs() < 0.01)
    })
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    cmd: &CommandTx,
    history: &HashMap<String, VecDeque<f32>>,
    time: f64,
    page: &mut Page,
) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            widgets::page_frame(ui, |ui| {
                let coolers: Vec<&WireDevice> = state
                    .devices
                    .iter()
                    .filter(|d| has_fan_or_pump(d) && !model::is_hidden(d))
                    .collect();

                ui.label(
                    egui::RichText::new(t!("cooling.title"))
                        .font(theme::bold(22.0))
                        .color(theme::TEXT),
                );
                ui.add_space(3.0);
                ui.label(
                    egui::RichText::new(t!("cooling.subtitle", count = coolers.len()))
                        .font(theme::body(12.0))
                        .color(theme::TEXT_MUT),
                );
                ui.add_space(22.0);

                if !crate::domain::models::sensors::sensors(state, false).is_empty() {
                    crate::ui::screens::home::sensors_grid(ui, state, cmd, false, history, 3);
                    ui.add_space(22.0);
                }
                cooler_grid(ui, state, cmd, &coolers, time, page);
            });
        });
}

// ── Cooler grid (2-col) ───────────────────────────────────────────────────────

fn cooler_grid(
    ui: &mut egui::Ui,
    state: &AppState,
    cmd: &CommandTx,
    coolers: &[&WireDevice],
    time: f64,
    page: &mut Page,
) {
    if coolers.is_empty() {
        widgets::empty_state(
            ui,
            &t!("cooling.empty_title"),
            Some(&t!("cooling.empty_subtitle")),
        );
        return;
    }

    let sensors = temp_sensors(state);
    for (row, pair) in coolers.chunks(2).enumerate() {
        if row > 0 {
            ui.add_space(16.0);
        }
        ui.columns(2, |cols| {
            for (i, dev) in pair.iter().enumerate() {
                if row == 0 && i == 0 {
                    let resp = cols[i].scope(|ui| {
                        cooler_card(ui, dev, state, cmd, &sensors, time, page);
                    });
                    crate::domain::tour::anchor(
                        cols[i].ctx(),
                        crate::domain::tour::AnchorId::CoolingCurve,
                        resp.response.rect,
                    );
                } else {
                    cooler_card(&mut cols[i], dev, state, cmd, &sensors, time, page);
                }
            }
        });
    }
}

fn cooler_card(
    ui: &mut egui::Ui,
    dev: &WireDevice,
    state: &AppState,
    cmd: &CommandTx,
    sensors: &[(String, Sensor)],
    time: f64,
    page: &mut Page,
) {
    let rpm = rpm_for(dev);
    let dev_color = theme::device_color(dev);
    let curve = state.cooling.fan_curves.iter().find(|c| c.fan_id == dev.id);
    let sensor_id = curve.and_then(|c| c.sensor_id.clone());
    let live = sensor_id
        .as_deref()
        .and_then(|sid| sensors.iter().find(|(_, s)| s.id == sid));
    let sensor_temp = live.map(|(_, s)| s.value as f32);
    let sensor_name = live
        .map(|(_, s)| s.name.clone())
        .unwrap_or_else(|| "-".into());

    widgets::card(ui, |ui| {
        // ── Header: rotating fan + name/type + RPM ────────────────────────────
        ui.horizontal(|ui| {
            let (icon, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
            fan_icon(ui.painter(), icon.center(), 18.0, time, rpm, dev_color);
            ui.add_space(12.0);
            let col_w = ui.available_width();
            ui.vertical(|ui| {
                ui.set_width(col_w);
                ui.spacing_mut().item_spacing.y = 2.0;
                egui::Sides::new().show(
                    ui,
                    |ui| {
                        ui.label(
                            egui::RichText::new(&dev.name)
                                .font(theme::semibold(14.0))
                                .color(theme::TEXT),
                        );
                    },
                    |ui| {
                        ui.label(
                            egui::RichText::new(format!("{rpm}"))
                                .font(theme::mono_bold(18.0))
                                .color(dev_color),
                        );
                    },
                );
                egui::Sides::new().show(
                    ui,
                    |ui| {
                        ui.label(
                            egui::RichText::new(model::type_label(dev))
                                .font(theme::body(11.0))
                                .color(theme::TEXT_MUT),
                        );
                    },
                    |ui| {
                        ui.label(
                            egui::RichText::new("RPM")
                                .font(theme::body(10.0))
                                .color(theme::TEXT_FAINT),
                        );
                    },
                );
            });
        });
        if rpm > 0 {
            ui.ctx().request_repaint();
        }

        ui.add_space(13.0);

        // ── Preset pills ──────────────────────────────────────────────────────
        if !state.cooling.preset_curves.is_empty() {
            // Highlight the preset whose curve matches this fan's current curve.
            let active = curve
                .map(|c| c.points.as_slice())
                .and_then(|pts| matching_preset(&state.cooling.preset_curves, pts))
                .map(|p| p.id.clone());
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(6.0);
                for preset in &state.cooling.preset_curves {
                    if widgets::pill(
                        ui,
                        &preset_display_name(preset),
                        active.as_deref() == Some(&preset.id),
                    ) {
                        crate::domain::actions::cooling::set_fan_curve_preset(
                            cmd,
                            &dev.id,
                            &preset.id,
                            sensor_id.clone(),
                        );
                    }
                }
            });
            ui.add_space(11.0);
        }

        // ── Curve sensor selector ─────────────────────────────────────────────
        ui.horizontal(|ui| {
            widgets::caps_label(ui, &t!("cooling.curve_sensor"));
            ui.add_space(6.0);
            let current = live
                .map(|(d, s)| format!("{d} · {} ({:.0}°C)", s.name, s.value))
                .unwrap_or_else(|| t!("cooling.no_sensor").to_string());
            let mut pick: Option<Option<String>> = None;
            egui::ComboBox::from_id_salt(format!("cool_sensor_{}", dev.id))
                .selected_text(current)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(sensor_id.is_none(), t!("cooling.no_sensor"))
                        .clicked()
                    {
                        pick = Some(None);
                    }
                    for (d, s) in sensors {
                        let on = sensor_id.as_deref() == Some(s.id.as_str());
                        let label = format!("{d} · {} ({:.0}°C)", s.name, s.value);
                        if ui.selectable_label(on, label).clicked() {
                            pick = Some(Some(s.id.clone()));
                        }
                    }
                });
            if let Some(sel) = pick {
                crate::domain::actions::cooling::set_fan_curve_points(
                    cmd,
                    &dev.id,
                    curve
                        .map(|c| c.points.clone())
                        .unwrap_or_else(default_curve),
                    sel,
                );
            }
        });

        ui.add_space(11.0);

        // ── Mini curve preview ────────────────────────────────────────────────
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 158.0), Sense::hover());
        curve_preview(ui.painter(), rect, curve, sensor_temp, dev_color);
        ui.painter().text(
            Pos2::new(rect.left() + 10.0, rect.top() + 8.0),
            Align2::LEFT_TOP,
            t!("cooling.rpm_vs", sensor = &sensor_name),
            theme::mono(9.5),
            theme::TEXT_FAINT2,
        );

        // ── Curve status warning (e.g. no sensor, stalled fan) ────────────────
        if let Some(warning) = curve.map(|c| &c.status).and_then(curve_status_text) {
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(warning)
                    .font(theme::body(11.0))
                    .color(theme::STAT_AMBER),
            );
        }

        ui.add_space(13.0);

        // ── Footer: sensor temp + open device ─────────────────────────────────
        ui.horizontal(|ui| {
            if let Some(t) = sensor_temp {
                ui.label(
                    egui::RichText::new(&sensor_name)
                        .font(theme::body(11.5))
                        .color(theme::TEXT_DIM),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!("{t:.0}°C"))
                        .font(theme::mono_semibold(11.5))
                        .color(dev_color),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::button(
                    ui,
                    &t!("cooling.open_device"),
                    ButtonKind::Ghost,
                    Vec2::new(118.0, 30.0),
                )
                .clicked()
                {
                    crate::ui::screens::device::request_cooling_tab(ui.ctx());
                    *page = Page::Device(dev.id.clone());
                }
            });
        });
    });
}

// ── Curve preview painter (read-only) ─────────────────────────────────────────

fn curve_preview(
    p: &egui::Painter,
    rect: Rect,
    curve: Option<&WireFanCurve>,
    sensor_temp: Option<f32>,
    color: Color32,
) {
    p.rect_filled(rect, 8.0, theme::INNER_BG);
    p.rect_stroke(
        rect,
        8.0,
        Stroke::new(1.0, theme::BORDER_INNER),
        egui::StrokeKind::Middle,
    );

    let points = match curve.map(|c| c.points.as_slice()) {
        Some(pts) if pts.len() >= 2 => pts,
        _ => return,
    };

    let map = |temp: f32, duty: f32| {
        let nx = ((temp - 20.0) / 80.0).clamp(0.0, 1.0);
        let ny = 1.0 - (duty / 100.0).clamp(0.0, 1.0);
        Pos2::new(
            rect.left() + nx * rect.width(),
            rect.top() + 8.0 + ny * (rect.height() - 14.0),
        )
    };

    // Extend flat to both edges of the [20..100] span.
    let mut ext: Vec<[f32; 2]> = Vec::with_capacity(points.len() + 2);
    if points[0][0] > 20.0 {
        ext.push([20.0, points[0][1]]);
    }
    ext.extend_from_slice(points);
    if points[points.len() - 1][0] < 100.0 {
        ext.push([100.0, points[points.len() - 1][1]]);
    }
    let line: Vec<Pos2> = ext.iter().map(|&[t, d]| map(t, d)).collect();

    widgets::fill_under_line(p, &line, rect.bottom(), a(color, 0.13));
    p.add(egui::Shape::line(line, Stroke::new(2.0, a(color, 0.85))));

    if let Some(temp) = sensor_temp {
        let duty = halod_shared::curve::duty_at(points, temp);
        let pos = map(temp.clamp(20.0, 100.0), duty);
        p.line_segment(
            [Pos2::new(pos.x, rect.bottom()), pos],
            Stroke::new(1.0, a(color, 0.35)),
        );
        p.circle_filled(pos, 4.0, color);
        p.circle_stroke(pos, 4.0, Stroke::new(1.5, theme::INNER_BG));
    }
}

// ── Rotating fan icon ─────────────────────────────────────────────────────────

/// A 4-blade fan at `center`, radius `r`, spinning at a speed scaled from `rpm`.
fn fan_icon(p: &egui::Painter, center: Pos2, r: f32, time: f64, rpm: u32, color: Color32) {
    let rps = (rpm as f64 / 1000.0).min(2.5);
    let angle = (time * rps * std::f64::consts::TAU) as f32;
    let blade_color = a(color, if rpm == 0 { 0.30 } else { 0.85 });
    let inner = r * 0.20;
    let outer = r * 0.88;
    let arc = std::f32::consts::TAU / 4.0 * 0.62;
    const SEG: u32 = 8;

    for b in 0..4u32 {
        let start = angle + b as f32 * std::f32::consts::FRAC_PI_2;
        let mut mesh = egui::Mesh::default();
        for j in 0..=SEG {
            let ang = start + (j as f32 / SEG as f32) * arc;
            let (s, c) = ang.sin_cos();
            mesh.colored_vertex(center + Vec2::new(c * inner, s * inner), blade_color);
            mesh.colored_vertex(center + Vec2::new(c * outer, s * outer), blade_color);
        }
        for j in 0..SEG {
            let k = j * 2;
            mesh.indices
                .extend_from_slice(&[k, k + 1, k + 2, k + 1, k + 2, k + 3]);
        }
        p.add(egui::Shape::mesh(mesh));
    }
    p.circle_filled(
        center,
        inner + 1.5,
        a(color, if rpm == 0 { 0.20 } else { 0.55 }),
    );
    p.circle_stroke(center, outer + 1.5, Stroke::new(1.5, a(color, 0.18)));
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn has_fan_or_pump(dev: &WireDevice) -> bool {
    dev.capabilities
        .iter()
        .any(|c| matches!(c, DeviceCapability::Fan(_) | DeviceCapability::Pump(_)))
}

fn rpm_for(dev: &WireDevice) -> u32 {
    dev.capabilities
        .iter()
        .find_map(|c| match c {
            DeviceCapability::Fan(f) => Some(f.rpm),
            DeviceCapability::Pump(p) => Some(p.rpm),
            _ => None,
        })
        .unwrap_or(0)
}

/// Temperature sensors across all devices, paired with their device name.
/// Shared by the global cooling page and the per-device cooling tab.
pub fn temp_sensors(state: &AppState) -> Vec<(String, Sensor)> {
    state
        .devices
        .iter()
        .flat_map(|d| {
            let name = d.name.as_str();
            d.capabilities.iter().filter_map(move |c| match c {
                DeviceCapability::Sensors(ss) => Some((name, ss)),
                _ => None,
            })
        })
        .flat_map(|(name, ss)| {
            ss.iter()
                .filter(|s| s.sensor_type == SensorType::Temperature)
                .map(move |s| (name.to_owned(), s.clone()))
        })
        .collect()
}

/// Default temp→duty curve, shared by both cooling pages.
pub fn default_curve() -> Vec<[f32; 2]> {
    vec![[20.0, 20.0], [40.0, 35.0], [60.0, 60.0], [80.0, 100.0]]
}

/// Human-readable warning for a non-OK fan-curve status, or `None` when the
/// curve is healthy. Shared by the cooling page and the per-device cooling tab
/// so both surface the same remediation hint instead of a raw enum name.
pub fn curve_status_text(status: &FanCurveStatus) -> Option<std::borrow::Cow<'static, str>> {
    match status {
        FanCurveStatus::Ok => None,
        FanCurveStatus::NoSensor => Some(t!("cooling.status_no_sensor")),
        FanCurveStatus::SensorMalfunction => Some(t!("cooling.status_sensor_malfunction")),
        FanCurveStatus::WriteError(e) if e.contains("Permission denied") => {
            Some(t!("cooling.status_permission_denied"))
        }
        FanCurveStatus::WriteError(_) => Some(t!("cooling.status_write_error")),
        FanCurveStatus::FanStalled => Some(t!("cooling.status_fan_stalled")),
        FanCurveStatus::NoDevice => Some(t!("cooling.status_no_device")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_daemon_preset_id_is_translated() {
        for id in [
            "balanced",
            "silent",
            "performance",
            "full_speed",
            "fifty_percent",
        ] {
            let preset = WirePresetCurve {
                id: id.into(),
                name: String::new(),
                points: Vec::new(),
            };
            assert_ne!(
                preset_display_name(&preset),
                format!("cooling.preset_name.{id}"),
                "missing preset translation for {id:?}"
            );
        }
    }

    #[test]
    fn matching_preset_finds_exact_curve_and_ignores_mismatch() {
        let presets = vec![
            WirePresetCurve {
                id: "silent".into(),
                name: String::new(),
                points: vec![[30.0, 20.0], [60.0, 50.0]],
            },
            WirePresetCurve {
                id: "performance".into(),
                name: String::new(),
                points: vec![[30.0, 40.0], [60.0, 100.0]],
            },
        ];
        assert_eq!(
            matching_preset(&presets, &[[30.0, 40.0], [60.0, 100.0]]).map(|p| p.id.as_str()),
            Some("performance"),
        );
        // Different point count or values → no match (custom curve).
        assert!(matching_preset(&presets, &[[30.0, 40.0]]).is_none());
        assert!(matching_preset(&presets, &[[30.0, 41.0], [60.0, 100.0]]).is_none());
        assert!(matching_preset(&[], &[[30.0, 40.0]]).is_none());
    }

    #[test]
    fn default_curve_is_monotonic_in_both_axes() {
        let c = default_curve();
        for w in c.windows(2) {
            assert!(w[1][0] > w[0][0]);
            assert!(w[1][1] >= w[0][1]);
        }
    }

    #[test]
    fn curve_status_text_only_silent_when_ok() {
        assert_eq!(curve_status_text(&FanCurveStatus::Ok), None);
        for s in [
            FanCurveStatus::NoSensor,
            FanCurveStatus::SensorMalfunction,
            FanCurveStatus::WriteError("boom".into()),
            FanCurveStatus::FanStalled,
            FanCurveStatus::NoDevice,
        ] {
            assert!(curve_status_text(&s).is_some(), "{s:?} should warn");
        }
    }

    #[test]
    fn curve_status_text_flags_permission_denied_specifically() {
        let denied = curve_status_text(&FanCurveStatus::WriteError(
            "Permission denied (os error 13)".into(),
        ))
        .unwrap();
        let generic = curve_status_text(&FanCurveStatus::WriteError("boom".into())).unwrap();
        assert!(denied.contains("udevadm"));
        assert_ne!(denied, generic);
    }
}
