// SPDX-License-Identifier: GPL-3.0-or-later
//! Info tab — throughput chart, device details, capabilities, diagnostics
//! (`GetDebugInfo` snapshot as JSON) and visibility management.

use halod_shared::commands::DeviceOp;
use std::collections::VecDeque;

use crate::ui::components as widgets;
use egui::{Align2, Pos2, Rect, Stroke, Vec2};
use halod_shared::debug_info::DeviceDebugInfo;
use halod_shared::types::{ConnectionType, DeviceCapability, VisibilityState, WriteRateStatus};

use super::header::led_count;
use super::{DeviceUi, TabCtx};
use crate::domain::models::device::find_hub_write_rate;
use crate::ui::theme;

/// Chart windows offered by the range selector, in seconds (history is sampled at 1 Hz).
const RANGES: [usize; 3] = [60, 600, 3600];
const CHART_POINTS: usize = 120;

fn kb_s(bytes_per_sec: f32) -> String {
    format!("{:.1} KB/s", bytes_per_sec / 1024.0)
}

/// The last `window` 1 Hz samples, bucket-averaged down to at most
/// [`CHART_POINTS`], each paired with its age as a fraction of the window
/// (0 = now, 1 = oldest edge).
fn window_points(history: &VecDeque<f32>, window: usize) -> Vec<(f32, f32)> {
    let n = history.len().min(window);
    let bucket = window.div_ceil(CHART_POINTS).max(1);
    let recent: Vec<f32> = history.iter().skip(history.len() - n).copied().collect();
    recent
        .rchunks(bucket)
        .enumerate()
        .map(|(i, chunk)| {
            let avg = chunk.iter().sum::<f32>() / chunk.len() as f32;
            ((i * bucket) as f32 / window as f32, avg)
        })
        .collect()
}

/// A round axis ceiling `>= peak` that splits evenly into three gridlines.
fn axis_max(peak: f32) -> f32 {
    let third = (peak.max(1.0) / 3.0).max(f32::MIN_POSITIVE);
    let mag = 10f32.powf(third.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .map(|m| m * mag)
        .find(|s| *s >= third)
        .unwrap_or(10.0 * mag);
    step * 3.0
}

fn axis_tick(v: f32) -> String {
    if (v - v.round()).abs() < 0.01 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

fn range_labels(window: usize) -> [String; 2] {
    if window < 120 {
        [format!("−{window} s"), format!("−{} s", window / 2)]
    } else if window < 3600 {
        [
            format!("−{} min", window / 60),
            format!("−{} min", window / 120),
        ]
    } else {
        [
            format!("−{} h", window / 3600),
            format!("−{} min", window / 120),
        ]
    }
}

fn throughput_chart(ui: &mut egui::Ui, history: Option<&VecDeque<f32>>, window: usize) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, w * 220.0 / 760.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 8.0, theme::INNER_BG);
    let scale = rect.height() / 220.0;
    let plot = Rect::from_min_max(
        rect.min + Vec2::new(48.0, 30.0 * scale),
        Pos2::new(rect.right() - 12.0, rect.top() + 180.0 * scale),
    );
    let pts = history
        .map(|h| window_points(h, window))
        .unwrap_or_default();
    let top = axis_max(pts.iter().map(|(_, v)| *v / 1024.0).fold(0.0, f32::max));
    let label = |pos: Pos2, align: Align2, text: String| {
        p.text(pos, align, text, theme::caption(), theme::TEXT_FAINT);
    };
    for i in 0..=3 {
        let y = plot.bottom() - plot.height() * i as f32 / 3.0;
        p.hline(plot.x_range(), y, Stroke::new(1.0, theme::BORDER_SOFT));
        label(
            Pos2::new(plot.left() - 8.0, y),
            Align2::RIGHT_CENTER,
            axis_tick(top * i as f32 / 3.0),
        );
    }
    label(
        rect.min + Vec2::new(12.0, 10.0),
        Align2::LEFT_TOP,
        "KB/s".into(),
    );
    let axis_y = rect.top() + 202.0 * scale;
    let [oldest, middle] = range_labels(window);
    label(Pos2::new(plot.left(), axis_y), Align2::LEFT_CENTER, oldest);
    label(
        Pos2::new(plot.center().x, axis_y),
        Align2::CENTER_CENTER,
        middle,
    );
    label(
        Pos2::new(plot.right(), axis_y),
        Align2::RIGHT_CENTER,
        t!("devtabs.axis_now").to_string(),
    );
    if pts.len() < 2 {
        return;
    }
    let line: Vec<Pos2> = pts
        .iter()
        .rev()
        .map(|(age, v)| {
            Pos2::new(
                plot.right() - age * plot.width(),
                plot.bottom() - (v / 1024.0 / top).min(1.0) * plot.height(),
            )
        })
        .collect();
    widgets::fill_under_line(p, &line, plot.bottom(), theme::a(theme::CYAN, 0.12));
    p.add(egui::Shape::line(
        line.clone(),
        Stroke::new(1.8, theme::CYAN),
    ));
    if let Some(last) = line.last() {
        p.circle_filled(*last, 4.0, theme::CYAN);
    }
}

