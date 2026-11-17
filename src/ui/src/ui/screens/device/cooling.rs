// SPDX-License-Identifier: GPL-3.0-or-later
//! Cooling tab — fan readings, a fixed-speed mode, and a drag-editable
//! temp→duty curve (reused [`widgets::curve_editor`]).

use crate::ui::components as widgets;
use egui::{Align2, Pos2, Rect, Sense, Vec2};
use halod_shared::commands::{DaemonCommand, DeviceOp};
use halod_shared::types::{
    ChannelKind, CoolingChannel, DeviceCapability, FanCurveStatus, Sensor, VisibilityState,
};

use super::{editing, DeviceUi, TabCtx};
use crate::domain::tour;
use crate::domain::tour::AnchorId;
use crate::runtime::ipc;
use crate::ui::screens::cooling::preset_display_name;
use crate::ui::theme;
use halod_shared::ids::DeviceId;

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    tour::anchor(ui.ctx(), AnchorId::TabCooling, ui.max_rect());
    let all_channels = visible_cooling_channels(ctx.dev, true);
    let has_hidden = all_channels.len() > cooling_channels(ctx.dev).len();
    let channels = visible_cooling_channels(ctx.dev, st.cooling.show_hidden);
    let default_channel = channels
        .first()
        .map(|c| c.id.clone())
        .unwrap_or_else(|| "default".into());
    let selected = st.cooling.channel_id.get_or_insert(default_channel).clone();
    let selected = if channels.iter().any(|c| c.id == selected) {
        selected
    } else {
        channels
            .first()
            .map(|c| c.id.clone())
            .unwrap_or_else(|| "default".into())
    };
    st.cooling.channel_id = Some(selected.clone());
    let fan = find_fan(ctx.dev, &selected);
    let fan_id = ctx.dev.id.clone();

    // The persisted curve for this fan (if any).
    let curve = ctx
        .state
        .cooling
        .fan_curves
        .iter()
        .find(|c| c.device_id == fan_id && c.channel_id == selected);
    if !st.cooling.curve_seeded {
        st.cooling.curve = curve
            .map(|c| c.points.clone())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(crate::ui::screens::cooling::default_curve);
        st.cooling.curve_sensor = curve.and_then(|c| c.sensor_id.clone());
        st.cooling.curve_seeded = true;
    } else if !editing(st, ctx.time) {
        if let Some(c) = curve {
            if !c.points.is_empty() {
                st.cooling.curve = c.points.clone();
            }
        }
    }

    // Sensors across all devices for the curve binding.
    let sensors = crate::ui::screens::cooling::temp_sensors(ctx.state);
    let sensor_temp = st
        .cooling
        .curve_sensor
        .as_ref()
        .and_then(|sid| sensors.iter().find(|(_, s)| &s.id == sid))
        .map(|(_, s)| s.value as f32);

    if channels.len() > 1 || has_hidden {
        let label = channels
            .iter()
            .find(|c| c.id == selected)
            .map(|c| c.name.as_str())
            .unwrap_or("Cooling");
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("cooling_channel")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    for channel in &channels {
                        let hidden = channel.visibility != VisibilityState::Visible;
                        let name = if hidden {
                            format!("{} ({})", channel.name, t!("cooling.channel_off"))
                        } else {
                            channel.name.clone()
                        };
                        let resp = ui.selectable_label(selected == channel.id, name);
                        if resp.clicked() {
                            st.cooling.channel_id = Some(channel.id.clone());
                            st.cooling.curve_seeded = false;
                        }
                        resp.context_menu(|ui| {
                            ui.set_width(170.0);
                            let (label, state) = if hidden {
                                (t!("cooling.enable_channel"), VisibilityState::Visible)
                            } else {
                                (t!("cooling.disable_channel"), VisibilityState::Disabled)
                            };
                            if widgets::context_menu_item(ui, &label, theme::TEXT).clicked() {
                                ipc::send(
                                    ctx.cmd,
                                    DaemonCommand::SetChannelVisibility {
                                        device_id: ctx.dev.id.clone(),
                                        kind: ChannelKind::Cooling,
                                        channel_id: channel.id.clone(),
                                        state,
                                    },
                                );
                                ui.close();
                            }
                        });
                    }
                });
            if has_hidden {
                let label = if st.cooling.show_hidden {
                    t!("cooling.hide_disabled")
                } else {
                    t!("cooling.show_disabled")
                };
                if widgets::button(
                    ui,
                    &label,
                    widgets::ButtonKind::Secondary,
                    egui::vec2(150.0, 28.0),
                )
                .clicked()
                {
                    st.cooling.show_hidden = !st.cooling.show_hidden;
                }
            }
        });
        ui.add_space(theme::SPACE_6);
    }
    let disconnected =
        crate::ui::screens::cooling::is_disconnected(fan.as_ref(), curve.map(|c| &c.status));
    if disconnected {
        ui.label(
            egui::RichText::new(t!("cooling.status_disconnected"))
                .font(theme::body_sm())
                .color(theme::TEXT_FAINT),
        );
        ui.add_space(theme::SPACE_4);
    }
    ui.add_enabled_ui(!disconnected, |ui| {
        top_row(ui, ctx, st, &fan, &sensors, sensor_temp, &selected);
        ui.add_space(theme::SPACE_8);

        let curve_title = t!("cooling.fan_curve");
        curve_card(
            ui,
            ctx,
            st,
            &fan_id,
            &selected,
            curve.map(|c| c.status.clone()),
            sensor_temp,
            &curve_title,
        );
    });
}

