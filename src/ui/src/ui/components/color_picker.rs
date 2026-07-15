// SPDX-License-Identifier: GPL-3.0-or-later
//! The shared RGB colour picker: a large swatch (click → full HSV popup), a
//! preset swatch grid, a rainbow hue slider, and a manual `#rrggbb` input.
//! This is the single colour-picking widget reused across the app — do not
//! hand-roll another.

use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::RgbColor;

use crate::ui::theme;

pub fn rgb_to_color32(c: RgbColor) -> Color32 {
    Color32::from_rgb(c.r, c.g, c.b)
}

pub fn color32_to_rgb(c: Color32) -> RgbColor {
    RgbColor {
        r: c.r(),
        g: c.g(),
        b: c.b(),
    }
}

/// `#rrggbb` for a swatch/readout label.
pub fn hex_label(c: RgbColor) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

pub const SWATCHES: [u32; 12] = [
    0xef5f63, 0xfb7185, 0xf472b6, 0xa78bfa, 0x818cf8, 0x60a5fa, 0x38bdf8, 0x22d3ee, 0x2dd4bf,
    0x34d399, 0xfbbf24, 0xffffff,
];

pub const MINI_SWATCHES: [u32; 7] = [
    0xef5f63, 0xfbbf24, 0x34d399, 0x38bdf8, 0x8b6fd8, 0xf472b6, 0xffffff,
];

pub fn color_swatch_row(ui: &mut egui::Ui, current: RgbColor) -> Option<RgbColor> {
    const D: f32 = 22.0;
    let mut out: Option<RgbColor> = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(9.0, 9.0);
        let mut matched_preset = false;
        for &hexv in &MINI_SWATCHES {
            let sc = theme::hex(hexv);
            let selected = color32_to_rgb(sc) == current;
            matched_preset |= selected;
            if mini_swatch(ui, D, sc, selected) {
                out = Some(color32_to_rgb(sc));
            }
        }

        let popup_id = ui.unique_id().with("mini_color");
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(D), Sense::click());
        let c32 = rgb_to_color32(current);
        let center = rect.center();
        let p = ui.painter();
        p.circle_filled(center, D / 2.0, c32);
        let (ring_w, ring_c) = if matched_preset {
            (1.5, theme::BORDER)
        } else {
            (2.0, theme::CYAN)
        };
        p.circle_stroke(center, D / 2.0, Stroke::new(ring_w, ring_c));
        p.text(
            center,
            egui::Align2::CENTER_CENTER,
            "+",
            theme::mono(11.0),
            mini_glyph_color(c32),
        );
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let mut c_pick = c32;
        egui::Popup::menu(&resp)
            .id(popup_id)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                const W: f32 = 216.0;
                ui.set_width(W);
                ui.spacing_mut().slider_width = W - 8.0;
                if egui::color_picker::color_picker_color32(
                    ui,
                    &mut c_pick,
                    egui::color_picker::Alpha::Opaque,
                ) {
                    out = Some(color32_to_rgb(c_pick));
                }
            });
    });
    out
}

