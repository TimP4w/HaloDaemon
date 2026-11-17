// SPDX-License-Identifier: GPL-3.0-or-later
//! The startup scan: discovery progress shown until the first scan completes.

use crate::domain::topic_store::TopicStore;
use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{DiscoveryDetail, DiscoveryPhase, DISCOVERY_STEPS};

use crate::domain::models::device as model;
use crate::ui::components as widgets;
use crate::ui::shell;
use crate::ui::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepState {
    Done,
    Running,
    Pending,
}

/// The scanner step `detail` belongs to; SMBus sub-steps count as SMBus.
fn step_index(detail: &DiscoveryDetail) -> Option<usize> {
    let detail = match detail {
        DiscoveryDetail::SmbusAdapter { .. } | DiscoveryDetail::SmbusBus { .. } => {
            &DiscoveryDetail::Smbus
        }
        other => other,
    };
    DISCOVERY_STEPS.iter().position(|step| step == detail)
}

fn step_states(phase: &DiscoveryPhase, detail: &DiscoveryDetail) -> Vec<StepState> {
    let current = match phase {
        DiscoveryPhase::Complete => DISCOVERY_STEPS.len(),
        _ => step_index(detail).unwrap_or(0),
    };
    (0..DISCOVERY_STEPS.len())
        .map(|i| match i.cmp(&current) {
            std::cmp::Ordering::Less => StepState::Done,
            std::cmp::Ordering::Equal => StepState::Running,
            std::cmp::Ordering::Greater => StepState::Pending,
        })
        .collect()
}

fn step_label(detail: &DiscoveryDetail) -> String {
    match detail {
        DiscoveryDetail::Usb => t!("misc.radar_phase_usb"),
        DiscoveryDetail::PluginHostTransports => t!("misc.radar_phase_plugin_hosts"),
        DiscoveryDetail::Hid => t!("misc.radar_phase_hid"),
        DiscoveryDetail::PluginIntegrations => t!("misc.radar_phase_plugin_integrations"),
        DiscoveryDetail::Hwmon => t!("misc.radar_phase_hwmon"),
        DiscoveryDetail::Computer => t!("misc.radar_phase_computer"),
        DiscoveryDetail::Smbus | DiscoveryDetail::None => t!("misc.radar_phase_smbus"),
        DiscoveryDetail::SmbusAdapter { name } => t!("misc.radar_phase_smbus_adapter", name = name),
        DiscoveryDetail::SmbusBus { number } => t!("misc.radar_phase_smbus_bus", number = number),
    }
    .to_string()
}

/// Returns true once discovery has completed and the workspace should open.
pub fn show(ui: &mut egui::Ui, state: &TopicStore, connected: bool) -> bool {
    egui::Panel::top("startup_title")
        .exact_size(52.0)
        .frame(egui::Frame::NONE.fill(theme::BODY))
        .show(ui, |ui| shell::title_bar_plain(ui, state));

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(theme::BODY))
        .show(ui, |ui| {
            let area = ui.max_rect();
            let column = Rect::from_min_size(
                Pos2::new(area.center().x - COLUMN_W / 2.0, area.center().y - 230.0),
                Vec2::new(COLUMN_W, 460.0),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(column), |ui| {
                body(ui, state, connected)
            });
        });
    connected && matches!(state.discovery.phase, DiscoveryPhase::Complete)
}

const COLUMN_W: f32 = 520.0;
const ROW_H: f32 = 43.0;

fn body(ui: &mut egui::Ui, state: &TopicStore, connected: bool) {
    ui.set_width(COLUMN_W);
    ui.spacing_mut().item_spacing.y = 0.0;
    let (logo, _) = ui.allocate_exact_size(Vec2::new(COLUMN_W, 22.0), Sense::hover());
    widgets::paint_logo(ui.painter(), ui.ctx(), logo.min, 22.0, 16.0);
    ui.add_space(22.0);
    let found = state.devices.iter().filter(|d| model::listable(d)).count();
    let (title, sub) = if connected {
        (
            t!("misc.startup_title"),
            t!("misc.startup_found", found = found),
        )
    } else {
        (t!("misc.radar_daemon_down_sub"), Default::default())
    };
    ui.label(
        egui::RichText::new(title)
            .font(theme::display())
            .color(theme::TEXT),
    );
    ui.add_space(theme::SPACE_3);
    ui.label(
        egui::RichText::new(sub)
            .font(theme::body_lg())
            .color(theme::TEXT_DIM),
    );
    ui.add_space(22.0);
    let states = step_states(&state.discovery.phase, &state.discovery.detail);
    let done = states.iter().filter(|s| **s == StepState::Done).count();
    widgets::progress_bar(
        ui,
        COLUMN_W,
        4.0,
        connected.then_some(done as f32 / DISCOVERY_STEPS.len() as f32),
    );
    ui.add_space(22.0);
    let (list, _) = ui.allocate_exact_size(
        Vec2::new(COLUMN_W, ROW_H * DISCOVERY_STEPS.len() as f32),
        Sense::hover(),
    );
    theme::paint_surface(
        ui.painter(),
        list,
        theme::RADIUS_LG,
        theme::CARD_BG,
        theme::BORDER,
    );
    let time = ui.input(|i| i.time) as f32;
    for (i, (step, step_state)) in DISCOVERY_STEPS.iter().zip(&states).enumerate() {
        let row = Rect::from_min_size(
            list.min + Vec2::new(0.0, i as f32 * ROW_H),
            Vec2::new(COLUMN_W, ROW_H),
        );
        if i > 0 {
            ui.painter().hline(
                row.x_range(),
                row.top(),
                Stroke::new(1.0, theme::BORDER_SOFT),
            );
        }
        let state = if connected {
            *step_state
        } else {
            StepState::Pending
        };
        step_row(ui.painter(), row, &step_label(step), state, time);
    }
    if state.discovery.checking_updates {
        ui.add_space(theme::SPACE_5);
        ui.label(
            egui::RichText::new(t!("misc.radar_checking_updates"))
                .font(theme::body_sm())
                .color(theme::TEXT_MUT),
        );
    }
    ui.ctx().request_repaint();
}