fn stat(ui: &mut egui::Ui, label: &str, value: &str, color: egui::Color32) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(
            egui::RichText::new(label)
                .font(theme::body_sm())
                .color(theme::TEXT_MUT),
        );
        ui.label(
            egui::RichText::new(value)
                .font(theme::semibold(26.0))
                .color(color),
        );
    });
}

fn throughput_card(ui: &mut egui::Ui, ctx: &TabCtx, wr: WriteRateStatus, via_hub: bool) {
    let range_key = egui::Id::unique("device_info_range");
    let mut range = ui
        .ctx()
        .data(|d| d.get_temp::<usize>(range_key))
        .unwrap_or(0);
    let window = RANGES[range.min(RANGES.len() - 1)];
    widgets::card_with_margin(ui, egui::Margin::symmetric(20, 18), |ui| {
        ui.spacing_mut().item_spacing.y = theme::SPACE_7;
        egui::Sides::new().show(
            ui,
            |ui| {
                let title = if via_hub {
                    t!("devtabs.write_rate_shared")
                } else {
                    t!("devtabs.throughput")
                };
                ui.label(
                    egui::RichText::new(title)
                        .font(theme::heading())
                        .color(theme::TEXT),
                );
            },
            |ui| {
                let opts = [
                    t!("devtabs.range_1m"),
                    t!("devtabs.range_10m"),
                    t!("devtabs.range_1h"),
                ];
                let refs: Vec<&str> = opts.iter().map(|s| s.as_ref()).collect();
                if widgets::segmented(ui, &refs, &mut range, false) {
                    ui.ctx().data_mut(|d| d.insert_temp(range_key, range));
                }
            },
        );
        let peak = ctx
            .write_rate_history
            .map(|h| h.iter().rev().take(window).copied().fold(0.0, f32::max))
            .unwrap_or(wr.current_bytes_per_sec)
            .max(wr.current_bytes_per_sec);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 28.0;
            stat(
                ui,
                &t!("devtabs.now"),
                &kb_s(wr.current_bytes_per_sec),
                theme::TEXT,
            );
            stat(ui, &t!("devtabs.peak"), &kb_s(peak), theme::TEXT_SOFT);
            let limit = wr
                .limit
                .map(|l| kb_s(l.max_bytes_per_sec as f32))
                .unwrap_or_else(|| t!("devtabs.write_limit_none").to_string());
            stat(ui, &t!("devtabs.write_limit"), &limit, theme::TEXT_SOFT);
            stat(
                ui,
                &t!("devtabs.rejected_writes"),
                &wr.rejected_total.to_string(),
                theme::TEXT_SOFT,
            );
        });
        throughput_chart(ui, ctx.write_rate_history, window);
    });
}

/// `HID 1e71:3012 · interface 0` from the debug snapshot's transport fields.
fn transport_detail(label: String, dbg: Option<&DeviceDebugInfo>) -> String {
    let field = |k: &str| {
        dbg.and_then(|d| d.fields.iter().find(|(key, _)| key == k))
            .map(|(_, v)| v.as_str())
    };
    let mut out = label;
    if let (Some(vid), Some(pid)) = (field("vid"), field("pid")) {
        out.push_str(&format!(" {vid}:{pid}"));
    }
    if let Some(iface) = field("interface") {
        out.push_str(&format!(" · {}", t!("devtabs.interface_n", n = iface)));
    }
    out
}