fn mini_swatch(ui: &mut egui::Ui, d: f32, color: Color32, selected: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(d), Sense::click());
    let center = rect.center();
    ui.painter().circle_filled(center, d / 2.0, color);
    if selected {
        ui.painter()
            .circle_stroke(center, d / 2.0 + 1.5, Stroke::new(2.0, theme::CYAN));
    } else if resp.hovered() {
        ui.painter().circle_stroke(
            center,
            d / 2.0 + 1.5,
            Stroke::new(2.0, theme::a(theme::CYAN, 0.4)),
        );
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

fn mini_glyph_color(c: Color32) -> Color32 {
    let luma = 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
    if luma > 140.0 {
        theme::hex(0x0a0d13)
    } else {
        Color32::WHITE
    }
}

fn preset_cols(avail: f32) -> usize {
    (((avail - 76.0 + 7.0) / 27.0).floor() as usize).clamp(1, 6)
}

pub fn color_picker(ui: &mut egui::Ui, current: RgbColor) -> Option<RgbColor> {
    // Use hand-laid rects to avoid egui::Grid width leakage.
    let avail = ui.available_width();
    ui.scope(|ui| {
        ui.set_max_width(avail);
        color_picker_inner(ui, avail, current)
    })
    .inner
}

fn color_picker_inner(ui: &mut egui::Ui, avail: f32, current: RgbColor) -> Option<RgbColor> {
    let mut out: Option<RgbColor> = None;
    let c32 = rgb_to_color32(current);

    let cols = preset_cols(avail);

    // Large swatch matches the preset grid height.
    const CELL: f32 = 20.0;
    const GAP: f32 = 7.0;
    let grid_w = cols as f32 * CELL + (cols.saturating_sub(1)) as f32 * GAP;
    let rows = SWATCHES.len().div_ceil(cols);
    let grid_h = rows as f32 * CELL + (rows.saturating_sub(1)) as f32 * GAP;

    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(12.0, 0.0);

        let popup_id = ui.unique_id().with("color_popup");
        let (swatch_rect, swatch_resp) =
            ui.allocate_exact_size(Vec2::new(64.0, grid_h), Sense::click());
        let painter = ui.painter();
        painter.rect_filled(swatch_rect, 10.0, c32);
        theme::halo(painter, swatch_rect, 10.0, c32, 26.0);
        if swatch_resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let mut c_pick = c32;
        egui::Popup::menu(&swatch_resp)
            .id(popup_id)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                const W: f32 = 216.0;
                ui.set_width(W);
                ui.spacing_mut().slider_width = W - 8.0;
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.spacing_mut().button_padding = Vec2::new(4.0, 3.0);
                ui.spacing_mut().interact_size.x = 30.0;
                if egui::color_picker::color_picker_color32(
                    ui,
                    &mut c_pick,
                    egui::color_picker::Alpha::Opaque,
                ) {
                    out = Some(color32_to_rgb(c_pick));
                }
            });

        let (grid_rect, _) = ui.allocate_exact_size(Vec2::new(grid_w, grid_h), Sense::hover());
        for (i, &hexv) in SWATCHES.iter().enumerate() {
            let (cx, cy) = (i % cols, i / cols);
            let r = Rect::from_min_size(
                Pos2::new(
                    grid_rect.left() + cx as f32 * (CELL + GAP),
                    grid_rect.top() + cy as f32 * (CELL + GAP),
                ),
                Vec2::splat(CELL),
            );
            let resp = ui.interact(r, ui.unique_id().with(("swatch", i)), Sense::click());
            let sc = theme::hex(hexv);
            ui.painter().rect_filled(r, 6.0, sc);
            if resp.hovered() {
                ui.painter().rect_stroke(
                    r,
                    6.0,
                    Stroke::new(2.0, theme::CYAN),
                    egui::StrokeKind::Middle,
                );
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() {
                out = Some(color32_to_rgb(sc));
            }
        }
    });

    ui.add_space(14.0);
    if let Some(nc) = hue_slider(ui, current) {
        out = Some(nc);
    }

    ui.add_space(12.0);
    let hex_id = ui.unique_id().with("hexbuf");
    let mut buf = ui
        .data(|d| d.get_temp::<String>(hex_id))
        .unwrap_or_default();
    if !ui.memory(|m| m.has_focus(hex_id)) {
        buf = hex_label(out.unwrap_or(current));
    }
    let resp = ui.add_sized(
        Vec2::new(ui.available_width(), 32.0),
        egui::TextEdit::singleline(&mut buf)
            .id(hex_id)
            .font(theme::mono(12.0))
            .margin(egui::vec2(10.0, 8.0))
            .hint_text("#rrggbb"),
    );
    if resp.changed() {
        if let Some(c) = parse_hex(&buf) {
            out = Some(c);
        }
    }
    ui.data_mut(|d| d.insert_temp(hex_id, buf));

    out
}