fn top_row(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    fan: &Option<CoolingChannel>,
    sensors: &[(String, Sensor)],
    sensor_temp: Option<f32>,
    channel_id: &str,
) {
    ui.columns(2, |cols| {
        // Curve sensor selector + live temp.
        widgets::card(&mut cols[0], |ui| {
            widgets::section_label(ui, &t!("cooling.curve_sensor"));
            ui.add_space(theme::SPACE_4);
            let mut pick: Option<Option<String>> = None;
            let sensor_combo_rect =
                Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), 28.0));
            tour::anchor(ui.ctx(), AnchorId::CoolingSensor, sensor_combo_rect);
            let options: Vec<(String, String)> = sensors
                .iter()
                .map(|(d, s)| (s.id.clone(), format!("{d} · {} ({:.0}°C)", s.name, s.value)))
                .collect();
            let current_id = st.cooling.curve_sensor.clone().unwrap_or_default();
            if let Some(id) = widgets::combo_picker_full(
                ui,
                "curve_sensor",
                &options,
                &current_id,
                Some(&t!("cooling.no_sensor")),
            ) {
                pick = Some((!id.is_empty()).then_some(id));
            }
            if let Some(sel) = pick {
                st.cooling.curve_sensor = sel;
                st.last_edit = ctx.time;
                ipc::send(
                    ctx.cmd,
                    curve_points_command(
                        ctx.dev.id.clone(),
                        channel_id.to_string(),
                        st.cooling.curve.clone(),
                        st.cooling.curve_sensor.clone(),
                    ),
                );
            }
            ui.add_space(theme::SPACE_6);
            let t = sensor_temp
                .map(|t| format!("{t:.0}"))
                .unwrap_or_else(|| "-".into());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(
                    egui::RichText::new(t)
                        .font(theme::semibold(40.0))
                        .color(theme::TEXT),
                );
                ui.label(
                    egui::RichText::new(t!("cooling.celsius_now"))
                        .font(theme::body_lg())
                        .color(theme::TEXT_MUT),
                );
            });
        });

        // Readings.
        widgets::card(&mut cols[1], |ui| {
            if let Some(f) = fan {
                widgets::value_row(
                    ui,
                    &t!("cooling.fan_speed"),
                    &t!("cooling.rpm", v = f.rpm.unwrap_or_default()),
                    theme::TEXT_BRIGHT,
                );
                widgets::value_row(
                    ui,
                    &t!("cooling.fan_duty"),
                    &t!("cooling.percent", v = f.duty.unwrap_or_default()),
                    theme::TEXT_BRIGHT,
                );
            }
        });
    });
}