fn provided_by(ctx: &TabCtx, dbg: Option<&DeviceDebugInfo>) -> Option<String> {
    let id = dbg?
        .fields
        .iter()
        .find(|(k, _)| k == "plugin_id" || k == "integration_id")
        .map(|(_, v)| v)?;
    let plugin = ctx.state.plugins.plugins.iter().find(|p| &p.id == id)?;
    Some(
        t!(
            "devtabs.provided_by",
            name = plugin.localized_name(&rust_i18n::locale()),
            version = plugin.version
        )
        .to_string(),
    )
}

fn details_card(ui: &mut egui::Ui, ctx: &TabCtx, dbg: Option<&DeviceDebugInfo>) {
    let dev = ctx.dev;
    let link = crate::domain::models::device::transport_label(dev);
    let connection = match dev.connection_type {
        Some(ConnectionType::Wireless) => format!("{} · {link}", t!("devtabs.wireless")),
        _ => format!("{} · {link}", t!("devtabs.wired")),
    };
    let leds = led_count(dev);
    let rows = [
        (t!("devtabs.info_name"), dev.name.clone()),
        (t!("devtabs.info_vendor"), dev.vendor.clone()),
        (t!("devtabs.info_model"), dev.model.clone()),
        (
            t!("devtabs.info_type"),
            crate::domain::models::device::type_label(dev).to_string(),
        ),
        (t!("devtabs.info_connection"), connection),
        (t!("devtabs.info_transport"), transport_detail(link, dbg)),
        (
            t!("devtabs.info_serial"),
            dev.serial_number.clone().unwrap_or_default(),
        ),
        (
            t!("devtabs.info_lighting_leds"),
            if leds > 0 {
                leds.to_string()
            } else {
                String::new()
            },
        ),
        (t!("devtabs.info_id"), dev.id.clone()),
    ];
    widgets::card_with_margin(ui, egui::Margin::symmetric(20, 6), |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let shown: Vec<_> = rows.iter().filter(|(_, v)| !v.is_empty()).collect();
        for (k, v) in shown {
            widgets::detail_row(ui, k, v);
        }
    });
}

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    if !st.debug_requested {
        crate::runtime::ipc::send(ctx.cmd, halod_shared::commands::DaemonCommand::GetDebugInfo);
        st.debug_requested = true;
    }
    let dev = ctx.dev;
    let dbg: Option<&DeviceDebugInfo> = ctx
        .debug
        .and_then(|d| d.devices.iter().find(|x| x.id == dev.id));

    const ASIDE_W: f32 = 340.0;
    let main_w = (ui.available_width() - ASIDE_W - 16.0).max(320.0);
    widgets::split_columns(ui, main_w, 16.0, |ui, aside| {
        ui.spacing_mut().item_spacing.y = theme::SPACE_8;
        let (write_rate, via_hub) = match dev.write_rate {
            Some(wr) => (Some(wr), false),
            None => (find_hub_write_rate(ctx.state, &dev.id), true),
        };
        if let Some(wr) = write_rate {
            throughput_card(ui, ctx, wr, via_hub);
        }
        details_card(ui, ctx, dbg);

        let ui = aside;
        ui.spacing_mut().item_spacing.y = theme::SPACE_8;
        aside_card(ui, &t!("devtabs.capabilities"), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                for c in &dev.capabilities {
                    widgets::chip(ui, &cap_label_tr(c));
                }
            });
            if let Some(by) = provided_by(ctx, dbg) {
                ui.label(
                    egui::RichText::new(by)
                        .font(theme::body_md())
                        .color(theme::TEXT_DIM),
                );
            }
        });
        aside_card(ui, &t!("devtabs.diagnostics"), |ui| {
            ui.label(
                egui::RichText::new(t!("devtabs.diagnostics_desc"))
                    .font(theme::body_md())
                    .color(theme::TEXT_DIM),
            );
            let copied_key = egui::Id::unique("info_diag_copied_at");
            let now = ui.ctx().input(|i| i.time);
            let copied_at = ui.ctx().data(|d| d.get_temp::<f64>(copied_key));
            let copied = super::super::settings::copied_feedback_visible(copied_at, now);
            if copied {
                ui.ctx().request_repaint();
            }
            let label = if copied {
                t!("devtabs.copied")
            } else {
                t!("devtabs.copy_json")
            };
            if full_button(ui, &label, widgets::ButtonKind::Secondary) {
                let json = match dbg {
                    Some(d) => serde_json::to_string_pretty(d),
                    None => serde_json::to_string_pretty(dev),
                }
                .unwrap_or_else(|e| format!("serialization error: {e}"));
                ui.ctx().copy_text(json);
                ui.ctx().data_mut(|d| d.insert_temp(copied_key, now));
            }
        });
        aside_card(ui, &t!("devtabs.manage"), |ui| {
            ui.spacing_mut().item_spacing.y = theme::SPACE_4;
            let (label, target) = if dev.active_state == VisibilityState::Hidden {
                (t!("devtabs.show_on_home"), VisibilityState::Visible)
            } else {
                (t!("devtabs.hide_from_home"), VisibilityState::Hidden)
            };
            if full_button(ui, &label, widgets::ButtonKind::Secondary) {
                set_visibility(ctx, target);
            }
            let (label, target, kind) = if dev.active_state == VisibilityState::Disabled {
                (
                    t!("devtabs.enable_device"),
                    VisibilityState::Visible,
                    widgets::ButtonKind::Secondary,
                )
            } else {
                (
                    t!("devtabs.disable_device"),
                    VisibilityState::Disabled,
                    widgets::ButtonKind::Danger,
                )
            };
            if full_button(ui, &label, kind) {
                set_visibility(ctx, target);
            }
        });
    });
}

