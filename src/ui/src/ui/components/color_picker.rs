// SPDX-License-Identifier: GPL-3.0-or-later
//! The shared RGB colour picker: a large swatch (click → full HSV popup), a
//! preset swatch grid, a rainbow hue slider, and a manual `#rrggbb` input.
//! This is the single colour-picking widget reused across the app — do not
//! hand-roll another.

use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::RgbColor;
use std::hash::Hash;

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

/// A 36 px swatch-and-label button that selects which colour slot the picker
/// edits ("Color A" / "Color B"); `active` rings it in the accent.
pub fn color_slot_button(
    ui: &mut egui::Ui,
    label: &str,
    color: Color32,
    active: bool,
    width: f32,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 36.0), Sense::click());
    let border = if active {
        theme::CYAN
    } else if resp.hovered() {
        theme::BORDER_INNER
    } else {
        theme::TRACK
    };
    let p = ui.painter();
    theme::paint_surface(p, rect, 6.0, theme::INNER_BG, border);
    let sw = Rect::from_min_size(
        Pos2::new(rect.left() + 10.0, rect.center().y - 9.0),
        Vec2::splat(18.0),
    );
    p.rect_filled(sw, 4.0, color);
    p.with_clip_rect(rect.shrink(4.0)).text(
        Pos2::new(sw.right() + 10.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        theme::body_md(),
        theme::TEXT,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// Labelled colours as slot buttons sharing one picker for the active slot.
/// Returns the slot edited this frame and its new colour.
pub fn color_slots(
    ui: &mut egui::Ui,
    label: &str,
    colors: &[(&str, RgbColor)],
    slot: &mut usize,
) -> Option<(usize, RgbColor)> {
    if colors.is_empty() {
        return None;
    }
    *slot = (*slot).min(colors.len() - 1);
    super::field_label(ui, label);
    let gap = theme::SPACE_4;
    let w = (ui.available_width() - gap * (colors.len() - 1) as f32) / colors.len() as f32;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (i, (name, c)) in colors.iter().enumerate() {
            if color_slot_button(ui, name, rgb_to_color32(*c), i == *slot, w).clicked() {
                *slot = i;
            }
        }
    });
    ui.add_space(theme::SPACE_4);
    color_picker(ui, colors[*slot].1).map(|c| (*slot, c))
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

pub fn color_swatch_row(
    ui: &mut egui::Ui,
    id_salt: impl Hash + std::fmt::Debug,
    current: RgbColor,
) -> Option<RgbColor> {
    ui.push_id(id_salt, |ui| color_swatch_row_inner(ui, current))
        .inner
}

fn color_swatch_row_inner(ui: &mut egui::Ui, current: RgbColor) -> Option<RgbColor> {
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
            theme::value_sm(),
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

/// A swatch row over a fixed palette, selected by index rather than by value —
/// for settings persisted as a palette slot (e.g. Home widget accents).
pub fn palette_swatch_row(
    ui: &mut egui::Ui,
    id_salt: impl Hash + std::fmt::Debug,
    palette: &[Color32],
    current: u8,
) -> Option<u8> {
    ui.push_id(id_salt, |ui| {
        let mut picked = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(9.0, 9.0);
            for (i, &color) in palette.iter().enumerate() {
                if mini_swatch(ui, 22.0, color, usize::from(current) == i) {
                    picked = Some(i as u8);
                }
            }
        });
        picked
    })
    .inner
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
        theme::ON_ACCENT
    } else {
        Color32::WHITE
    }
}

pub fn color_picker(ui: &mut egui::Ui, current: RgbColor) -> Option<RgbColor> {
    ui.scope(|ui| picker_body(ui, current)).inner
}

