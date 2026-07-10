// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugins page — lists device plugins found in the plugins directory and lets
//! the user enable/disable each. Importing plugin repositories is a documented
//! follow-up (a disabled affordance points at it).

use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{AppState, PluginInfo};

use crate::runtime::ipc::CommandTx;
use crate::ui::components as widgets;
use crate::ui::theme;

/// Presentation model for one plugin row — a plain-data projection of
/// `PluginInfo`, factored out so it's unit-testable without an egui frame.
#[derive(Debug, PartialEq)]
pub struct PluginRow {
    pub id: String,
    pub title: String,
    /// Capability labels joined for display (empty when none declared).
    pub caps: String,
    pub subtitle: String,
    pub enabled: bool,
}

pub fn row_of(info: &PluginInfo) -> PluginRow {
    PluginRow {
        id: info.id.clone(),
        title: info.name.clone(),
        caps: info.capabilities.join("  ·  "),
        subtitle: info.path.clone(),
        enabled: info.enabled,
    }
}

pub fn show(ui: &mut egui::Ui, state: &AppState, cmd: &CommandTx) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            widgets::page_frame(ui, |ui| page_body(ui, state, cmd));
        });
}

fn page_body(ui: &mut egui::Ui, state: &AppState, cmd: &CommandTx) {
    ui.label(
        egui::RichText::new(t!("plugins.title"))
            .font(theme::bold(22.0))
            .color(theme::TEXT),
    );
    ui.add_space(3.0);
    ui.label(
        egui::RichText::new(t!("plugins.subtitle"))
            .font(theme::body(12.0))
            .color(theme::TEXT_MUT),
    );
    ui.add_space(22.0);

    if state.plugins.is_empty() {
        empty_state(ui, state);
    } else {
        widgets::card(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for info in &state.plugins {
                plugin_row(ui, &row_of(info), cmd);
            }
        });
    }

    ui.add_space(16.0);
    widgets::card(ui, |ui| {
        let rect = row_rect(ui);
        row_label(
            ui,
            rect,
            &t!("plugins.import_title"),
            &t!("plugins.import_sub"),
        );
        ui.painter().text(
            Pos2::new(rect.right() - 8.0, rect.center().y),
            Align2::RIGHT_CENTER,
            t!("plugins.import_soon"),
            theme::body(10.5),
            theme::TEXT_FAINT,
        );
    });
}

fn empty_state(ui: &mut egui::Ui, state: &AppState) {
    widgets::card(ui, |ui| {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(t!("plugins.empty_title"))
                .font(theme::body(13.0))
                .color(theme::TEXT),
        );
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(t!("plugins.empty_hint"))
                .font(theme::body(11.0))
                .color(theme::TEXT_MUT),
        );
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(plugins_dir_hint(state))
                .font(theme::body(11.0))
                .color(theme::TEXT_FAINT),
        );
        ui.add_space(6.0);
    });
}

/// The plugins directory path shown in the empty state.
pub fn plugins_dir_hint(state: &AppState) -> String {
    if state.config_dir.is_empty() {
        "<config dir>/plugins".to_owned()
    } else {
        format!("{}/plugins", state.config_dir.trim_end_matches('/'))
    }
}

fn plugin_row(ui: &mut egui::Ui, row: &PluginRow, cmd: &CommandTx) {
    let rect = row_rect(ui);
    let title = if row.caps.is_empty() {
        row.title.clone()
    } else {
        format!("{}      {}", row.title, row.caps)
    };
    row_label(ui, rect, &title, &row.subtitle);
    if row_toggle(ui, rect, row.enabled, &row.id) {
        crate::domain::actions::plugins::set_plugin_enabled(cmd, row.id.clone(), !row.enabled);
    }
    bottom_border(ui, rect);
}

// ── Row primitives (mirrors the Settings page idiom) ────────────────────────

fn row_rect(ui: &mut egui::Ui) -> Rect {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::hover());
    rect
}

fn row_label(ui: &mut egui::Ui, rect: Rect, title: &str, subtitle: &str) {
    ui.painter().text(
        Pos2::new(rect.left(), rect.top() + 10.0),
        Align2::LEFT_TOP,
        title,
        theme::body(12.5),
        theme::TEXT,
    );
    ui.painter().text(
        Pos2::new(rect.left(), rect.top() + 28.0),
        Align2::LEFT_TOP,
        subtitle,
        theme::body(10.5),
        theme::TEXT_FAINT,
    );
}

fn bottom_border(ui: &mut egui::Ui, rect: Rect) {
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, theme::BORDER_SOFT),
    );
}

fn row_toggle(ui: &mut egui::Ui, rect: Rect, on: bool, id: &str) -> bool {
    let toggle_rect = Rect::from_min_size(
        Pos2::new(rect.right() - 36.0, rect.top() + 17.0),
        Vec2::new(28.0, 15.0),
    );
    let resp = ui.interact(
        toggle_rect,
        ui.id().with(("plugin_toggle", id)),
        Sense::click(),
    );
    let t = ui.ctx().animate_bool_with_time(resp.id, on, 0.15);
    widgets::paint_toggle(ui.painter(), toggle_rect, t);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, caps: &[&str], enabled: bool) -> PluginInfo {
        PluginInfo {
            id: id.into(),
            name: format!("{id} device"),
            path: format!("/home/u/.config/halod/plugins/{id}.lua"),
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
            enabled,
        }
    }

    #[test]
    fn row_projects_plugin_info() {
        let row = row_of(&info("kraken", &["RGB", "Fan"], true));
        assert_eq!(row.id, "kraken");
        assert_eq!(row.title, "kraken device");
        assert_eq!(row.caps, "RGB  ·  Fan");
        assert!(row.enabled);
        assert!(row.subtitle.ends_with("kraken.lua"));
    }

    #[test]
    fn row_with_no_caps_has_empty_caps_string() {
        let row = row_of(&info("plain", &[], false));
        assert_eq!(row.caps, "");
        assert!(!row.enabled);
    }

    fn state_with_config_dir(dir: &str) -> AppState {
        AppState {
            config_dir: dir.into(),
            ..AppState::default()
        }
    }

    #[test]
    fn plugins_dir_hint_appends_plugins_dir() {
        let state = state_with_config_dir("/home/u/.config/halod");
        assert_eq!(plugins_dir_hint(&state), "/home/u/.config/halod/plugins");
    }

    #[test]
    fn plugins_dir_hint_handles_trailing_slash_and_empty() {
        assert_eq!(
            plugins_dir_hint(&state_with_config_dir("/cfg/")),
            "/cfg/plugins"
        );
        assert_eq!(
            plugins_dir_hint(&state_with_config_dir("")),
            "<config dir>/plugins"
        );
    }
}
