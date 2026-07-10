// SPDX-License-Identifier: GPL-3.0-or-later
//! Device/battery glyphs and the effect-picker cell.

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::DeviceType;

use crate::ui::icons;
use crate::ui::theme;

/// The shared device chip: the device-type glyph drawn in white with no
/// background fill. Replaces the old 2–3 letter code chips so every
/// surface (home cards/rows, sidebar, lighting targets, device header, radar)
/// uses one component.
pub fn device_badge(
    p: &egui::Painter,
    rect: Rect,
    ty: DeviceType,
    color: Color32,
    rounding: f32,
    stroke_w: f32,
) {
    let _ = (rounding, color, stroke_w); // retained for call-site compatibility
    let glyph = Rect::from_center_size(rect.center(), Vec2::splat(rect.height() * 0.8));
    icons::draw_device(p, glyph, ty, Color32::WHITE);
}

/// Paint a battery glyph — outlined cell, right-side nub, and a `level`-percent
/// fill — inside `body`. Callers size and position `body` themselves.
pub fn battery_glyph(p: &egui::Painter, body: Rect, level: u8, color: Color32) {
    p.rect_stroke(body, 2.5, Stroke::new(1.5, color), egui::StrokeKind::Middle);
    let nub = Rect::from_min_size(
        Pos2::new(body.right(), body.center().y - body.height() * 0.2),
        Vec2::new(2.0, body.height() * 0.4),
    );
    p.rect_filled(nub, 1.0, color);
    let inner = body.shrink(2.5);
    let fill = Rect::from_min_size(
        inner.min,
        Vec2::new(inner.width() * (level as f32 / 100.0), inner.height()),
    );
    p.rect_filled(fill, 1.0, color);
}

/// Paint a connection glyph inside `body`: ascending signal bars for a wireless
/// link, a plug for a wired one. Callers size and position `body` themselves.
pub fn connection_glyph(p: &egui::Painter, body: Rect, wireless: bool, color: Color32) {
    if wireless {
        let (bw, gap) = (3.0, 3.0);
        for i in 0..3 {
            let h = body.height() * (0.4 + 0.3 * i as f32);
            let x = body.left() + i as f32 * (bw + gap);
            let bar = Rect::from_min_max(
                Pos2::new(x, body.bottom() - h),
                Pos2::new(x + bw, body.bottom()),
            );
            p.rect_filled(bar, 1.0, color);
        }
    } else {
        let cy = body.center().y;
        let h = body.height();
        let plug = Rect::from_min_max(
            Pos2::new(body.center().x, cy - h * 0.3),
            Pos2::new(body.center().x + h * 0.55, cy + h * 0.3),
        );
        let stroke = Stroke::new(1.5, color);
        p.line_segment(
            [Pos2::new(body.left(), cy), Pos2::new(plug.left(), cy)],
            stroke,
        );
        p.rect_filled(plug, 1.5, color);
        for dy in [-h * 0.18, h * 0.18] {
            p.line_segment(
                [
                    Pos2::new(plug.right(), cy + dy),
                    Pos2::new(plug.right() + 4.0, cy + dy),
                ],
                stroke,
            );
        }
    }
}

/// Preview swatch shown at the top of an [`effect_cell`].
pub enum CellPreview {
    /// A solid representative color.
    Solid(Color32),
    /// The logo RGB spectrum, for effects with no single color.
    Spectrum,
    /// A sparkline of `[0,1]` samples — e.g. a generator's brightness shape,
    /// so the picker shows what the waveform looks like rather than a color.
    Curve(Vec<f32>),
}

/// An effect-picker cell: a rounded card with a top preview strip and a bottom
/// label. `strip_h` sizes the preview; `glow` adds the active cyan bloom +
/// thicker border used by the global lighting page (the per-device tab passes
/// `false`). Returns whether it was clicked. Shared by both lighting screens.
#[allow(clippy::too_many_arguments)]
pub fn effect_cell(
    ui: &mut egui::Ui,
    label: &str,
    active: bool,
    preview: CellPreview,
    w: f32,
    height: f32,
    strip_h: f32,
    glow: bool,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, height), Sense::click());
    let p = ui.painter();
    p.rect_filled(rect, 9.0, theme::INNER_BG);
    p.rect_stroke(
        rect,
        9.0,
        Stroke::new(
            if active && glow { 1.5 } else { 1.0 },
            if active { theme::CYAN } else { theme::BORDER },
        ),
        egui::StrokeKind::Middle,
    );
    if active && glow {
        theme::glow(p, rect.center(), 28.0, theme::CYAN, 0.04);
    }
    let strip = Rect::from_min_size(
        Pos2::new(rect.left() + 10.0, rect.top() + 10.0),
        Vec2::new(rect.width() - 20.0, strip_h),
    );
    match preview {
        CellPreview::Solid(c) => {
            p.rect_filled(strip, 5.0, c);
        }
        CellPreview::Spectrum => theme::h_gradient(p, strip, &theme::LOGO_STOPS),
        CellPreview::Curve(samples) => {
            p.rect_filled(strip, 5.0, theme::INNER_BG);
            if samples.len() >= 2 {
                let n = samples.len() - 1;
                let pts: Vec<Pos2> = samples
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| {
                        Pos2::new(
                            strip.left() + (i as f32 / n as f32) * strip.width(),
                            strip.bottom() - v.clamp(0.0, 1.0) * strip.height(),
                        )
                    })
                    .collect();
                p.add(egui::Shape::line(pts, Stroke::new(1.5, theme::CYAN)));
            }
        }
    }
    p.text(
        Pos2::new(rect.left() + 10.0, rect.bottom() - 12.0),
        Align2::LEFT_CENTER,
        label,
        theme::body(11.5),
        if active { theme::TEXT } else { theme::TEXT_DIM },
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}