fn aside_card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    widgets::card_with_margin(ui, egui::Margin::same(18), |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = theme::SPACE_6;
        ui.label(
            egui::RichText::new(title)
                .font(theme::heading())
                .color(theme::TEXT),
        );
        body(ui);
    });
}

fn full_button(ui: &mut egui::Ui, label: &str, kind: widgets::ButtonKind) -> bool {
    widgets::button(ui, label, kind, Vec2::new(ui.available_width(), 30.0)).clicked()
}

fn set_visibility(ctx: &TabCtx, state: VisibilityState) {
    crate::runtime::ipc::send(
        ctx.cmd,
        halod_shared::commands::DaemonCommand::Device {
            device_id: ctx.dev.id.clone(),
            op: DeviceOp::SetDeviceVisibility { state },
        },
    );
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
        DeviceCapability::Lighting(_) => t!("devtabs.cap_rgb"),
        DeviceCapability::Dpi(_) => t!("devtabs.cap_dpi"),
        DeviceCapability::OnboardProfiles(_) => t!("devtabs.cap_onboard"),
        DeviceCapability::Lcd(_) => t!("devtabs.cap_lcd"),
        DeviceCapability::KeyRemap(_) => t!("devtabs.cap_key_remap"),
        DeviceCapability::KeyboardLayout(_) => t!("devtabs.cap_keyboard_layout"),
    }
}

#[cfg(test)]
mod tests {
    use super::{axis_max, cap_label_tr, window_points, CHART_POINTS};
    use halod_shared::types::DeviceCapability as C;
    use std::collections::VecDeque;

    #[test]
    fn axis_ceiling_covers_peak_and_splits_into_round_thirds() {
        for peak in [0.0, 0.4, 3.0, 9.6, 41.2, 48.0, 950.0, 12_345.0] {
            let top = axis_max(peak);
            assert!(top >= peak, "{top} < {peak}");
            let third = top / 3.0;
            let mag = 10f32.powf(third.log10().floor());
            let m = third / mag;
            assert!(
                [1.0, 2.0, 2.5, 5.0, 10.0]
                    .iter()
                    .any(|r| (m - r).abs() < 1e-3),
                "{third} is not a round step"
            );
        }
        assert!(axis_max(10.0) <= axis_max(41.2));
    }

    #[test]
    fn window_points_are_bounded_and_ordered_from_now() {
        let history: VecDeque<f32> = (0..5000).map(|i| i as f32).collect();
        for window in [60, 600, 3600] {
            let pts = window_points(&history, window);
            assert!(pts.len() <= CHART_POINTS);
            assert_eq!(pts[0].0, 0.0);
            assert!(pts.windows(2).all(|w| w[0].0 < w[1].0 && w[1].0 <= 1.0));
            assert!(pts[0].1 > pts[pts.len() - 1].1);
        }
    }

    #[test]
    fn short_history_only_spans_its_own_age() {
        let history: VecDeque<f32> = VecDeque::from(vec![1.0; 30]);
        let pts = window_points(&history, 60);
        assert_eq!(pts.len(), 30);
        assert!(pts.last().unwrap().0 < 0.5);
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
