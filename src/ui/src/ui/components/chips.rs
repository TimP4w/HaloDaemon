// SPDX-License-Identifier: GPL-3.0-or-later
//! Pills, chips, and context-menu row helpers.

use egui::{Align2, Color32, Pos2, Rect, Response, Sense, Stroke, Vec2};

use crate::ui::theme;

/// Height of a pill/chip control, shared so inline labels beside them can match.
pub const PILL_H: f32 = 31.0;

/// A compact option tile sized to its label; returns whether it was clicked.
#[must_use]
pub fn pill(ui: &mut egui::Ui, label: &str, active: bool) -> bool {
    let w = ui
        .painter()
        .layout_no_wrap(label.to_string(), theme::body_md(), theme::TEXT)
        .size()
        .x
        + 24.0;
    let (_, resp) = ui.allocate_exact_size(Vec2::new(w, PILL_H), Sense::click());
    super::segmented::paint_option(ui, &resp, label, active, Align2::CENTER_CENTER);
    resp.clicked()
}

/// Wrapping pill/chip strip with the standard 7 px wrap spacing — the layout
/// every inline selector row shares; the body places its own pills.
pub fn pill_strip<R>(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(7.0, 7.0);
        body(ui)
    })
    .inner
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
    theme::paint_well(p, rect, 8.0);
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

/// A static, non-interactive capability chip.
pub fn chip(ui: &mut egui::Ui, label: &str) {
    tag(ui, label, TagTone::Neutral);
}

// ---------------------------------------------------------------------------
// Context menu helpers — shared by home.rs device cards, canvas zone chips,
// and sensor cards so every right-click menu in the app looks and feels the
// same.
// ---------------------------------------------------------------------------

/// A tracked-caps section header inside a popover / context menu.
pub fn context_menu_title(ui: &mut egui::Ui, text: &str) {
    ui.add_space(theme::SPACE_1);
    ui.label(
        egui::RichText::new(text)
            .font(theme::body(9.5))
            .color(theme::TEXT_FAINT2),
    );
    ui.add_space(theme::SPACE_1);
}

/// A full-width, left-aligned context-menu row with a subtle hover fill.
/// Returns the click response so callers can dispatch on click.
#[must_use]
pub fn context_menu_item(ui: &mut egui::Ui, label: &str, color: Color32) -> Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, theme::RADIUS_SM, theme::ROW_ACTIVE);
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

/// Semantic tone of a [`paint_tag`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TagTone {
    Neutral,
    Accent,
    Warn,
    Danger,
    Ok,
}

fn tag_colors(tone: TagTone) -> (Color32, Color32, Color32) {
    let tinted = |c: Color32| (theme::a(c, 0.12), theme::a(c, 0.35), c);
    match tone {
        TagTone::Neutral => (Color32::TRANSPARENT, theme::BORDER_INNER, theme::TEXT_DIM),
        TagTone::Accent => tinted(theme::CYAN),
        TagTone::Warn => tinted(theme::STAT_AMBER),
        TagTone::Danger => tinted(theme::OFFLINE_TEXT),
        TagTone::Ok => tinted(theme::TRAFFIC_GREEN),
    }
}

/// A small status tag whose right edge sits at `right_center`. Returns its rect.
pub fn paint_tag(p: &egui::Painter, right_center: Pos2, label: &str, tone: TagTone) -> Rect {
    let (fill, stroke, ink) = tag_colors(tone);
    let galley = p.layout_no_wrap(label.to_owned(), theme::value_sm(), ink);
    let rect = Rect::from_min_max(
        Pos2::new(
            right_center.x - galley.size().x - 16.0,
            right_center.y - 12.0,
        ),
        Pos2::new(right_center.x, right_center.y + 12.0),
    );
    p.rect_filled(rect, 5.0, fill);
    p.rect_stroke(
        rect,
        5.0,
        Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    p.galley(rect.center() - galley.size() / 2.0, galley, ink);
    rect
}

/// A [`paint_tag`] laid out in the current flow.
pub fn tag(ui: &mut egui::Ui, label: &str, tone: TagTone) {
    let w = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::value_sm(), theme::TEXT)
        .size()
        .x
        + 16.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 24.0), Sense::hover());
    paint_tag(ui.painter(), rect.right_center(), label, tone);
}
