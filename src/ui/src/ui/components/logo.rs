// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared HaloDaemon mark and wordmark.

use egui::{Pos2, Rect, Vec2};

use crate::ui::theme;

const ICON_TEXT_GAP: f32 = 9.0;
const WORDMARK: &str = "halod";

pub fn logo_size(painter: &egui::Painter, icon_size: f32, font_size: f32) -> Vec2 {
    let text = painter.layout_no_wrap(WORDMARK.into(), theme::semibold(font_size), theme::TEXT);
    Vec2::new(
        icon_size + ICON_TEXT_GAP + text.size().x,
        icon_size.max(text.size().y),
    )
}

/// Paint the Drift mark and wordmark at `origin`, returning its occupied rectangle.
pub fn paint_logo(
    painter: &egui::Painter,
    ctx: &egui::Context,
    origin: Pos2,
    icon_size: f32,
    font_size: f32,
) -> Rect {
    let size = logo_size(painter, icon_size, font_size);
    let cy = origin.y + size.y / 2.0;
    let mark = Rect::from_center_size(
        Pos2::new(origin.x + icon_size / 2.0, cy),
        Vec2::splat(icon_size),
    );
    theme::logo_icon(painter, ctx, mark, ctx.input(|i| i.time) as f32);
    ctx.request_repaint();
    painter.text(
        Pos2::new(origin.x + icon_size + ICON_TEXT_GAP, cy),
        egui::Align2::LEFT_CENTER,
        WORDMARK,
        theme::semibold(font_size),
        theme::TEXT,
    );
    Rect::from_min_size(origin, size)
}
