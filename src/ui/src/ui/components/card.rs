// SPDX-License-Identifier: GPL-3.0-or-later
//! The card surface and section-header helpers shared by every page.

use egui::Stroke;

use crate::ui::theme;

/// The shared raised card surface. `body` paints inside the 20 px inner margin.
pub fn card<R>(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    card_with_margin(ui, theme::PAD_CARD, body)
}

/// Allocate a full-width clickable list row and paint the shared
/// selected/hover backgrounds plus the pointer cursor. The body paints into
/// the returned rect.
pub fn hover_row(ui: &mut egui::Ui, height: f32, selected: bool) -> (egui::Rect, egui::Response) {
    let (rect, resp) = ui.allocate_exact_size(
        egui::Vec2::new(ui.available_width(), height),
        egui::Sense::click(),
    );
    if selected {
        ui.painter().rect_filled(rect, 9.0, theme::ROW_ACTIVE);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 9.0, theme::a(theme::ROW_ACTIVE, 0.55));
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    (rect, resp)
}

/// The 28 px leading icon/logo tile every [`hover_row`] list row shares.
pub fn row_tile_rect(rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::Pos2::new(rect.left() + 8.0, rect.center().y - 14.0),
        egui::Vec2::splat(28.0),
    )
}

/// A card surface with no inner margin, for bodies that lay out edge-to-edge
/// (e.g. full-width header rows).
pub fn card_frameless<R>(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    card_with_margin(ui, egui::Margin::ZERO, body)
}

/// Same card surface as [`card`] but with a caller-chosen inner margin, for
/// tighter chrome (e.g. a compact tab strip) than the default 20 px.
pub fn card_with_margin<R>(
    ui: &mut egui::Ui,
    margin: egui::Margin,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    card_with_surface(ui, margin, theme::CARD_BG, theme::BORDER, body)
}

/// A card using caller-selected theme surfaces while retaining the shared
/// spacing, border width and corner radius.
pub fn card_with_surface<R>(
    ui: &mut egui::Ui,
    margin: egui::Margin,
    fill: egui::Color32,
    border: egui::Color32,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    egui::Frame::NONE
        .fill(fill)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(theme::RADIUS_XL)
        .inner_margin(margin)
        .show(ui, body)
        .inner
}

/// A card with a bold title row (and an optional right-aligned accessory drawn
/// by `right`).
pub fn card_titled<R>(
    ui: &mut egui::Ui,
    title: &str,
    right: impl FnOnce(&mut egui::Ui),
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    card(ui, |ui| {
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.label(
                    egui::RichText::new(title)
                        .font(theme::heading())
                        .color(theme::TEXT),
                );
            },
            right,
        );
        ui.add_space(theme::SPACE_7);
        body(ui)
    })
}

/// Tracked-caps faint section label (`CURVE SENSOR`, `ZONE`, …).
pub fn caps_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .font(theme::caption())
            .color(theme::TEXT_FAINT2),
    );
}

/// [`caps_label`] sized to the [`super::PILL_H`] control-row height so it
/// vertically centers against pills/buttons laid out beside it in a horizontal
/// row (a plain `ui.label` is text-height and rides high next to taller pills).
pub fn caps_label_inline(ui: &mut egui::Ui, text: &str) {
    let galley =
        ui.painter()
            .layout_no_wrap(text.to_string(), theme::caption(), theme::TEXT_FAINT2);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(galley.size().x, super::PILL_H),
        egui::Sense::hover(),
    );
    ui.painter().galley(
        egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0),
        galley,
        theme::TEXT_FAINT2,
    );
}

#[cfg(test)]
mod tests {
    use super::caps_label_inline;
    use crate::ui::components::{pill, PILL_H};
    use crate::ui::theme;

    /// The inline caps label occupies the same row height as a pill, so the two
    /// vertically center against each other in a shared horizontal row.
    #[test]
    fn inline_caps_label_matches_pill_row_height() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 200.0),
            )),
            ..Default::default()
        };
        let mut label_h = 0.0;
        let mut pill_h = 0.0;
        let _ = ctx.run_ui(input, |ui| {
            label_h = ui
                .horizontal(|ui| caps_label_inline(ui, "ZONE"))
                .response
                .rect
                .height();
            pill_h = ui
                .horizontal(|ui| {
                    let _ = pill(ui, "Ambiglow", true);
                })
                .response
                .rect
                .height();
        });
        assert_eq!(label_h, PILL_H);
        assert_eq!(pill_h, PILL_H);
    }
}
