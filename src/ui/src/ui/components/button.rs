// SPDX-License-Identifier: GPL-3.0-or-later
//! The themed button — every clickable action button in the app routes
//! through here instead of hand-rolling `egui::Button`.

use egui::{Align2, Color32, Rect, Response, Sense, Stroke, Vec2};

use crate::ui::theme::{self, a};

/// Visual role of a [`button`].
#[derive(Clone, Copy, PartialEq)]
pub enum ButtonKind {
    /// Bright accent fill with dark ink — the affirmative action.
    Primary,
    /// Like [`Primary`] but with a trailing `→` arrow after the label.
    PrimaryArrow,
    /// Transparent with a hairline border — secondary / cancel.
    Ghost,
    /// Like [`Ghost`] but tinted red — destructive actions (delete/remove).
    /// Carries a faint red hint at rest and turns fully red on hover.
    Danger,
}

/// A themed button that animates its hover state, laid out in the current flow.
/// Returns the click response.
#[must_use]
pub fn button(ui: &mut egui::Ui, label: &str, kind: ButtonKind, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    paint_button(ui, rect, resp.id, resp.hovered(), label, kind, false);
    resp
}

/// Like [`button`] but non-interactive and visually dimmed (e.g. while a task
/// is in progress). Always returns a non-clicked response.
pub fn button_disabled(ui: &mut egui::Ui, label: &str, kind: ButtonKind, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    paint_button(ui, rect, resp.id, false, label, kind, true);
    resp
}

/// Like [`button_disabled`] but overlays a small spinner on the right side of
/// the button to indicate an async operation in progress.
pub fn button_loading(ui: &mut egui::Ui, label: &str, kind: ButtonKind, size: Vec2) -> Response {
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

    let (fill, stroke, text_color, font) = match kind {
        ButtonKind::Primary | ButtonKind::PrimaryArrow => (
            theme::lerp_color(a(theme::CYAN, 0.92), theme::CYAN, t),
            Stroke::NONE,
            if disabled {
                a(theme::hex(0x0a0d13), 0.5)
            } else {
                theme::hex(0x0a0d13)
            },
            theme::semibold(11.5),
        ),
        ButtonKind::Ghost => (
            Color32::TRANSPARENT,
            Stroke::new(
                1.0,
                if disabled {
                    a(theme::BORDER, 0.4)
                } else {
                    theme::lerp_color(theme::BORDER, theme::hex(0x2a3446), t)
                },
            ),
            if disabled {
                a(theme::TEXT_DIM, 0.4)
            } else {
                theme::lerp_color(theme::TEXT_DIM, theme::TEXT, t)
            },
            theme::body(11.5),
        ),
        ButtonKind::Danger => (
            theme::lerp_color(Color32::TRANSPARENT, a(theme::OFFLINE, 0.12), t),
            Stroke::new(
                1.0,
                if disabled {
                    a(theme::BORDER, 0.4)
                } else {
                    theme::lerp_color(a(theme::OFFLINE, 0.45), theme::OFFLINE, t)
                },
            ),
            if disabled {
                a(theme::OFFLINE_TEXT, 0.4)
            } else {
                theme::lerp_color(theme::OFFLINE_TEXT, theme::OFFLINE, t)
            },
            theme::body(11.5),
        ),
    };

    if matches!(kind, ButtonKind::Primary | ButtonKind::PrimaryArrow) {
        let time = ui.ctx().input(|i| i.time) as f32;
        let pulse = 0.5 + 0.5 * (time * 2.0).sin();
        let alpha = 0.28 + 0.10 * pulse + 0.32 * t;
        let blur = 20.0 + 5.0 * pulse + 16.0 * t;
        theme::halo(p, rect, 7.0, a(theme::CYAN, alpha), blur);
        ui.ctx().request_repaint();
    }
    p.rect_filled(rect, 7.0, fill);
    if stroke != Stroke::NONE {
        p.rect_stroke(rect, 7.0, stroke, egui::StrokeKind::Middle);
    }

    let text = if matches!(kind, ButtonKind::PrimaryArrow) {
        format!("{label}   →")
    } else {
        label.to_string()
    };
    p.text(rect.center(), Align2::CENTER_CENTER, text, font, text_color);
}