/// Parse a `#rrggbb` / `rrggbb` hex string into an [`RgbColor`]. Returns `None`
/// unless the input is exactly six hexadecimal digits.
pub fn parse_hex(s: &str) -> Option<RgbColor> {
    let s = s.trim();
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(RgbColor {
        r: u8::from_str_radix(&s[0..2], 16).ok()?,
        g: u8::from_str_radix(&s[2..4], 16).ok()?,
        b: u8::from_str_radix(&s[4..6], 16).ok()?,
    })
}

/// A rainbow hue slider. Returns the new colour (keeping reasonable S/V) when the
/// user drags it.
fn hue_slider(ui: &mut egui::Ui, current: RgbColor) -> Option<RgbColor> {
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 16.0),
        Sense::click_and_drag(),
    );
    let p = ui.painter();
    let track = Rect::from_min_max(
        Pos2::new(rect.left(), rect.center().y - 3.0),
        Pos2::new(rect.right(), rect.center().y + 3.0),
    );
    const HUE_STOPS: &[(f32, u32)] = &[
        (0.0, 0xff0000),
        (1.0 / 6.0, 0xffff00),
        (2.0 / 6.0, 0x00ff00),
        (3.0 / 6.0, 0x00ffff),
        (4.0 / 6.0, 0x0000ff),
        (5.0 / 6.0, 0xff00ff),
        (1.0, 0xff0000),
    ];
    draw_h_gradient_rect(p, track, HUE_STOPS, 3.0);

    let (h, s, v) = rgb_to_hsv(current);
    let cx = rect.left() + (h / 360.0) * rect.width();

    let mut out = None;
    if resp.dragged() || resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let t = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            out = Some(hsv_to_rgb(t * 360.0, s.max(0.7), v.max(0.7)));
        }
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    p.circle_filled(Pos2::new(cx, rect.center().y), 7.0, theme::hex(0xdfe6f2));
    out
}

/// Draw a horizontal multi-stop gradient into `rect` with corner rounding `r`.
fn draw_h_gradient_rect(painter: &egui::Painter, rect: Rect, stops: &[(f32, u32)], r: f32) {
    if stops.len() < 2 {
        return;
    }
    let mut mesh = egui::Mesh::default();
    for (i, &(t, hexv)) in stops.iter().enumerate() {
        let x = rect.left() + t * rect.width();
        let c = theme::hex(hexv);
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(Pos2::new(x, rect.top()), c);
        mesh.colored_vertex(Pos2::new(x, rect.bottom()), c);
        if i > 0 {
            mesh.add_triangle(base - 2, base - 1, base);
            mesh.add_triangle(base - 1, base, base + 1);
        }
    }
    painter.add(egui::Shape::mesh(mesh));
    theme::round_corners(painter, rect, r, theme::CARD_BG);
}

/// RGB → HSV (h: 0–360, s/v: 0–1) via egui's `Hsva`.
fn rgb_to_hsv(c: RgbColor) -> (f32, f32, f32) {
    let hsva = egui::ecolor::Hsva::from(rgb_to_color32(c));
    (hsva.h * 360.0, hsva.s, hsva.v)
}

