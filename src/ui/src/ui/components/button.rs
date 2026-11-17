// SPDX-License-Identifier: GPL-3.0-or-later
//! The themed button — every clickable action button in the app routes
//! through here instead of hand-rolling `egui::Button`.

use egui::{Align2, Color32, FontId, Rect, Response, Sense, Stroke, Vec2};

use crate::ui::theme::{self, a};

/// Horizontal breathing room kept on each side of a button's label when sizing
/// it to fit — so a long (e.g. localized) label never touches the edges.
const LABEL_H_PAD: f32 = 16.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ButtonState {
    Idle,
    Disabled,
    Loading,
}

const SPINNER_SIZE: f32 = 14.0;
const SPINNER_GAP: f32 = 8.0;

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
    paint_button(
        ui,
        rect,
        resp.id,
        resp.hovered(),
        label,
        kind,
        ButtonState::Idle,
    );
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

pub fn paint_trash_glyph(p: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.3, color);
    p.line_segment(
        [c + Vec2::new(-5.0, -3.5), c + Vec2::new(5.0, -3.5)],
        stroke,
    );
    p.line_segment(
        [c + Vec2::new(-1.5, -5.5), c + Vec2::new(1.5, -5.5)],
        stroke,
    );
    p.add(egui::Shape::closed_line(
        vec![
            c + Vec2::new(-3.8, -3.5),
            c + Vec2::new(3.8, -3.5),
            c + Vec2::new(3.1, 5.5),
            c + Vec2::new(-3.1, 5.5),
        ],
        stroke,
    ));
    for dx in [-1.3, 1.3] {
        p.line_segment([c + Vec2::new(dx, -1.0), c + Vec2::new(dx, 3.5)], stroke);
    }
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
    paint_button(ui, rect, resp.id, false, label, kind, ButtonState::Disabled);
    resp
}

/// Like [`button_disabled`] but overlays a small spinner on the right side of
/// the button to indicate an async operation in progress.
pub fn button_loading(ui: &mut egui::Ui, label: &str, kind: ButtonKind, size: Vec2) -> Response {
    let size = fit_width(
        label_width(ui, label, kind, size.y) + SPINNER_SIZE + SPINNER_GAP,
        size,
    );
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    paint_button(ui, rect, resp.id, false, label, kind, ButtonState::Loading);
    ui.ctx().request_repaint();
    resp
}

fn loading_layout(center: egui::Pos2, label_w: f32) -> (egui::Pos2, f32) {
    let group = SPINNER_SIZE + SPINNER_GAP + label_w;
    let left = center.x - group / 2.0;
    (
        egui::pos2(left + SPINNER_SIZE / 2.0, center.y),
        left + SPINNER_SIZE + SPINNER_GAP,
    )
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
    paint_button(ui, rect, id, resp.hovered(), label, kind, ButtonState::Idle);
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
    fit_width(label_width(ui, label, kind, size.y), size)
}

fn label_width(ui: &egui::Ui, label: &str, kind: ButtonKind, height: f32) -> f32 {
    ui.painter()
        .layout_no_wrap(label.to_owned(), button_font(kind, height), Color32::WHITE)
        .size()
        .x
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
    state: ButtonState,
) {
    let disabled = state == ButtonState::Disabled;
    let hovered = hovered && state == ButtonState::Idle;
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let t = ui.ctx().animate_bool_with_time(id, hovered, 0.12);
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

    if state == ButtonState::Loading {
        let galley = p.layout_no_wrap(label.to_string(), font, text_color);
        let (spinner, text_left) = loading_layout(rect.center(), galley.size().x);
        super::feedback::paint_spinner(
            p,
            spinner,
            SPINNER_SIZE,
            ui.input(|i| i.time) as f32,
            text_color,
        );
        p.galley(
            egui::pos2(text_left, rect.center().y - galley.size().y / 2.0),
            galley,
            text_color,
        );
        return;
    }
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        font,
        text_color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loading_spinner_and_its_label_are_centred_together() {
        let center = egui::pos2(100.0, 20.0);
        let label_w = 60.0;
        let (spinner, text_left) = loading_layout(center, label_w);
        let group_left = spinner.x - SPINNER_SIZE / 2.0;
        let group_right = text_left + label_w;
        assert!(((group_left + group_right) / 2.0 - center.x).abs() < 1e-4);
        assert!(spinner.x + SPINNER_SIZE / 2.0 + SPINNER_GAP <= text_left + 1e-4);
    }

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