fn picker_body(ui: &mut egui::Ui, current: RgbColor) -> Option<RgbColor> {
    let id = ui.unique_id().with("color_picker");
    let hue_id = id.with("hue");
    let (h, sat, val) = rgb_to_hsv(current);
    let hue = if sat > 0.01 && val > 0.01 {
        h
    } else {
        ui.data(|d| d.get_temp::<f32>(hue_id)).unwrap_or(h)
    };
    let mut out = None;
    let width = ui.available_width();

    let (sv_rect, sv_resp) =
        ui.allocate_exact_size(Vec2::new(width, 160.0), Sense::click_and_drag());
    paint_sv_square(ui.painter(), sv_rect, hue);
    if let Some(pos) = sv_resp
        .interact_pointer_pos()
        .filter(|_| sv_resp.dragged() || sv_resp.clicked())
    {
        let (s, v) = sv_at(sv_rect, pos);
        out = Some(hsv_to_rgb(hue, s, v));
    }
    let thumb = Pos2::new(
        sv_rect.left() + sat * sv_rect.width(),
        sv_rect.top() + (1.0 - val) * sv_rect.height(),
    );
    paint_thumb(ui.painter(), thumb, rgb_to_color32(current));

    ui.add_space(theme::SPACE_6);
    let (hue_rect, hue_resp) =
        ui.allocate_exact_size(Vec2::new(width, 14.0), Sense::click_and_drag());
    draw_h_gradient_rect(
        ui.painter(),
        hue_rect.shrink2(Vec2::new(0.0, 1.0)),
        HUE_STOPS,
        6.0,
    );
    let mut shown_hue = hue;
    if let Some(pos) = hue_resp
        .interact_pointer_pos()
        .filter(|_| hue_resp.dragged() || hue_resp.clicked())
    {
        shown_hue = ((pos.x - hue_rect.left()) / hue_rect.width()).clamp(0.0, 1.0) * 360.0;
        out = Some(hsv_to_rgb(shown_hue, sat.max(0.7), val.max(0.7)));
    }
    paint_thumb(
        ui.painter(),
        Pos2::new(
            hue_rect.left() + shown_hue / 360.0 * hue_rect.width(),
            hue_rect.center().y,
        ),
        rgb_to_color32(hsv_to_rgb(shown_hue, 1.0, 1.0)),
    );
    for r in [&sv_resp, &hue_resp] {
        if r.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
    }
    ui.data_mut(|d| d.insert_temp(hue_id, shown_hue));

    ui.add_space(theme::SPACE_6);
    ui.horizontal(|ui| {
        let (sw, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
        theme::paint_surface(
            ui.painter(),
            sw,
            theme::RADIUS_SM,
            rgb_to_color32(out.unwrap_or(current)),
            theme::BORDER_INNER,
        );
        let hex_id = id.with("hexbuf");
        let mut buf = ui
            .data(|d| d.get_temp::<String>(hex_id))
            .unwrap_or_default();
        if !ui.memory(|m| m.has_focus(hex_id)) {
            buf = hex_label(out.unwrap_or(current));
        }
        let resp = ui.add_sized(
            Vec2::new(ui.available_width(), 36.0),
            egui::TextEdit::singleline(&mut buf)
                .id(hex_id)
                .font(theme::mono(13.0))
                .margin(egui::vec2(10.0, 9.0))
                .hint_text("#rrggbb"),
        );
        if resp.changed() {
            if let Some(c) = parse_hex(&buf) {
                out = Some(c);
            }
        }
        ui.data_mut(|d| d.insert_temp(hex_id, buf));
    });

    ui.add_space(theme::SPACE_6);
    ui.label(
        egui::RichText::new(t!("misc.color_recent"))
            .font(theme::caption())
            .color(theme::TEXT_MUT),
    );
    ui.add_space(theme::SPACE_3);
    const COLS: usize = 8;
    const GAP: f32 = 6.0;
    let cell = (width - GAP * (COLS - 1) as f32) / COLS as f32;
    let rows = SWATCHES.len().div_ceil(COLS);
    let (grid, _) = ui.allocate_exact_size(
        Vec2::new(width, rows as f32 * (cell + GAP) - GAP),
        Sense::hover(),
    );
    for (i, &hexv) in SWATCHES.iter().enumerate() {
        let r = Rect::from_min_size(
            grid.min
                + Vec2::new(
                    (i % COLS) as f32 * (cell + GAP),
                    (i / COLS) as f32 * (cell + GAP),
                ),
            Vec2::splat(cell),
        );
        let resp = ui.interact(r, id.with(("swatch", i)), Sense::click());
        let sc = theme::hex(hexv);
        ui.painter().rect_filled(r, theme::RADIUS_XS + 2.0, sc);
        if resp.hovered() || sc == rgb_to_color32(current) {
            ui.painter().rect_stroke(
                r,
                theme::RADIUS_XS + 2.0,
                Stroke::new(2.0, theme::TEXT),
                egui::StrokeKind::Inside,
            );
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            out = Some(color32_to_rgb(sc));
        }
    }
    out
}

const HUE_STOPS: &[(f32, u32)] = &[
    (0.0, 0xff0000),
    (1.0 / 6.0, 0xffff00),
    (2.0 / 6.0, 0x00ff00),
    (3.0 / 6.0, 0x00ffff),
    (4.0 / 6.0, 0x0000ff),
    (5.0 / 6.0, 0xff00ff),
    (1.0, 0xff0000),
];

/// Saturation (left→right) and value (top→bottom) under `pos`, clamped.
fn sv_at(rect: Rect, pos: Pos2) -> (f32, f32) {
    (
        ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0),
        1.0 - ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0),
    )
}