/// HSV (h: 0–360, s/v: 0–1) → RGB via egui's `Hsva`.
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> RgbColor {
    let c32 = Color32::from(egui::ecolor::Hsva::new(
        (h / 360.0).rem_euclid(1.0),
        s,
        v,
        1.0,
    ));
    color32_to_rgb(c32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_cols_row_fits_within_available_width() {
        // For any plausible card width the swatch (64) + gap (12) + grid row
        // (cols*20 + (cols-1)*7) must not exceed the available width.
        for avail in [120.0_f32, 150.0, 180.0, 208.0, 260.0, 369.0, 600.0] {
            let cols = preset_cols(avail);
            assert!((1..=6).contains(&cols));
            let row = 64.0 + 12.0 + cols as f32 * 20.0 + (cols as f32 - 1.0) * 7.0;
            assert!(
                row <= avail,
                "cols {cols} row {row} overflows avail {avail}"
            );
        }
        // Narrow sidebar drops below 6 columns; wide panel keeps the full 6.
        assert!(preset_cols(208.0) < 6);
        assert_eq!(preset_cols(369.0), 6);
    }

    #[test]
    fn mini_glyph_color_contrasts_with_fill() {
        assert_eq!(mini_glyph_color(Color32::WHITE), theme::hex(0x0a0d13));
        assert_eq!(mini_glyph_color(Color32::BLACK), Color32::WHITE);
        assert_eq!(mini_glyph_color(theme::hex(0x8b6fd8)), Color32::WHITE);
    }

    #[test]
    fn rgb_color32_round_trips() {
        for c in [
            RgbColor { r: 0, g: 0, b: 0 },
            RgbColor {
                r: 90,
                g: 209,
                b: 232,
            },
            RgbColor {
                r: 255,
                g: 255,
                b: 255,
            },
        ] {
            assert_eq!(color32_to_rgb(rgb_to_color32(c)), c);
        }
    }

    #[test]
    fn parse_hex_accepts_six_digits_with_optional_hash() {
        let cyan = RgbColor {
            r: 0x38,
            g: 0xbd,
            b: 0xf8,
        };
        assert_eq!(parse_hex("#38bdf8"), Some(cyan));
        assert_eq!(parse_hex("38bdf8"), Some(cyan));
        assert_eq!(
            parse_hex("  #FFFFFF "),
            Some(RgbColor {
                r: 255,
                g: 255,
                b: 255
            })
        );
        assert_eq!(parse_hex(&hex_label(cyan)), Some(cyan));
    }

    #[test]
    fn parse_hex_rejects_malformed() {
        assert_eq!(parse_hex(""), None);
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("#1234567"), None);
        assert_eq!(parse_hex("#gggggg"), None);
    }

    #[test]
    fn hsv_round_trips_pure_colors() {
        for (r, g, b) in [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 0)] {
            let c = RgbColor { r, g, b };
            let (h, s, v) = rgb_to_hsv(c);
            let c2 = hsv_to_rgb(h, s, v);
            let diff = |a: u8, b: u8| (a as i32 - b as i32).unsigned_abs();
            assert!(diff(c.r, c2.r) <= 1 && diff(c.g, c2.g) <= 1 && diff(c.b, c2.b) <= 1);
        }
    }

    #[test]
    fn hue_slider_math_preserves_saturation_floor_on_gray() {
        let gray = RgbColor {
            r: 128,
            g: 128,
            b: 128,
        };
        let (_, s, _) = rgb_to_hsv(gray);
        assert!(s < 0.01);
        // The slider clamps S/V to 0.7 so a vivid hue is always producible.
        assert_ne!(hsv_to_rgb(120.0, 0.7_f32.max(s), 0.7), gray);
    }

    #[test]
    fn hex_label_is_lowercase_six_digits() {
        assert_eq!(
            hex_label(RgbColor {
                r: 0x5a,
                g: 0xd1,
                b: 0xe8
            }),
            "#5ad1e8"
        );
    }

    #[test]
    fn two_color_pickers_in_one_panel_keep_separate_state() {
        // Two pickers under one parent (LCD editor: widget color + screen
        // accent) must not share ids. Sibling child Uis all get the same
        // stable `ui.id()`, so the picker derives its popup/swatch/hex ids
        // from `unique_id()`; each picker stores one hex-input String, so a
        // clash leaves only one entry, and instability across frames would
        // grow the count.
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 1000.0))),
            ..Default::default()
        };
        for _ in 0..2 {
            let _ = ctx.run_ui(input(), |ui| {
                let c = RgbColor { r: 1, g: 2, b: 3 };
                let _ = color_picker(ui, c);
                let _ = color_picker(ui, c);
            });
        }
        assert_eq!(ctx.data_mut(|d| d.count::<String>()), 2);
    }
}
