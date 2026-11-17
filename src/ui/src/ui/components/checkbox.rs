// SPDX-License-Identifier: GPL-3.0-or-later
//! The checkbox row: a bordered, selectable row with a leading checkbox, a
//! label, an optional sub-line and an optional right-aligned meta note.

use egui::{Align2, Color32, Pos2, Rect, Response, Sense, Stroke, Vec2};

use crate::ui::theme;

const PAD: Vec2 = Vec2::new(12.0, 10.0);
const BOX: f32 = 16.0;

/// A full-width checkbox row. `meta` is a right-aligned note and its ink.
pub fn check_row(
    ui: &mut egui::Ui,
    label: &str,
    sub: Option<&str>,
    meta: Option<(&str, Color32)>,
    checked: bool,
) -> Response {
    let p = ui.painter();
    let title = p.layout_no_wrap(label.to_owned(), theme::medium(14.0), theme::TEXT);
    let sub = sub.map(|s| p.layout_no_wrap(s.to_owned(), theme::body_sm(), theme::TEXT_MUT));
    let text_h = title.size().y + sub.as_ref().map_or(0.0, |g| 2.0 + g.size().y);
    let size = Vec2::new(ui.available_width(), text_h + 2.0 * PAD.y);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let p = ui.painter();
    let (fill, border) = if checked {
        (theme::CHECKED_BG, theme::ACCENT_BORDER)
    } else if resp.hovered() {
        (theme::INNER_BG, theme::BORDER_INNER)
    } else {
        (theme::INNER_BG, theme::BORDER)
    };
    theme::paint_surface(p, rect, 8.0, fill, border);
    let top = rect.top() + PAD.y;
    let cb = Rect::from_min_size(Pos2::new(rect.left() + PAD.x, top + 2.0), Vec2::splat(BOX));
    paint_checkbox(p, cb, checked);
    let x = cb.right() + 12.0;
    let title_h = title.size().y;
    p.galley(Pos2::new(x, top), title, theme::TEXT);
    if let Some(g) = sub {
        p.galley(Pos2::new(x, top + title_h + 2.0), g, theme::TEXT_MUT);
    }
    if let Some((meta, ink)) = meta {
        p.text(
            Pos2::new(rect.right() - PAD.x, top + title_h / 2.0),
            Align2::RIGHT_CENTER,
            meta,
            theme::body_sm(),
            ink,
        );
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// A 16 px checkbox: accent-filled with a check when on, a light box when off.
pub fn paint_checkbox(p: &egui::Painter, rect: Rect, checked: bool) {
    if !checked {
        p.rect_filled(rect, 3.0, theme::TEXT);
        return;
    }
    p.rect_filled(rect, 3.0, theme::CYAN);
    let s = rect.width();
    let pt = |x: f32, y: f32| Pos2::new(rect.left() + x * s, rect.top() + y * s);
    p.add(egui::Shape::line(
        vec![pt(0.25, 0.52), pt(0.43, 0.70), pt(0.76, 0.32)],
        Stroke::new(2.0, theme::TEXT_BRIGHT),
    ));
}

/// An inline checkbox with its label. Returns the click response.
pub fn checkbox(ui: &mut egui::Ui, label: &str, checked: bool) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::body_md(), theme::TEXT_SOFT);
    let size = Vec2::new(BOX + 8.0 + galley.size().x, BOX.max(galley.size().y));
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let cb = Rect::from_min_size(
        Pos2::new(rect.left(), rect.center().y - BOX / 2.0),
        Vec2::splat(BOX),
    );
    paint_checkbox(ui.painter(), cb, checked);
    ui.painter().galley(
        Pos2::new(cb.right() + 8.0, rect.center().y - galley.size().y / 2.0),
        galley,
        theme::TEXT_SOFT,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}