fn paint_sv_square(p: &egui::Painter, rect: Rect, hue: f32) {
    let pure = rgb_to_color32(hsv_to_rgb(hue, 1.0, 1.0));
    let quad = |tl: Color32, tr: Color32, bl: Color32, br: Color32| {
        let mut mesh = egui::Mesh::default();
        mesh.colored_vertex(rect.left_top(), tl);
        mesh.colored_vertex(rect.right_top(), tr);
        mesh.colored_vertex(rect.left_bottom(), bl);
        mesh.colored_vertex(rect.right_bottom(), br);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(1, 3, 2);
        egui::Shape::mesh(mesh)
    };
    p.add(quad(Color32::WHITE, pure, Color32::WHITE, pure));
    p.add(quad(
        Color32::TRANSPARENT,
        Color32::TRANSPARENT,
        Color32::BLACK,
        Color32::BLACK,
    ));
    theme::round_corners(p, rect, theme::RADIUS_SM, theme::CARD_BG);
}

fn paint_thumb(p: &egui::Painter, center: Pos2, fill: Color32) {
    p.circle_filled(center, 7.0, fill);
    p.circle_stroke(center, 7.0, Stroke::new(2.0, Color32::WHITE));
    p.circle_stroke(center, 8.0, Stroke::new(1.0, theme::a(Color32::BLACK, 0.5)));
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
    fn sv_square_maps_corners_and_clamps() {
        let r = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(100.0, 50.0));
        assert_eq!(sv_at(r, r.left_top()), (0.0, 1.0));
        assert_eq!(sv_at(r, r.right_bottom()), (1.0, 0.0));
        assert_eq!(sv_at(r, Pos2::new(-50.0, 500.0)), (0.0, 0.0));
    }

    #[test]
    fn mini_glyph_color_contrasts_with_fill() {
        assert_eq!(mini_glyph_color(Color32::WHITE), theme::ON_ACCENT);
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
            let _ = crate::ui::run_headless(&ctx, input(), |ui| {
                let c = RgbColor { r: 1, g: 2, b: 3 };
                let _ = color_picker(ui, c);
                let _ = color_picker(ui, c);
            });
        }
        assert_eq!(ctx.data_mut(|d| d.count::<String>()), 2);
    }
}
