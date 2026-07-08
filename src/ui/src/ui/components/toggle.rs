// SPDX-License-Identifier: GPL-3.0-or-later
//! The pill toggle switch.

use egui::{Color32, Pos2, Rect, Sense, Vec2};

use crate::ui::theme;

/// Paint a pill toggle knob into `rect` for animation phase `t` (0 = off,
/// 1 = on). Pure paint — callers that position via a painter use this directly.
pub fn paint_toggle(painter: &egui::Painter, rect: Rect, t: f32) {
    let track = theme::lerp_color(theme::hex(0x222936), theme::CYAN, t);
    let r = rect.height() / 2.0;
    painter.rect_filled(rect, r, track);
    let knob = r - 2.5;
    let cx = rect.left() + (r) + t * (rect.width() - 2.0 * r);
    painter.circle_filled(Pos2::new(cx, rect.center().y), knob, Color32::WHITE);
}

/// An interactive pill toggle. Allocates a `34×18` knob and returns the
/// (possibly toggled) state.
pub fn toggle(ui: &mut egui::Ui, on: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(34.0, 18.0), Sense::click());
    let mut val = on;
    if resp.clicked() {
        val = !val;
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, val, 0.1);
    paint_toggle(ui.painter(), rect, t);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    val
}
