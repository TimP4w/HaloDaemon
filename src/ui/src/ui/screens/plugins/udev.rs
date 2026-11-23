// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
pub(super) use crate::ui::screens::depcheck::udev_install_commands;

pub(super) fn udev_plugin_warning_banner(ui: &mut egui::Ui) -> bool {
    widgets::Banner::warn(&t!("plugins.udev_plugin_action"))
        .subtitle(&t!("plugins.udev_plugin_action_sub"))
        .action(widgets::BannerAction::new(
            &t!("plugins.issue_details"),
            ButtonKind::Secondary,
            Vec2::new(78.0, 28.0),
        ))
        .action_below()
        .show(ui)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UdevBannerAction {
    None,
    Install,
    Info,
}

pub(super) fn udev_copied_visible(copied_at: Option<f64>, now: f64) -> bool {
    crate::ui::screens::settings::copied_feedback_visible(copied_at, now)
}

pub(super) fn udev_rules_banner(
    ui: &mut egui::Ui,
    status: &halod_shared::types::UdevRulesStatus,
    copied: bool,
) -> UdevBannerAction {
    let mut action = UdevBannerAction::None;
    let (title, subtitle) = if let Some(path) = &status.installed_path {
        (
            t!("plugins.udev_outdated"),
            t!(
                "plugins.udev_outdated_sub",
                count = status.plugins_requiring_update.len(),
                path = path
            ),
        )
    } else {
        (
            t!("plugins.udev_missing"),
            t!(
                "plugins.udev_missing_sub",
                count = status.generated_rule_count
            ),
        )
    };
    widgets::Banner::warn(&title)
        .title_color(theme::TEXT)
        .subtitle(&subtitle)
        .dot(theme::STAT_AMBER)
        .show_with(ui, |ui| {
            ui.add_space(theme::SPACE_5);
            ui.horizontal(|ui| {
                let label = if copied {
                    t!("plugins.udev_copied")
                } else {
                    t!("plugins.udev_generate")
                };
                if widgets::button(ui, &label, ButtonKind::Warn, Vec2::new(150.0, 30.0)).clicked() {
                    action = UdevBannerAction::Install;
                }
                ui.add_space(theme::SPACE_3);
                if widgets::button(
                    ui,
                    &t!("plugins.udev_info"),
                    ButtonKind::Secondary,
                    Vec2::new(78.0, 30.0),
                )
                .clicked()
                {
                    action = UdevBannerAction::Info;
                }
            });
        });
    action
}

pub(super) fn udev_info_modal(
    ctx: &egui::Context,
    status: &halod_shared::types::UdevRulesStatus,
    rechecking: bool,
    copied_at: &mut Option<f64>,
) -> (bool, bool) {
    let now = ctx.input(|i| i.time);
    let copied = udev_copied_visible(*copied_at, now);
    if copied {
        ctx.request_repaint();
    }
    let mut close = false;
    let mut recheck = false;
    let dismissed = widgets::dialog_with_subtitle(
        ctx,
        "udev_rules_info",
        &t!("plugins.udev_info_title"),
        &t!("plugins.udev_info_intro"),
        700.0,
        |ui| {
            udev_modal_status(ui, status);
            ui.add_space(theme::SPACE_9);
            ui.label(
                egui::RichText::new(t!("plugins.udev_info_scope_title"))
                    .font(theme::heading())
                    .color(theme::TEXT),
            );
            ui.add_space(theme::SPACE_1);
            ui.label(
                egui::RichText::new(t!("plugins.udev_info_scope"))
                    .font(theme::body_md())
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(theme::SPACE_8);
            ui.label(
                egui::RichText::new(t!("plugins.udev_info_install_title"))
                    .font(theme::heading())
                    .color(theme::TEXT),
            );
            ui.add_space(theme::SPACE_1);
            ui.label(
                egui::RichText::new(t!("plugins.udev_info_install"))
                    .font(theme::body_md())
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(theme::SPACE_4);
            if widgets::command_block(ui, &udev_install_commands(), copied) {
                *copied_at = Some(now);
            }
        },
        |ui| {
            // The action row lays out right-to-left: buttons hug the right edge,
            // then the hint fills the space that remains, folding onto extra lines
            // instead of running under the buttons.
            let recheck_size = Vec2::new(132.0, 34.0);
            if rechecking {
                widgets::button_loading(
                    ui,
                    &t!("plugins.udev_rechecking"),
                    ButtonKind::Primary,
                    recheck_size,
                );
            } else if widgets::button(
                ui,
                &t!("plugins.udev_recheck"),
                ButtonKind::Primary,
                recheck_size,
            )
            .clicked()
            {
                recheck = true;
            }
            ui.add_space(theme::SPACE_4);
            if widgets::button(
                ui,
                &t!("plugins.issue_close"),
                ButtonKind::Secondary,
                Vec2::new(100.0, 32.0),
            )
            .clicked()
            {
                close = true;
            }
            ui.add_space(theme::SPACE_7);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(t!("plugins.udev_recheck_hint"))
                        .font(theme::caption())
                        .color(theme::TEXT_FAINT),
                )
                .wrap(),
            );
        },
    );
    (close || dismissed, recheck)
}

pub(super) fn udev_rule_details_modal(ctx: &egui::Context, rules: &[String]) -> bool {
    let mut close = false;
    let dismissed = widgets::dialog(
        ctx,
        "udev_rule_details",
        &t!("plugins.udev_rule_details_title"),
        700.0,
        |ui| {
            widgets::inset(ui, egui::Margin::same(14), |ui| {
                ui.label(
                    egui::RichText::new(rules.join("\n"))
                        .font(theme::mono(10.5))
                        .color(theme::TEXT_DIM),
                );
            });
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("plugins.issue_close"),
                ButtonKind::Secondary,
                Vec2::new(100.0, 32.0),
            )
            .clicked()
            {
                close = true;
            }
        },
    );
    close || dismissed
}

pub(super) fn udev_modal_status(ui: &mut egui::Ui, status: &halod_shared::types::UdevRulesStatus) {
    let count = status.generated_rule_count;
    if status.current {
        let path = status.installed_path.as_deref().unwrap_or_default();
        widgets::Banner::ok(&t!(
            "plugins.udev_modal_current",
            count = count,
            path = path
        ))
        .title_color(theme::TEXT_DIM)
        .dot(theme::STAT_GREEN)
        .show(ui);
    } else {
        let text = match &status.installed_path {
            Some(path) => t!(
                "plugins.udev_modal_outdated",
                count = status.plugins_requiring_update.len(),
                path = path
            ),
            None => t!("plugins.udev_modal_missing", count = count),
        };
        widgets::Banner::warn(&text).dot(theme::STAT_AMBER).show(ui);
    }
}
