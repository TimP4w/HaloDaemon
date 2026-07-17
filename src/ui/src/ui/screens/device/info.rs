// SPDX-License-Identifier: GPL-3.0-or-later
//! Info tab — device metadata, the live `GetDebugInfo` detection/driver state,
//! a capabilities chip card, and a Copy-JSON button (mirrors the GTK debug
//! dialog). The debug snapshot is requested once per opened device.

use crate::ui::components as widgets;
use halod_shared::debug_info::DeviceDebugInfo;
use halod_shared::types::{ConnectionType, DeviceCapability, WriteRateStatus};

use super::header::led_count;
use super::{DeviceUi, TabCtx};
use crate::domain::models::device::find_hub_write_rate;
use crate::ui::theme;

fn format_write_rate(wr: WriteRateStatus) -> String {
    let current_kb_s = wr.current_bytes_per_sec / 1024.0;
    match wr.limit {
        Some(limit) => format!(
            "{:.1} / {:.1} KB/s",
            current_kb_s,
            limit.max_bytes_per_sec as f32 / 1024.0
        ),
        None => format!("{current_kb_s:.1} KB/s"),
    }
}

fn write_rate_samples_kb_s(history: &std::collections::VecDeque<f32>) -> Vec<f32> {
    history.iter().map(|b| b / 1024.0).collect()
}

fn draw_write_rate_graph(
    ui: &mut egui::Ui,
    history: &std::collections::VecDeque<f32>,
    wr: WriteRateStatus,
) {
    let samples = write_rate_samples_kb_s(history);
    if samples.len() < 2 {
        return;
    }
    ui.add_space(theme::SPACE_2);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 50.0), egui::Sense::hover());
    let painter = ui.painter();
    crate::ui::screens::home::sparkline(painter, rect, &samples, theme::STAT_CYAN);
    if let Some(limit) = wr.limit {
        let limit_kb_s = limit.max_bytes_per_sec as f32 / 1024.0;
        let y = crate::ui::screens::home::sparkline_reference_y(rect, limit_kb_s, &samples);
        painter.hline(
            rect.x_range(),
            y,
            egui::Stroke::new(1.0, theme::TEXT_FAINT2),
        );
    }
}

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    // Request a debug snapshot once for this device.
    if !st.debug_requested {
        crate::runtime::ipc::send(ctx.cmd, halod_shared::commands::DaemonCommand::GetDebugInfo);
        st.debug_requested = true;
    }
    let dev = ctx.dev;
    let dbg: Option<&DeviceDebugInfo> = ctx
        .debug
        .and_then(|d| d.devices.iter().find(|x| x.id == dev.id));

    fn row(ui: &mut egui::Ui, label: &str, value: String) {
        if !value.is_empty() {
            widgets::value_row(ui, label, &value, theme::TEXT_BRIGHT);
        }
    }

    ui.columns(2, |cols| {
        // Left: device + detection rows.
        cols[0].vertical(|ui| {
            widgets::card(ui, |ui| {
                row(ui, &t!("devtabs.info_id"), dev.id.clone());
                row(ui, &t!("devtabs.info_name"), dev.name.clone());
                row(ui, &t!("devtabs.info_vendor"), dev.vendor.clone());
                row(ui, &t!("devtabs.info_model"), dev.model.clone());
                row(
                    ui,
                    &t!("devtabs.info_type"),
                    crate::domain::models::device::type_label(dev).to_string(),
                );
                if let Some(sn) = &dev.serial_number {
                    row(ui, &t!("devtabs.info_serial"), sn.clone());
                }
                row(
                    ui,
                    &t!("devtabs.info_connection"),
                    match dev.connection_type {
                        Some(ConnectionType::Wireless) => t!("devtabs.wireless").to_string(),
                        Some(ConnectionType::Wired) => t!("devtabs.wired").to_string(),
                        None => String::new(),
                    },
                );
                let leds = led_count(dev);
                if leds > 0 {
                    row(ui, &t!("devtabs.info_lighting_leds"), leds.to_string());
                }
                row(
                    ui,
                    &t!("devtabs.info_status"),
                    if dev.connected {
                        t!("devtabs.connected")
                    } else {
                        t!("devtabs.offline")
                    }
                    .to_string(),
                );
                if let Some(d) = dbg {
                    row(ui, &t!("devtabs.info_transport"), d.transport.clone());
                    for (k, v) in &d.fields {
                        row(ui, k, v.clone());
                    }
                }
            });
        });

        // Right: capabilities, write rate, copy JSON.
        cols[1].vertical(|ui| {
            widgets::card_titled(
                ui,
                &t!("devtabs.capabilities"),
                |_| {},
                |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                        for c in &dev.capabilities {
                            widgets::chip(ui, &cap_label_tr(c));
                        }
                    });
                },
            );

            let (write_rate, via_hub) = match dev.write_rate {
                Some(wr) => (Some(wr), false),
                None => (find_hub_write_rate(ctx.state, &dev.id), true),
            };
            if let Some(wr) = write_rate {
                ui.add_space(theme::SPACE_8);
                let wr_title = if via_hub {
                    t!("devtabs.write_rate_shared")
                } else {
                    t!("devtabs.write_rate")
                };
                widgets::card_titled(
                    ui,
                    &wr_title,
                    |_| {},
                    |ui| {
                        row(ui, &t!("devtabs.throughput"), format_write_rate(wr));
                        if wr.rejected_total > 0 {
                            row(
                                ui,
                                &t!("devtabs.rejected_writes"),
                                wr.rejected_total.to_string(),
                            );
                        }
                        if let Some(history) = ctx.write_rate_history {
                            draw_write_rate_graph(ui, history, wr);
                        }
                    },
                );
            }
            ui.add_space(theme::SPACE_8);
            widgets::card_titled(
                ui,
                &t!("devtabs.diagnostics"),
                |_| {},
                |ui| {
                    let json = dbg
                        .map(|d| {
                            serde_json::to_string_pretty(d)
                                .unwrap_or_else(|e| format!("serialization error: {e}"))
                        })
                        .unwrap_or_else(|| {
                            serde_json::to_string_pretty(dev)
                                .unwrap_or_else(|e| format!("serialization error: {e}"))
                        });
                    let copied_key = egui::Id::new("info_diag_copied_at");
                    ui.horizontal(|ui| {
                        if widgets::button(
                            ui,
                            &t!("devtabs.copy_json"),
                            widgets::ButtonKind::Ghost,
                            egui::vec2(110.0, 30.0),
                        )
                        .clicked()
                        {
                            ui.ctx().copy_text(json);
                            let now = ui.ctx().input(|i| i.time);
                            ui.ctx().data_mut(|d| d.insert_temp(copied_key, now));
                        }

                        let copied_at = ui.ctx().data(|d| d.get_temp::<f64>(copied_key));
                        let now = ui.ctx().input(|i| i.time);
                        if super::super::settings::copied_feedback_visible(copied_at, now) {
                            ui.add_space(theme::SPACE_5);
                            ui.label(
                                egui::RichText::new(t!("devtabs.copied"))
                                    .font(theme::subhead())
                                    .color(theme::TRAFFIC_GREEN),
                            );
                            ui.ctx().request_repaint();
                        }
                    });
                    if ctx.debug.is_none() {
                        ui.add_space(theme::SPACE_4);
                        ui.label(
                            egui::RichText::new(t!("devtabs.fetching_debug"))
                                .font(theme::caption())
                                .color(theme::TEXT_FAINT2),
                        );
                    }
                },
            );
        });
    });
}

