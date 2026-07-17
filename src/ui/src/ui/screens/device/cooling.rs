// SPDX-License-Identifier: GPL-3.0-or-later
//! Cooling tab — fan readings, a fixed-speed mode, and a drag-editable
//! temp→duty curve (reused [`widgets::curve_editor`]).

use crate::ui::components as widgets;
use egui::{Align2, Pos2, Rect, Sense, Vec2};
use halod_shared::commands::DaemonCommand;
use halod_shared::types::{CoolingChannel, DeviceCapability, FanCurveStatus, FanStatus, Sensor};

use super::{editing, DeviceUi, TabCtx};
use crate::ui::screens::cooling::preset_display_name;
use crate::ui::theme;

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::TabCooling,
        ui.max_rect(),
    );
    let channels = cooling_channels(ctx.dev);
    let is_cooling = ctx.dev.cooling().is_some();
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
        .find(|c| c.device_id == fan_id && c.channel_id == selected)
        // Pre-channel snapshots remain readable while a daemon/client pair is upgraded.
        .or_else(|| {
            ctx.state
                .cooling
                .fan_curves
                .iter()
                .find(|c| c.fan_id == fan_id && selected == "default")
        });
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

    if channels.len() > 1 {
        let label = channels
            .iter()
            .find(|c| c.id == selected)
            .map(|c| c.name.as_str())
            .unwrap_or("Cooling");
        egui::ComboBox::from_id_salt("cooling_channel")
            .selected_text(label)
            .show_ui(ui, |ui| {
                for channel in &channels {
                    if ui
                        .selectable_label(selected == channel.id, &channel.name)
                        .clicked()
                    {
                        st.cooling.channel_id = Some(channel.id.clone());
                        st.cooling.curve_seeded = false;
                    }
                }
            });
        ui.add_space(theme::SPACE_6);
    }
    top_row(
        ui,
        ctx,
        st,
        &fan,
        &sensors,
        sensor_temp,
        &selected,
        is_cooling,
    );
    ui.add_space(theme::SPACE_8);

    let curve_title = t!("cooling.fan_curve");
    curve_card(
        ui,
        ctx,
        st,
        &fan_id,
        &selected,
        is_cooling,
        curve.map(|c| c.status.clone()),
        sensor_temp,
        &curve_title,
    );
}

