// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared HaloDaemon mark and wordmark.

use egui::{Pos2, Rect, Vec2};

use crate::ui::theme;

const ICON_TEXT_GAP: f32 = 10.0;

/// Allocate and paint the standalone Halo mark.
pub fn logo_icon(ui: &mut egui::Ui, size: f32, time: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    theme::logo_icon(ui.painter(), ui.ctx(), rect, time);
    response
}

pub fn logo_size(painter: &egui::Painter, icon_size: f32, font_size: f32) -> Vec2 {
    let halo = painter.layout_no_wrap("halo".into(), theme::bold(font_size), theme::TEXT);
    let daemon = painter.layout_no_wrap(
        "daemon".into(),
        theme::bold(font_size),
        theme::hex(0x9b7fe0),
    );
    Vec2::new(
        icon_size + ICON_TEXT_GAP + halo.size().x + daemon.size().x,
        icon_size.max(halo.size().y.max(daemon.size().y)),
    )
}

/// Paint the complete mark at `origin`, returning its occupied rectangle.
pub fn paint_logo(
    painter: &egui::Painter,
    ctx: &egui::Context,
    origin: Pos2,
    icon_size: f32,
    font_size: f32,
    time: f32,
) -> Rect {
    let size = logo_size(painter, icon_size, font_size);
    let mark = Rect::from_min_size(
        Pos2::new(origin.x, origin.y + (size.y - icon_size) / 2.0),
        Vec2::splat(icon_size),
    );
    theme::logo_icon(painter, ctx, mark, time);

    let halo = painter.layout_no_wrap("halo".into(), theme::bold(font_size), theme::TEXT);
    let daemon = painter.layout_no_wrap(
        "daemon".into(),
        theme::bold(font_size),
        theme::hex(0x9b7fe0),
    );
    let text_y = origin.y + (size.y - halo.size().y) / 2.0;
    let halo_pos = Pos2::new(mark.right() + ICON_TEXT_GAP, text_y);
    painter.galley(halo_pos, halo.clone(), theme::TEXT);
    painter.galley(
        Pos2::new(halo_pos.x + halo.size().x, text_y),
        daemon,
        theme::hex(0x9b7fe0),
    );
    Rect::from_min_size(origin, size)
}
