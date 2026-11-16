// SPDX-License-Identifier: GPL-3.0-or-later
//! Cooling overview — temperature sensors across the top, then a 2-column grid
//! of cooler cards (rotating fan icon, preset pills, curve sensor, mini curve
//! preview) for every fan/pump device.

use crate::ui::components as widgets;

use crate::domain::topic_store::TopicStore;
use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{
    DeviceCapability, FanCurveStatus, Sensor, SensorType, WireDevice, WireFanCurve, WirePresetCurve,
};

use crate::domain::models::device as model;
use crate::domain::state::Page;
use crate::runtime::ipc::CommandTx;
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

fn curve_points_command(
    device_id: String,
    channel_id: String,
    points: Vec<[f32; 2]>,
    sensor_id: Option<String>,
) -> halod_shared::commands::DaemonCommand {
    halod_shared::commands::DaemonCommand::SetCoolingCurvePoints {
        device_id,
        channel_id,
        points,
        sensor_id,
    }
}

fn curve_preset_command(
    device_id: String,
    channel_id: String,
    preset: String,
    sensor_id: Option<String>,
) -> halod_shared::commands::DaemonCommand {
    halod_shared::commands::DaemonCommand::SetCoolingCurvePreset {
        device_id,
        channel_id,
        preset,
        sensor_id,
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Everything the Cooling page owns across frames.
#[derive(Default)]
pub struct CoolingUi {
    show_disabled: bool,
}

pub fn show(
    ui: &mut egui::Ui,
    state: &TopicStore,
    cmd: &CommandTx,
    page: &mut Page,
    cooling: &mut CoolingUi,
) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            widgets::page_frame(ui, |ui| {
                let mut coolers: Vec<(&WireDevice, Option<&halod_shared::types::CoolingChannel>)> =
                    state
                        .devices
                        .iter()
                        .filter(|d| !model::is_hidden(d))
                        .flat_map(|device| match device.cooling() {
                            Some(cooling_cap) => cooling_cap
                                .channels
                                .iter()
                                .filter(|channel| {
                                    cooling.show_disabled
                                        || channel.visibility
                                            == halod_shared::types::VisibilityState::Visible
                                })
                                .map(|channel| (device, Some(channel)))
                                .collect::<Vec<_>>(),
                            None => Vec::new(),
                        })
                        .collect();
                coolers.sort_by_key(|(device, channel)| {
                    let channel_id = channel.map(|c| c.id.as_str()).unwrap_or("default");
                    let status = state
                        .cooling
                        .fan_curves
                        .iter()
                        .find(|c| c.device_id == device.id && c.channel_id == channel_id)
                        .map(|c| &c.status);
                    is_disconnected(*channel, status)
                });
                let devices = coolers
                    .iter()
                    .map(|(d, _)| d.id.as_str())
                    .collect::<std::collections::HashSet<_>>()
                    .len();

                let title = widgets::page_title(
                    ui,
                    &t!("cooling.title"),
                    &t!(
                        "cooling.subtitle",
                        channels = coolers.len(),
                        devices = devices
                    ),
                    |ui| {
                        if widgets::checkbox(
                            ui,
                            &t!("cooling.show_disabled_channels"),
                            cooling.show_disabled,
                        )
                        .clicked()
                        {
                            cooling.show_disabled = !cooling.show_disabled;
                        }
                    },
                );
                crate::domain::tour::anchor(
                    ui.ctx(),
                    crate::domain::tour::AnchorId::CoolingCurve,
                    title,
                );
                ui.add_space(theme::SPACE_1);
                cooler_grid(ui, state, cmd, &coolers, page);
            });
        });
}

// ── Cooler grid (2-col) ───────────────────────────────────────────────────────

const GRID_GAP: f32 = 16.0;

