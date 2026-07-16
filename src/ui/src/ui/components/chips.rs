// SPDX-License-Identifier: GPL-3.0-or-later
//! Pills, chips, and context-menu row helpers.

use egui::{Align2, Color32, Pos2, Rect, Response, Sense, Stroke, Vec2};

use crate::ui::theme;

/// Height of a pill/chip control, shared so inline labels beside them can match.
pub const PILL_H: f32 = 31.0;

/// A rounded pill button; returns whether it was clicked. `active` paints the
/// cyan-filled selected state.
#[must_use]
pub fn pill(ui: &mut egui::Ui, label: &str, active: bool) -> bool {
    pill_styled(ui, label, active, theme::CYAN, theme::INNER_BG)
}

/// Split a chip rect into its label zone and the trailing square "×" zone.
fn chip_close_zones(rect: Rect) -> (Rect, Rect) {
    let close = Rect::from_min_max(Pos2::new(rect.max.x - rect.height(), rect.min.y), rect.max);
    let body = Rect::from_min_max(rect.min, Pos2::new(close.min.x, rect.max.y));
    (body, close)
}

/// A [`pill`]-shaped chip with a trailing "×" close zone, drawn as a single
/// widget so rows of chips wrap cleanly inside `horizontal_wrapped` (a nested
/// `horizontal` child breaks the wrapped cursor). Returns
/// `(body_clicked, close_clicked)`.
pub fn chip_closable(ui: &mut egui::Ui, label: &str) -> (bool, bool) {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), theme::body_md(), theme::TEXT_DIM);
    let h = 31.0;
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(galley.size().x + 20.0 + h, h), Sense::click());
    let (body, close) = chip_close_zones(rect);
    let close_hovered = resp.hover_pos().is_some_and(|p| close.contains(p));
    let p = ui.painter();
    p.rect_filled(rect, 8.0, theme::INNER_BG);
    p.rect_stroke(
        rect,
        8.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Middle,
    );
    p.galley(
        Pos2::new(body.min.x + 12.0, rect.center().y - galley.size().y / 2.0),
        galley,
        Color32::WHITE,
    );
    p.text(
        close.center(),
        Align2::CENTER_CENTER,
        "×",
        theme::body_lg(),
        if close_hovered {
            theme::TRAFFIC_RED
        } else {
            theme::TEXT_DIM
        },
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let clicked_at = |zone: &Rect| {
        resp.clicked()
            && resp
                .interact_pointer_pos()
                .is_some_and(|p| zone.contains(p))
    };
    (clicked_at(&body), clicked_at(&close))
}

/// Like [`pill`] but with caller-chosen `active_fill` / `inactive_fill` colors,
/// so the Home dashboard and the global pages can share one implementation
/// instead of hand-rolling near-identical pills.
#[must_use]
pub fn pill_styled(
    ui: &mut egui::Ui,
    label: &str,
    active: bool,
    active_fill: Color32,
    inactive_fill: Color32,
) -> bool {
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        theme::body_md(),
        if active {
            theme::hex(0x0a0d13)
        } else {
            theme::TEXT_DIM
        },
    );
    let w = galley.size().x + 24.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, PILL_H), Sense::click());
    let (bg, border) = if active {
        (active_fill, active_fill)
    } else {
        (inactive_fill, theme::BORDER)
    };
    ui.painter().rect_filled(rect, 8.0, bg);
    ui.painter().rect_stroke(
        rect,
        8.0,
        Stroke::new(1.0, border),
        egui::StrokeKind::Middle,
    );
    ui.painter().galley(
        Pos2::new(
            rect.center().x - galley.size().x / 2.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        Color32::WHITE,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

/// A full-width [`pill`] row with a left-aligned label, for vertical selector
/// lists (e.g. the equalizer preset column). Fills `ui.available_width()`.
#[must_use]
pub fn pill_row(ui: &mut egui::Ui, label: &str, active: bool) -> bool {
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        theme::body_md(),
        if active {
            theme::hex(0x0a0d13)
        } else {
            theme::TEXT_DIM
        },
    );
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), PILL_H), Sense::click());
    let (bg, border) = if active {
        (theme::CYAN, theme::CYAN)
    } else {
        (theme::INNER_BG, theme::BORDER)
    };
    ui.painter().rect_filled(rect, 8.0, bg);
    ui.painter().rect_stroke(
        rect,
        8.0,
        Stroke::new(1.0, border),
        egui::StrokeKind::Middle,
    );
    ui.painter().galley(
        Pos2::new(rect.min.x + 12.0, rect.center().y - galley.size().y / 2.0),
        galley,
        Color32::WHITE,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

/// A static, non-interactive capability chip.
pub fn chip(ui: &mut egui::Ui, label: &str) {
    chip_impl(
        ui,
        label,
        theme::TEXT_DIM,
        theme::hex(0x181d29),
        theme::BORDER,
    );
}

/// A static chip tinted with an accent color (e.g. assigned-zone chips on the
/// canvas instance rack). Returns a clickable response so callers can attach
/// context menus.
#[must_use]
pub fn chip_colored(ui: &mut egui::Ui, label: &str, color: Color32) -> Response {
    chip_impl(
        ui,
        label,
        color,
        color.gamma_multiply(0.14),
        color.gamma_multiply(0.3),
    )
}

fn chip_impl(
    ui: &mut egui::Ui,
    label: &str,
    text: Color32,
    fill: Color32,
    border: Color32,
) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), theme::caption(), text);
    let size = galley.size() + Vec2::new(18.0, 10.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    ui.painter().rect_filled(rect, 7.0, fill);
    ui.painter().rect_stroke(
        rect,
        7.0,
        Stroke::new(1.0, border),
        egui::StrokeKind::Middle,
    );
    ui.painter().galley(
        Pos2::new(
            rect.center().x - galley.size().x / 2.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        text,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

// ---------------------------------------------------------------------------
// Context menu helpers — shared by home.rs device cards, canvas zone chips,
// and sensor cards so every right-click menu in the app looks and feels the
// same.
// ---------------------------------------------------------------------------

/// A tracked-caps section header inside a popover / context menu.
pub fn context_menu_title(ui: &mut egui::Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(text)
            .font(theme::body(9.5))
            .color(theme::TEXT_FAINT2),
    );
    ui.add_space(2.0);
}

/// A full-width, left-aligned context-menu row with a subtle hover fill.
/// Returns the click response so callers can dispatch on click.
#[must_use]
pub fn context_menu_item(ui: &mut egui::Ui, label: &str, color: Color32) -> Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 7.0, theme::hex(0x1a2230));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    ui.painter().text(
        Pos2::new(rect.left() + 9.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        theme::body_md(),
        color,
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip_close_zones_partition_the_chip() {
        let rect = Rect::from_min_size(Pos2::new(10.0, 5.0), Vec2::new(120.0, 31.0));
        let (body, close) = chip_close_zones(rect);
        assert_eq!(close.width(), rect.height());
        assert_eq!(body.min, rect.min);
        assert_eq!(close.max, rect.max);
        assert_eq!(body.max.x, close.min.x);
        assert_eq!(body.width() + close.width(), rect.width());
    }
}