fn top_row(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    fan: &Option<FanStatus>,
    sensors: &[(String, Sensor)],
    sensor_temp: Option<f32>,
    channel_id: &str,
    is_cooling: bool,
) {
    ui.columns(2, |cols| {
        // Curve sensor selector + live temp.
        widgets::card(&mut cols[0], |ui| {
            widgets::caps_label(ui, &t!("cooling.curve_sensor_caps"));
            ui.add_space(theme::SPACE_4);
            let current = st
                .cooling
                .curve_sensor
                .as_ref()
                .and_then(|sid| sensors.iter().find(|(_, s)| &s.id == sid))
                .map(|(d, s)| format!("{d} · {} ({:.0}°C)", s.name, s.value))
                .unwrap_or_else(|| t!("cooling.no_sensor").to_string());
            let mut pick: Option<Option<String>> = None;
            let sensor_combo_rect =
                Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), 28.0));
            crate::domain::tour::anchor(
                ui.ctx(),
                crate::domain::tour::AnchorId::CoolingSensor,
                sensor_combo_rect,
            );
            egui::ComboBox::from_id_salt("curve_sensor")
                .selected_text(current)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(
                            st.cooling.curve_sensor.is_none(),
                            t!("cooling.no_sensor"),
                        )
                        .clicked()
                    {
                        pick = Some(None);
                    }
                    for (d, s) in sensors {
                        let on = st.cooling.curve_sensor.as_deref() == Some(s.id.as_str());
                        let label = format!("{d} · {} ({:.0}°C)", s.name, s.value);
                        if ui.selectable_label(on, label).clicked() {
                            pick = Some(Some(s.id.clone()));
                        }
                    }
                });
            if let Some(sel) = pick {
                st.cooling.curve_sensor = sel;
                st.last_edit = ctx.time;
                crate::runtime::ipc::send(
                    ctx.cmd,
                    curve_points_command(
                        ctx.dev.id.clone(),
                        channel_id.to_string(),
                        is_cooling,
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
                ui.label(
                    egui::RichText::new(t)
                        .font(theme::mono_bold(22.0))
                        .color(theme::STAT_CYAN),
                );
                ui.label(
                    egui::RichText::new(t!("cooling.celsius_now"))
                        .font(theme::body_sm())
                        .color(theme::TEXT_FAINT),
                );
            });
        });

        // Readings.
        widgets::card(&mut cols[1], |ui| {
            if let Some(f) = fan {
                widgets::value_row(
                    ui,
                    &t!("cooling.fan_speed"),
                    &t!("cooling.rpm", v = f.rpm),
                    theme::TEXT_BRIGHT,
                );
                widgets::value_row(
                    ui,
                    &t!("cooling.fan_duty"),
                    &t!("cooling.percent", v = f.duty),
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
    fan_id: &str,
    channel_id: &str,
    is_cooling: bool,
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
                    let selected_text = active
                        .map(preset_display_name)
                        .unwrap_or_else(|| t!("cooling.preset").to_string());
                    let mut pick: Option<&halod_shared::types::WirePresetCurve> = None;
                    let combo = egui::ComboBox::from_id_salt("curve_preset")
                        .selected_text(selected_text)
                        .width(130.0)
                        .show_ui(ui, |ui| {
                            ui.set_max_width(180.0);
                            for p in &presets {
                                let selected = active.is_some_and(|a| a.id == p.id);
                                if ui
                                    .selectable_label(selected, preset_display_name(p))
                                    .clicked()
                                {
                                    pick = Some(p);
                                }
                            }
                        });
                    crate::domain::tour::anchor(
                        ui.ctx(),
                        crate::domain::tour::AnchorId::CoolingPreset,
                        combo.response.rect,
                    );
                    if let Some(p) = pick {
                        st.cooling.curve = p.points.clone();
                        st.last_edit = ctx.time;
                        st.queue(
                            "curve",
                            curve_preset_command(
                                fan_id.to_string(),
                                channel_id.to_string(),
                                is_cooling,
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
        crate::domain::tour::anchor(
            ui.ctx(),
            crate::domain::tour::AnchorId::CoolingCurveEditor,
            plot,
        );
        // Y axis (duty %) and X axis (°C) labels.
        for i in 0..=4 {
            let frac = i as f32 / 4.0;
            let y = plot.bottom() - frac * plot.height();
            ui.painter().text(
                Pos2::new(plot.left() - 8.0, y),
                Align2::RIGHT_CENTER,
                format!("{}", (frac * 100.0) as i32),
                theme::value_xs(),
                theme::TEXT_FAINT2,
            );
            let x = plot.left() + frac * plot.width();
            ui.painter().text(
                Pos2::new(x, plot.bottom() + 12.0),
                Align2::CENTER_CENTER,
                format!("{}", 20 + (frac * 80.0) as i32),
                theme::value_xs(),
                theme::TEXT_FAINT2,
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
        ) {
            let cmd = curve_points_command(
                fan_id.to_string(),
                channel_id.to_string(),
                is_cooling,
                st.cooling.curve.clone(),
                st.cooling.curve_sensor.clone(),
            );
            st.queue("curve", cmd, ctx.time);
        }
    });
}

fn curve_points_command(
    device_id: String,
    channel_id: String,
    is_cooling: bool,
    points: Vec<[f32; 2]>,
    sensor_id: Option<String>,
) -> DaemonCommand {
    if is_cooling {
        DaemonCommand::SetCoolingCurvePoints {
            device_id,
            channel_id,
            points,
            sensor_id,
        }
    } else {
        DaemonCommand::SetFanCurvePoints {
            fan_id: device_id,
            points,
            sensor_id,
        }
    }
}

fn curve_preset_command(
    device_id: String,
    channel_id: String,
    is_cooling: bool,
    preset: String,
    sensor_id: Option<String>,
) -> DaemonCommand {
    if is_cooling {
        DaemonCommand::SetCoolingCurvePreset {
            device_id,
            channel_id,
            preset,
            sensor_id,
        }
    } else {
        DaemonCommand::SetFanCurvePreset {
            fan_id: device_id,
            preset,
            sensor_id,
        }
    }
}

fn cooling_channels(dev: &halod_shared::types::WireDevice) -> Vec<CoolingChannel> {
    dev.cooling()
        .map(|cooling| cooling.channels.clone())
        .unwrap_or_else(|| {
            vec![CoolingChannel {
                id: "default".into(),
                name: "Fan".into(),
                ..Default::default()
            }]
        })
}

fn find_fan(dev: &halod_shared::types::WireDevice, channel_id: &str) -> Option<FanStatus> {
    dev.capabilities.iter().find_map(|c| match c {
        DeviceCapability::Cooling(cooling) => cooling
            .channels
            .iter()
            .find(|c| c.id == channel_id)
            .map(|channel| FanStatus {
                channel: 0,
                rpm: channel.rpm.unwrap_or_default(),
                duty: channel.duty.unwrap_or_default(),
                controllable: channel.controllable,
            }),
        DeviceCapability::Fan(f) => Some(f.clone()),
        _ => None,
    })
}