fn cooler_grid(
    ui: &mut egui::Ui,
    state: &TopicStore,
    cmd: &CommandTx,
    coolers: &[(&WireDevice, Option<&halod_shared::types::CoolingChannel>)],
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
            ui.add_space(theme::SPACE_8);
        }
        let col_w = (ui.available_width() - GRID_GAP) / 2.0;
        widgets::split_columns(ui, col_w, GRID_GAP, |left, right| {
            for (col, dev) in [left, right].into_iter().zip(pair) {
                cooler_card(col, dev.0, dev.1, state, cmd, &sensors, page);
            }
        });
    }
}

fn cooler_card(
    ui: &mut egui::Ui,
    dev: &WireDevice,
    channel: Option<&halod_shared::types::CoolingChannel>,
    state: &TopicStore,
    cmd: &CommandTx,
    sensors: &[(String, Sensor)],
    page: &mut Page,
) {
    let rpm = channel
        .and_then(|channel| channel.rpm)
        .unwrap_or_else(|| rpm_for(dev));
    let channel_id = channel
        .map(|channel| channel.id.as_str())
        .unwrap_or("default");
    let curve = state
        .cooling
        .fan_curves
        .iter()
        .find(|c| c.device_id == dev.id && c.channel_id == channel_id);
    let sensor_id = curve.and_then(|c| c.sensor_id.clone());
    let live = sensor_id
        .as_deref()
        .and_then(|sid| sensors.iter().find(|(_, s)| s.id == sid));
    let sensor_temp = live.map(|(_, s)| s.value as f32);
    let sensor_name = live
        .map(|(_, s)| s.name.clone())
        .unwrap_or_else(|| "-".into());

    let disconnected = is_disconnected(channel, curve.map(|c| &c.status));
    let disabled =
        channel.is_some_and(|c| c.visibility != halod_shared::types::VisibilityState::Visible);
    if disconnected || disabled {
        ui.multiply_opacity(IDLE_OPACITY);
    }
    let (title, subtitle) = card_titles(dev, channel);
    let duty = channel.and_then(|channel| channel.duty);

    widgets::card_with_margin(ui, egui::Margin::same(18), |ui| {
        ui.set_width(ui.available_width());
        if disconnected {
            ui.disable();
        }
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        egui::RichText::new(&title)
                            .font(theme::semibold(16.0))
                            .color(theme::TEXT),
                    );
                    ui.label(
                        egui::RichText::new(&subtitle)
                            .font(theme::body_sm())
                            .color(theme::TEXT_DIM),
                    );
                });
            },
            |ui| {
                ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        egui::RichText::new(rpm.to_string())
                            .font(theme::semibold(26.0))
                            .color(theme::TEXT),
                    );
                    let caption = match duty {
                        Some(duty) => t!("cooling.rpm_duty", duty = duty).to_string(),
                        None => "rpm".to_owned(),
                    };
                    ui.label(
                        egui::RichText::new(caption)
                            .font(theme::body_sm())
                            .color(theme::TEXT_MUT),
                    );
                });
            },
        );

        if disconnected {
            ui.add_space(theme::SPACE_6);
            widgets::Banner::neutral(&t!("cooling.status_disconnected")).show(ui);
        } else if let Some(warning) = curve.map(|c| &c.status).and_then(curve_status_text) {
            ui.add_space(theme::SPACE_6);
            widgets::Banner::warn(&warning).show(ui);
        }
        ui.add_space(theme::SPACE_7);

        if !state.cooling.preset_curves.is_empty() {
            let names: Vec<String> = state
                .cooling
                .preset_curves
                .iter()
                .map(preset_display_name)
                .collect();
            let labels: Vec<&str> = names.iter().map(String::as_str).collect();
            let mut selected = curve
                .map(|c| c.points.as_slice())
                .and_then(|pts| matching_preset(&state.cooling.preset_curves, pts))
                .and_then(|m| {
                    state
                        .cooling
                        .preset_curves
                        .iter()
                        .position(|p| p.id == m.id)
                })
                .unwrap_or(usize::MAX);
            if widgets::segmented(ui, &labels, &mut selected, false) {
                crate::runtime::ipc::send(
                    cmd,
                    curve_preset_command(
                        dev.id.clone(),
                        channel_id.to_string(),
                        state.cooling.preset_curves[selected].id.clone(),
                        sensor_id.clone(),
                    ),
                );
            }
            ui.add_space(theme::SPACE_7);
        }
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), 32.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = theme::SPACE_4;
                ui.label(
                    egui::RichText::new(t!("cooling.sensor"))
                        .font(theme::body_md())
                        .color(theme::TEXT_DIM),
                );
                let current = live
                    .map(|(d, s)| format!("{d} · {}", s.name))
                    .unwrap_or_else(|| t!("cooling.no_sensor").to_string());
                let mut pick: Option<Option<String>> = None;
                // Channel in the salt so a device's selectors don't alias.
                egui::ComboBox::from_id_salt(format!("cool_sensor_{}_{}", dev.id, channel_id))
                    .selected_text(current)
                    .icon(widgets::select_chevron)
                    .width(210.0)
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
                    crate::runtime::ipc::send(
                        cmd,
                        curve_points_command(
                            dev.id.clone(),
                            channel_id.to_string(),
                            curve
                                .map(|c| c.points.clone())
                                .unwrap_or_else(default_curve),
                            sel,
                        ),
                    );
                }
            },
        );
        ui.add_space(theme::SPACE_7);

        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, w * 170.0 / 520.0), Sense::hover());
        curve_preview(ui.painter(), rect, curve, sensor_temp, theme::CYAN);

        ui.add_space(theme::SPACE_6);
        ui.horizontal(|ui| {
            let summary = match (sensor_temp, curve) {
                (Some(temp), Some(curve)) if curve.points.len() >= 2 => t!(
                    "cooling.now_summary",
                    sensor = &sensor_name,
                    temp = format!("{temp:.0}"),
                    duty = format!("{:.0}", halod_shared::curve::duty_at(&curve.points, temp))
                )
                .to_string(),
                _ => String::new(),
            };
            ui.label(
                egui::RichText::new(summary)
                    .font(theme::body_md())
                    .color(theme::TEXT_DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::text_link(ui, &t!("cooling.open_device")).clicked() {
                    crate::ui::screens::device::request_cooling_tab(ui.ctx());
                    *page = Page::Device(dev.id.clone());
                }
            });
        });
    });
}

