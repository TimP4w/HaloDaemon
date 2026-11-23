// SPDX-License-Identifier: GPL-3.0-or-later
use egui::Vec2;

use super::{button, inset, ButtonKind};
use crate::ui::theme;

pub fn command_block(ui: &mut egui::Ui, command: &str, copied: bool) -> bool {
    let mut clicked = false;
    inset(ui, egui::Margin::same(12), |ui| {
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(command)
                            .font(theme::mono(10.5))
                            .color(theme::TEXT_DIM),
                    )
                    .selectable(true),
                );
            },
            |ui| {
                let label = if copied {
                    t!("misc.command_copied")
                } else {
                    t!("misc.copy_command")
                };
                if button(ui, &label, ButtonKind::Secondary, Vec2::new(125.0, 28.0)).clicked() {
                    ui.ctx().copy_text(command.to_owned());
                    clicked = true;
                }
            },
        );
    });
    clicked
}