fn curve_card(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    fan_id: &DeviceId,
    channel_id: &str,
    status: Option<FanCurveStatus>,
    sensor_temp: Option<f32>,
    title: &str,
) {
    widgets::card(ui, |ui| {
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.label(
                    egui::RichText::new(title)
                        .font(theme::title())
                        .color(theme::TEXT),
                );
            },
            |ui| {
                // Preset selector (applies a named curve from the daemon).
                let presets = ctx.state.cooling.preset_curves.clone();
                if !presets.is_empty() {
                    let active =
                        crate::ui::screens::cooling::matching_preset(&presets, &st.cooling.curve);
                    let names: Vec<String> = presets.iter().map(preset_display_name).collect();
                    let labels: Vec<&str> = names.iter().map(String::as_str).collect();
                    let mut idx = active
                        .and_then(|a| presets.iter().position(|p| p.id == a.id))
                        .unwrap_or(usize::MAX);
                    let seg = ui.scope(|ui| widgets::segmented(ui, &labels, &mut idx, false));
                    let pick = seg.inner.then(|| presets.get(idx)).flatten();
                    tour::anchor(ui.ctx(), AnchorId::CoolingPreset, seg.response.rect);
                    if let Some(p) = pick {
                        st.cooling.curve = p.points.clone();
                        st.last_edit = ctx.time;
                        st.queue(
                            "curve",
                            curve_preset_command(
                                fan_id.clone(),
                                channel_id.to_string(),
                                p.id.clone(),
                                st.cooling.curve_sensor.clone(),
                            ),
                            ctx.time,
                        );
                    }
                }
            },
        );
        ui.label(
            egui::RichText::new(t!("cooling.curve_hint"))
                .font(theme::body_sm())
                .color(theme::TEXT_MUT),
        );
        if let Some(warning) = status
            .as_ref()
            .and_then(crate::ui::screens::cooling::curve_status_text)
        {
            ui.add_space(theme::SPACE_3);
            ui.label(
                egui::RichText::new(warning)
                    .font(theme::body_sm())
                    .color(theme::STAT_AMBER),
            );
        }
        ui.add_space(theme::SPACE_7);

        // Plot area with axis labels.
        let (outer, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 320.0), Sense::hover());
        let plot = Rect::from_min_max(
            Pos2::new(outer.left() + 40.0, outer.top() + 8.0),
            Pos2::new(outer.right() - 6.0, outer.bottom() - 22.0),
        );
        tour::anchor(ui.ctx(), AnchorId::CoolingCurveEditor, plot);
        // Y axis (duty %) and X axis (°C) labels.
        for i in 0..=4 {
            let frac = i as f32 / 4.0;
            let y = plot.bottom() - frac * plot.height();
            ui.painter().text(
                Pos2::new(plot.left() - 8.0, y),
                Align2::RIGHT_CENTER,
                format!("{}", (frac * 100.0) as i32),
                theme::caption(),
                theme::TEXT_FAINT,
            );
            let x = plot.left() + frac * plot.width();
            ui.painter().text(
                Pos2::new(x, plot.bottom() + 12.0),
                Align2::CENTER_CENTER,
                format!("{}", 20 + (frac * 80.0) as i32),
                theme::caption(),
                theme::TEXT_FAINT,
            );
        }
        let op = sensor_temp.map(|t| [t, halod_shared::curve::duty_at(&st.cooling.curve, t)]);
        if widgets::curve_editor(
            ui,
            plot,
            &mut st.cooling.curve,
            20.0..=100.0,
            0.0..=100.0,
            theme::CYAN,
            op,
            |[t, d]| format!("{t:.0} °C · {d:.0}%"),
        ) {
            let cmd = curve_points_command(
                fan_id.clone(),
                channel_id.to_string(),
                st.cooling.curve.clone(),
                st.cooling.curve_sensor.clone(),
            );
            st.queue("curve", cmd, ctx.time);
        }
    });
}

fn curve_points_command(
    device_id: DeviceId,
    channel_id: String,
    points: Vec<[f32; 2]>,
    sensor_id: Option<String>,
) -> DaemonCommand {
    DaemonCommand::Device {
        device_id: device_id.clone(),
        op: DeviceOp::SetCoolingCurvePoints {
            channel_id,
            points,
            sensor_id,
        },
    }
}

fn curve_preset_command(
    device_id: DeviceId,
    channel_id: String,
    preset: String,
    sensor_id: Option<String>,
) -> DaemonCommand {
    DaemonCommand::Device {
        device_id: device_id.clone(),
        op: DeviceOp::SetCoolingCurvePreset {
            channel_id,
            preset,
            sensor_id,
        },
    }
}

fn cooling_channels(dev: &halod_shared::types::WireDevice) -> Vec<CoolingChannel> {
    visible_cooling_channels(dev, false)
}

/// The device's cooling channels, minus the ones the user switched off.
pub fn visible_cooling_channels(
    dev: &halod_shared::types::WireDevice,
    include_hidden: bool,
) -> Vec<CoolingChannel> {
    dev.cooling()
        .map(|cooling| {
            cooling
                .channels
                .iter()
                .filter(|channel| include_hidden || channel.visibility == VisibilityState::Visible)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn find_fan(dev: &halod_shared::types::WireDevice, channel_id: &str) -> Option<CoolingChannel> {
    dev.capabilities.iter().find_map(|c| match c {
        DeviceCapability::Cooling(cooling) => cooling
            .channels
            .iter()
            .find(|c| c.id == channel_id)
            .cloned(),
        _ => None,
    })
}
