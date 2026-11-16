// SPDX-License-Identifier: GPL-3.0-or-later
//! The pill toggle switch.

use egui::{Color32, Pos2, Rect, Sense, Vec2};

use crate::ui::theme;

fn paint_toggle(painter: &egui::Painter, rect: Rect, t: f32) {
    let track = theme::lerp_color(theme::BORDER_INNER, theme::CYAN, t);
    let r = rect.height() / 2.0;
    painter.rect_filled(rect, r, track);
    let knob = r - 2.5;
    let cx = rect.left() + (r) + t * (rect.width() - 2.0 * r);
    painter.circle_filled(Pos2::new(cx, rect.center().y), knob, Color32::WHITE);
}

pub const TOGGLE_SIZE: Vec2 = Vec2::new(40.0, 22.0);

/// A toggle painted at `rect`. `locked` shows it dimmed and ignores clicks
/// (e.g. while a change is applying). Returns the response; `clicked()` means
/// the user asked to flip it.
pub fn toggle_at(
    ui: &mut egui::Ui,
    rect: Rect,
    id: egui::Id,
    on: bool,
    locked: bool,
) -> egui::Response {
    let resp = ui.interact(
        rect,
        id,
        if locked {
            Sense::hover()
        } else {
            Sense::click()
        },
    );
    let t = ui.ctx().animate_bool_with_time(resp.id, on, 0.12);
    paint_toggle(ui.painter(), rect, t);
    if locked {
        ui.painter()
            .rect_filled(rect, rect.height() / 2.0, theme::a(theme::CARD_BG, 0.55));
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(if locked {
            egui::CursorIcon::NotAllowed
        } else {
            egui::CursorIcon::PointingHand
        });
    }
    resp
}

/// [`toggle_at`] laid out in the current flow.
pub fn toggle_ex(ui: &mut egui::Ui, on: bool, locked: bool) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(TOGGLE_SIZE, Sense::hover());
    let id = ui.auto_id_with("toggle");
    toggle_at(ui, rect, id, on, locked)
}

/// An interactive toggle in the current flow. Returns the (possibly toggled) state.
pub fn toggle(ui: &mut egui::Ui, on: bool) -> bool {
    on ^ toggle_ex(ui, on, false).clicked()
}