/// Live discovery status for pages behind the startup screen (a rescan).
pub(crate) fn discovery_status(ui: &mut egui::Ui, state: &TopicStore) {
    if !matches!(state.discovery.phase, DiscoveryPhase::Discovering) {
        return;
    }
    let states = step_states(&state.discovery.phase, &state.discovery.detail);
    let done = states.iter().filter(|s| **s == StepState::Done).count();
    let time = ui.input(|i| i.time) as f32;
    widgets::card_with_surface(
        ui,
        egui::Margin::symmetric(16, 14),
        theme::CHECKED_BG,
        theme::ACCENT_BORDER,
        |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (icon, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                widgets::paint_spinner(ui.painter(), icon.center(), 16.0, time, theme::CYAN);
                ui.add_space(theme::SPACE_2);
                ui.label(
                    egui::RichText::new(t!("misc.discovery_looking"))
                        .font(theme::semibold(14.0))
                        .color(theme::TEXT),
                );
                ui.label(
                    egui::RichText::new(format!("· {}", step_label(&state.discovery.detail)))
                        .font(theme::body_lg())
                        .color(theme::ACCENT_TEXT),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(t!(
                            "misc.discovery_step",
                            step = (done + 1).min(DISCOVERY_STEPS.len()),
                            total = DISCOVERY_STEPS.len()
                        ))
                        .font(theme::body_md())
                        .color(theme::ACCENT_TEXT),
                    );
                });
            });
            ui.add_space(theme::SPACE_5);
            widgets::progress_bar(
                ui,
                ui.available_width(),
                4.0,
                Some(done as f32 / DISCOVERY_STEPS.len() as f32),
            );
        },
    );
    ui.add_space(theme::SPACE_8);
    ui.ctx().request_repaint();
}

fn step_row(p: &egui::Painter, row: Rect, label: &str, step_state: StepState, time: f32) {
    let icon = Pos2::new(row.left() + 25.0, row.center().y);
    match step_state {
        StepState::Done => {
            p.add(egui::Shape::line(
                vec![
                    icon + Vec2::new(-4.0, 0.3),
                    icon + Vec2::new(-1.2, 3.0),
                    icon + Vec2::new(4.0, -3.0),
                ],
                Stroke::new(1.6, theme::TEXT),
            ));
        }
        StepState::Running => widgets::paint_spinner(p, icon, 16.0, time, theme::CYAN),
        StepState::Pending => {}
    }
    p.text(
        Pos2::new(row.left() + 46.0, row.center().y),
        Align2::LEFT_CENTER,
        label,
        theme::body_lg(),
        if step_state == StepState::Pending {
            theme::TEXT_MUT
        } else {
            theme::TEXT
        },
    );
    if step_state == StepState::Running {
        p.text(
            Pos2::new(row.right() - 16.0, row.center().y),
            Align2::RIGHT_CENTER,
            t!("misc.startup_scanning"),
            theme::body_md(),
            theme::TEXT_MUT,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smbus_substeps_map_to_the_smbus_step() {
        let smbus = step_index(&DiscoveryDetail::Smbus);
        assert!(smbus.is_some());
        assert_eq!(step_index(&DiscoveryDetail::SmbusBus { number: 3 }), smbus);
        assert_eq!(
            step_index(&DiscoveryDetail::SmbusAdapter {
                name: "i801".into()
            }),
            smbus
        );
    }

    #[test]
    fn steps_before_the_current_one_are_done() {
        let states = step_states(&DiscoveryPhase::Discovering, &DISCOVERY_STEPS[2]);
        assert_eq!(
            &states[..3],
            &[StepState::Done, StepState::Done, StepState::Running]
        );
        assert!(states[3..].iter().all(|s| *s == StepState::Pending));
    }

    #[test]
    fn completion_marks_every_step_done() {
        assert!(
            step_states(&DiscoveryPhase::Complete, &DiscoveryDetail::None)
                .iter()
                .all(|s| *s == StepState::Done)
        );
    }
}