/// User-facing translated capability chip label.
fn cap_label_tr(c: &DeviceCapability) -> std::borrow::Cow<'static, str> {
    match c {
        DeviceCapability::Children(_) => t!("devtabs.cap_children"),
        DeviceCapability::Pairing(_) => t!("devtabs.cap_pairing"),
        DeviceCapability::Choice(_) => t!("devtabs.cap_settings"),
        DeviceCapability::Range(_) => t!("devtabs.cap_ranges"),
        DeviceCapability::Boolean(_) => t!("devtabs.cap_toggles"),
        DeviceCapability::Action(_) => t!("devtabs.cap_actions"),
        DeviceCapability::Battery(_) => t!("devtabs.cap_battery"),
        DeviceCapability::Connection(_) => t!("devtabs.cap_connection"),
        DeviceCapability::Equalizer(_) => t!("devtabs.cap_equalizer"),
        DeviceCapability::Sensors(_) => t!("devtabs.cap_sensors"),
        DeviceCapability::Cooling(_) => t!("devtabs.cap_fan"),
        DeviceCapability::Rgb(_) => t!("devtabs.cap_rgb"),
        DeviceCapability::Dpi(_) => t!("devtabs.cap_dpi"),
        DeviceCapability::OnboardProfiles(_) => t!("devtabs.cap_onboard"),
        DeviceCapability::Lcd(_) => t!("devtabs.cap_lcd"),
        DeviceCapability::KeyRemap(_) => t!("devtabs.cap_key_remap"),
        DeviceCapability::KeyboardLayout(_) => t!("devtabs.cap_keyboard_layout"),
    }
}

#[cfg(test)]
mod tests {
    use super::{cap_label_tr, format_write_rate, write_rate_samples_kb_s};
    use halod_shared::types::{DeviceCapability as C, WriteRateLimit, WriteRateStatus};
    use std::collections::VecDeque;

    #[test]
    fn write_rate_samples_kb_s_converts_bytes_to_kb() {
        let history: VecDeque<f32> = VecDeque::from([1024.0, 2048.0, 512.0]);
        assert_eq!(write_rate_samples_kb_s(&history), vec![1.0, 2.0, 0.5]);
    }

    #[test]
    fn format_write_rate_omits_ceiling_when_unset() {
        let wr = WriteRateStatus {
            limit: None,
            current_writes_per_sec: 5.0,
            current_bytes_per_sec: 2048.0,
            rejected_total: 0,
        };
        assert_eq!(format_write_rate(wr), "2.0 KB/s");
    }

    #[test]
    fn format_write_rate_shows_ceiling_when_set() {
        let wr = WriteRateStatus {
            limit: Some(WriteRateLimit {
                max_bytes_per_sec: 4096,
            }),
            current_writes_per_sec: 5.0,
            current_bytes_per_sec: 1024.0,
            rejected_total: 0,
        };
        assert_eq!(format_write_rate(wr), "1.0 / 4.0 KB/s");
    }

    #[test]
    fn cap_label_covers_every_capability() {
        // One representative per variant — a new capability that forgets a
        // label arm fails to compile (exhaustive match); each must resolve to a
        // real translation, not the raw `devtabs.cap_*` key.
        for c in [
            C::Children(vec![]),
            C::Battery(vec![]),
            C::Sensors(vec![]),
            C::Choice(vec![]),
            C::Range(vec![]),
            C::Boolean(vec![]),
            C::Action(vec![]),
        ] {
            let label = cap_label_tr(&c);
            assert!(!label.is_empty());
            assert!(!label.starts_with("devtabs."), "untranslated: {label}");
        }
    }
}