/// Card heading: the channel name over its device, or the device over its
/// type when the channel carries the device's own name.
fn card_titles(
    dev: &WireDevice,
    channel: Option<&halod_shared::types::CoolingChannel>,
) -> (String, String) {
    match channel {
        Some(channel) if channel.name.trim() != dev.name.trim() => {
            (channel.name.clone(), dev.name.clone())
        }
        _ => (dev.name.clone(), model::type_label(dev).to_string()),
    }
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
    let grid = Stroke::new(1.0, theme::STAGE_GRID);
    for i in 1..5 {
        p.vline(
            rect.left() + rect.width() * i as f32 / 5.0,
            rect.y_range(),
            grid,
        );
    }
    for i in 1..4 {
        p.hline(
            rect.x_range(),
            rect.top() + rect.height() * i as f32 / 4.0,
            grid,
        );
    }
    let label = |pos: Pos2, align: Align2, text: &str| {
        p.text(pos, align, text, theme::caption(), theme::TEXT_FAINT);
    };
    label(
        rect.left_top() + Vec2::new(10.0, 10.0),
        Align2::LEFT_TOP,
        "100%",
    );
    label(
        rect.left_bottom() + Vec2::new(10.0, -8.0),
        Align2::LEFT_BOTTOM,
        "20 °C",
    );
    label(
        rect.right_bottom() + Vec2::new(-10.0, -8.0),
        Align2::RIGHT_BOTTOM,
        "100 °C",
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
            rect.top() + ny * rect.height(),
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

    let clipped = p.with_clip_rect(rect);
    widgets::fill_under_line(&clipped, &line, rect.bottom(), a(color, 0.10));
    clipped.add(egui::Shape::line(line, Stroke::new(2.0, color)));
    for &[t, d] in points {
        let pos = map(t, d);
        clipped.circle_filled(pos, 5.0, theme::INNER_BG);
        clipped.circle_stroke(pos, 5.0, Stroke::new(2.0, color));
    }

    if let Some(temp) = sensor_temp {
        let duty = halod_shared::curve::duty_at(points, temp);
        let pos = map(temp.clamp(20.0, 100.0), duty);
        clipped.extend(egui::Shape::dashed_line(
            &[
                Pos2::new(pos.x, rect.top()),
                Pos2::new(pos.x, rect.bottom()),
            ],
            Stroke::new(1.0, a(theme::TEXT, 0.35)),
            3.0,
            3.0,
        ));
        clipped.circle_filled(pos, 4.0, theme::TEXT);
    }
}

const IDLE_OPACITY: f32 = 0.45;

/// A header with no fan physically connected: either the device reports it as
/// not controllable (e.g. an empty NZXT header via `fan_type`) or the daemon's
/// presence probe found no RPM at full duty (`Disconnected`). Such cards are
/// dimmed, non-interactive, and sorted last.
pub fn is_disconnected(
    channel: Option<&halod_shared::types::CoolingChannel>,
    status: Option<&FanCurveStatus>,
) -> bool {
    channel.is_some_and(|c| !c.controllable) || matches!(status, Some(FanCurveStatus::Disconnected))
}

fn rpm_for(dev: &WireDevice) -> u32 {
    dev.capabilities
        .iter()
        .find_map(|c| match c {
            DeviceCapability::Cooling(cooling) => {
                cooling.channels.first().and_then(|channel| channel.rpm)
            }
            _ => None,
        })
        .unwrap_or(0)
}

/// Temperature sensors across all devices, paired with their device name.
/// Shared by the global cooling page and the per-device cooling tab.
pub fn temp_sensors(state: &TopicStore) -> Vec<(String, Sensor)> {
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
        // A disconnected header renders its own muted caption, not an amber warning.
        FanCurveStatus::Disconnected => None,
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
    fn disconnected_when_not_controllable_or_probe_failed() {
        use halod_shared::types::CoolingChannel;
        let controllable = CoolingChannel {
            controllable: true,
            ..Default::default()
        };
        let uncontrollable = CoolingChannel {
            controllable: false,
            ..Default::default()
        };

        assert!(is_disconnected(Some(&uncontrollable), None));
        assert!(is_disconnected(
            Some(&controllable),
            Some(&FanCurveStatus::Disconnected)
        ));
        assert!(!is_disconnected(
            Some(&controllable),
            Some(&FanCurveStatus::Ok)
        ));
        assert!(!is_disconnected(Some(&controllable), None));
        assert!(
            !is_disconnected(Some(&controllable), Some(&FanCurveStatus::NoSensor)),
            "a present, unconfigured fan must stay editable"
        );
        assert!(!is_disconnected(None, None));
    }

    #[test]
    fn disconnected_sort_key_sinks_absent_coolers_and_keeps_the_rest_in_order() {
        use halod_shared::types::CoolingChannel;
        let ctrl = CoolingChannel {
            controllable: true,
            ..Default::default()
        };
        let no_ctrl = CoolingChannel {
            controllable: false,
            ..Default::default()
        };
        let mut coolers = [
            ("a", Some(&no_ctrl), None),
            ("b", Some(&ctrl), Some(FanCurveStatus::Ok)),
            ("c", Some(&ctrl), Some(FanCurveStatus::NoSensor)),
            ("d", Some(&ctrl), Some(FanCurveStatus::Disconnected)),
            ("e", None, None),
        ];
        coolers.sort_by_key(|(_, ch, st)| is_disconnected(*ch, st.as_ref()));
        assert_eq!(
            coolers.map(|c| c.0),
            ["b", "c", "e", "a", "d"],
            "disconnected cards go last, everything else keeps its order"
        );
    }

    #[test]
    fn curve_status_text_only_silent_when_ok_or_disconnected() {
        assert_eq!(curve_status_text(&FanCurveStatus::Ok), None);
        assert_eq!(curve_status_text(&FanCurveStatus::Disconnected), None);
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
