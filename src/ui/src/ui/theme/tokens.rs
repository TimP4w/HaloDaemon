// SPDX-License-Identifier: GPL-3.0-or-later
//! Named radius / spacing / padding tokens and low-level surface painters.
// Some tokens are consumed by later design-system migration batches (radius,
// spacing, and painter sweeps); allow until every site is routed through them.
#![allow(dead_code)]

use egui::{Color32, Margin, Rect, Stroke, Vec2};

use super::{BORDER, CARD_BG, INNER_BG};

pub const RADIUS_XS: f32 = 4.0;
pub const RADIUS_SM: f32 = 7.0;
pub const RADIUS_MD: f32 = 10.0;
pub const RADIUS_LG: f32 = 12.0;
pub const RADIUS_XL: f32 = 14.0;
pub const RADIUS_2XL: f32 = 18.0;

pub const SPACE_1: f32 = 2.0;
pub const SPACE_2: f32 = 4.0;
pub const SPACE_3: f32 = 6.0;
pub const SPACE_4: f32 = 8.0;
pub const SPACE_5: f32 = 10.0;
pub const SPACE_6: f32 = 12.0;
pub const SPACE_7: f32 = 14.0;
pub const SPACE_8: f32 = 16.0;
pub const SPACE_9: f32 = 18.0;
pub const SPACE_10: f32 = 24.0;
pub const SPACE_12: f32 = 40.0;
pub const SPACE_16: f32 = 60.0;

pub const PAD_CARD: Margin = Margin::same(20);
pub const PAD_MODAL: Margin = Margin::same(18);
pub const PAD_BANNER: Margin = Margin::symmetric(14, 11);
pub const PAD_WELL: Margin = Margin::same(12);
pub const PAD_FIELD: Vec2 = Vec2::new(12.0, 9.0);

pub fn paint_surface(p: &egui::Painter, rect: Rect, radius: f32, fill: Color32, stroke: Color32) {
    p.rect_filled(rect, radius, fill);
    p.rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, stroke),
        egui::StrokeKind::Middle,
    );
}

pub fn paint_well(p: &egui::Painter, rect: Rect, radius: f32) {
    paint_surface(p, rect, radius, INNER_BG, BORDER);
}

pub fn paint_card_rect(p: &egui::Painter, rect: Rect, radius: f32) {
    paint_surface(p, rect, radius, CARD_BG, BORDER);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_surface_emits_fill_and_stroke() {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, Vec2::splat(200.0))),
            ..Default::default()
        };
        for r in [0.0, 4.0, 7.0, 10.0, 14.0, 32.0] {
            let _ = ctx.clone().run_ui(input.clone(), |ui| {
                let before = ui.painter().add(egui::Shape::Noop);
                paint_surface(
                    ui.painter(),
                    Rect::from_min_size(egui::Pos2::ZERO, Vec2::splat(50.0)),
                    r,
                    INNER_BG,
                    BORDER,
                );
                let after = ui.painter().add(egui::Shape::Noop);
                assert_eq!(after.0 - before.0, 3);
            });
        }
    }
}
