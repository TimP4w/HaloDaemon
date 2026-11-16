// SPDX-License-Identifier: GPL-3.0-or-later
//! The themed button — every clickable action button in the app routes
//! through here instead of hand-rolling `egui::Button`.

use egui::{Align2, Color32, FontId, Rect, Response, Sense, Stroke, Vec2};

use crate::ui::theme::{self, a};

/// Horizontal breathing room kept on each side of a button's label when sizing
/// it to fit — so a long (e.g. localized) label never touches the edges.
const LABEL_H_PAD: f32 = 16.0;

/// Visual role of a [`button`].
#[derive(Clone, Copy, PartialEq)]
pub enum ButtonKind {
    /// Bright accent fill with white ink — the affirmative action.
    Primary,
    /// Amber fill with dark ink — an affirmative action tied to a warning
    /// context (e.g. the "Update" call-to-action inside an amber update banner).
    Warn,
    /// Raised fill with a strong outline — the default / cancel action.
    Secondary,
    /// Text-only, no outline — low-emphasis inline actions.
    Ghost,
    /// Outline tinted red — destructive actions (delete/remove).
    /// Carries a faint red hint at rest and turns fully red on hover.
    Danger,
    /// Solid red fill — the confirming action of a destructive dialog.
    DangerSolid,
}

/// A themed button that animates its hover state, laid out in the current flow.
/// Returns the click response.
#[must_use]
pub fn button(ui: &mut egui::Ui, label: &str, kind: ButtonKind, size: Vec2) -> Response {
    let size = fit_size(ui, label, kind, size);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    paint_button(ui, rect, resp.id, resp.hovered(), label, kind, false);
    resp
}

/// [`button`] when `enabled`, otherwise [`button_disabled`]. Returns whether it
/// was clicked.
pub fn button_enabled(
    ui: &mut egui::Ui,
    label: &str,
    kind: ButtonKind,
    size: Vec2,
    enabled: bool,
) -> bool {
    if enabled {
        button(ui, label, kind, size).clicked()
    } else {
        button_disabled(ui, label, kind, size);
        false
    }
}

/// A square icon-only button at `rect` (close ×, gear, …). `glyph` paints the
/// icon in the given color; hover adds the row-active fill.
pub fn glyph_button_at(
    ui: &mut egui::Ui,
    rect: Rect,
    id: egui::Id,
    glyph: impl FnOnce(&egui::Painter, Rect, Color32),
) -> Response {
    let resp = ui.interact(rect, id, Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, theme::RADIUS_XS + 1.0, theme::ROW_ACTIVE);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    glyph(
        ui.painter(),
        rect,
        if resp.hovered() {
            theme::TEXT
        } else {
            theme::TEXT_MUT
        },
    );
    resp
}

/// The × glyph for [`glyph_button_at`].
pub fn paint_close_glyph(p: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.4, color);
    p.line_segment([c + Vec2::new(-3.5, -3.5), c + Vec2::new(3.5, 3.5)], stroke);
    p.line_segment([c + Vec2::new(3.5, -3.5), c + Vec2::new(-3.5, 3.5)], stroke);
}

/// A 6×3 chevron centred at `c`, pointing down (or up when `up`).
pub fn paint_chevron(p: &egui::Painter, c: egui::Pos2, up: bool, color: Color32) {
    let dy = if up { -1.5 } else { 1.5 };
    p.add(egui::Shape::line(
        vec![
            c + Vec2::new(-3.0, -dy),
            c + Vec2::new(0.0, dy),
            c + Vec2::new(3.0, -dy),
        ],
        Stroke::new(1.5, color),
    ));
}

/// Like [`button`] but non-interactive and visually dimmed (e.g. while a task
/// is in progress). Always returns a non-clicked response.
pub fn button_disabled(ui: &mut egui::Ui, label: &str, kind: ButtonKind, size: Vec2) -> Response {
    let size = fit_size(ui, label, kind, size);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    paint_button(ui, rect, resp.id, false, label, kind, true);
    resp
}

/// Like [`button_disabled`] but overlays a small spinner on the right side of
/// the button to indicate an async operation in progress.
pub fn button_loading(ui: &mut egui::Ui, label: &str, kind: ButtonKind, size: Vec2) -> Response {
    let size = fit_size(ui, label, kind, size);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    paint_button(ui, rect, resp.id, false, label, kind, true);
    let spin_center = egui::pos2(rect.right() - 14.0, rect.center().y);
    let spin_rect = Rect::from_center_size(spin_center, Vec2::splat(14.0));
    ui.new_child(egui::UiBuilder::new().max_rect(spin_rect))
        .add(egui::Spinner::new().size(12.0).color(a(theme::CYAN, 0.5)));
    resp
}

/// Same styling as [`button`], but painted at an explicit `rect` for buttons
/// positioned by hand (header/painter code) rather than by layout. The caller
/// supplies a stable `id` — flow-laid buttons get one for free from layout, but
/// positioned ones must provide their own so hover/click survive across frames.
#[must_use]
pub fn button_at(
    ui: &mut egui::Ui,
    rect: Rect,
    id: egui::Id,
    label: &str,
    kind: ButtonKind,
) -> Response {
    let resp = ui.interact(rect, id, Sense::click());
    paint_button(ui, rect, id, resp.hovered(), label, kind, false);
    resp
}

/// The label font for `kind` at button height `h`: 13 px on small (≤ 32 px)
/// buttons, 14 px otherwise; bolder for the filled call-to-action kinds.
fn button_font(kind: ButtonKind, h: f32) -> FontId {
    let size = if h <= 32.0 { 13.0 } else { 14.0 };
    match kind {
        ButtonKind::Primary | ButtonKind::Warn | ButtonKind::DangerSolid => theme::semibold(size),
        ButtonKind::Secondary | ButtonKind::Ghost | ButtonKind::Danger => theme::medium(size),
    }
}

/// Corner radius at button height `h`: 6 px small, 8 px otherwise.
fn button_radius(h: f32) -> f32 {
    if h <= 32.0 {
        6.0
    } else {
        8.0
    }
}

/// The rendered size of a flow-laid button: the requested `size`, widened when
/// the label (measured in its own font) plus padding needs more room.
fn fit_size(ui: &egui::Ui, label: &str, kind: ButtonKind, size: Vec2) -> Vec2 {
    let text_w = ui
        .painter()
        .layout_no_wrap(label.to_owned(), button_font(kind, size.y), Color32::WHITE)
        .size()
        .x;
    fit_width(text_w, size)
}

/// Pure sizing rule behind [`fit_size`]: never shrink below `size`, but widen to
/// fit `text_w` plus symmetric padding.
fn fit_width(text_w: f32, size: Vec2) -> Vec2 {
    Vec2::new(size.x.max(text_w + LABEL_H_PAD * 2.0), size.y)
}

/// Shared paint pass for [`button`]/[`button_at`]/[`button_disabled`]: the single
/// source of button fill/stroke/text styling. `disabled` suppresses hover
/// animation and dims the colors to indicate the button is inert.
fn paint_button(
    ui: &egui::Ui,
    rect: Rect,
    id: egui::Id,
    hovered: bool,
    label: &str,
    kind: ButtonKind,
    disabled: bool,
) {
    if hovered && !disabled {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let t = if disabled {
        0.0
    } else {
        ui.ctx().animate_bool_with_time(id, hovered, 0.12)
    };
    let p = ui.painter();

    let font = button_font(kind, rect.height());
    let (fill, stroke, text_color) = match kind {
        ButtonKind::Primary => (
            theme::lerp_color(theme::CYAN, a(theme::CYAN, 0.85), t),
            Stroke::NONE,
            if disabled {
                a(theme::ON_ACCENT, 0.5)
            } else {
                theme::ON_ACCENT
            },
        ),
        ButtonKind::Warn => (
            theme::lerp_color(a(theme::STAT_AMBER, 0.92), theme::STAT_AMBER, t),
            Stroke::NONE,
            if disabled {
                a(theme::ON_ACCENT, 0.5)
            } else {
                theme::ON_ACCENT
            },
        ),
        ButtonKind::Secondary => (
            if disabled {
                a(theme::RAISED_BG, 0.45)
            } else {
                theme::lerp_color(theme::RAISED_BG, theme::ROW_ACTIVE, t)
            },
            Stroke::new(
                1.0,
                if disabled {
                    a(theme::BORDER_INNER, 0.45)
                } else {
                    theme::BORDER_INNER
                },
            ),
            if disabled {
                a(theme::TEXT, 0.45)
            } else {
                theme::TEXT
            },
        ),
        ButtonKind::Ghost => (
            theme::lerp_color(Color32::TRANSPARENT, theme::ROW_ACTIVE, t),
            Stroke::NONE,
            if disabled {
                a(theme::TEXT_DIM, 0.45)
            } else {
                theme::lerp_color(theme::TEXT_DIM, theme::TEXT, t)
            },
        ),
        ButtonKind::DangerSolid => (
            theme::lerp_color(theme::DANGER, a(theme::DANGER, 0.85), t),
            Stroke::NONE,
            if disabled {
                a(theme::TEXT_BRIGHT, 0.5)
            } else {
                theme::TEXT_BRIGHT
            },
        ),
        ButtonKind::Danger => (
            theme::lerp_color(Color32::TRANSPARENT, a(theme::OFFLINE, 0.12), t),
            Stroke::new(
                1.0,
                if disabled {
                    a(theme::BORDER, 0.4)
                } else {
                    theme::lerp_color(theme::DANGER_BORDER, theme::OFFLINE, t)
                },
            ),
            if disabled {
                a(theme::OFFLINE_TEXT, 0.4)
            } else {
                theme::lerp_color(theme::OFFLINE_TEXT, theme::OFFLINE, t)
            },
        ),
    };

    // Buttons use fill, border, and hover contrast only. Animated halos made
    // ordinary actions such as “Update all” look like alerts and competed
    // with the content around them.
    let radius = button_radius(rect.height());
    p.rect_filled(rect, radius, fill);
    if stroke != Stroke::NONE {
        p.rect_stroke(rect, radius, stroke, egui::StrokeKind::Inside);
    }

    let text = label.to_string();
    p.text(rect.center(), Align2::CENTER_CENTER, text, font, text_color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_buttons_use_the_smaller_label_and_radius() {
        for kind in [
            ButtonKind::Primary,
            ButtonKind::Secondary,
            ButtonKind::DangerSolid,
        ] {
            assert!(button_font(kind, 30.0).size < button_font(kind, 36.0).size);
        }
        assert!(button_radius(30.0) < button_radius(36.0));
    }

    #[test]
    fn fit_width_keeps_requested_size_when_label_fits() {
        // A narrow label leaves the button at its requested width and height.
        let out = fit_width(40.0, Vec2::new(130.0, 36.0));
        assert_eq!(out, Vec2::new(130.0, 36.0));
    }

    #[test]
    fn fit_width_grows_to_fit_a_wide_label() {
        // A label wider than the requested size widens the button (label +
        // padding on both sides) while keeping the height.
        let out = fit_width(180.0, Vec2::new(130.0, 36.0));
        assert_eq!(out, Vec2::new(180.0 + LABEL_H_PAD * 2.0, 36.0));
    }
}
