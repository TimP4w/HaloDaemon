// SPDX-License-Identifier: GPL-3.0-or-later
//! Palette, fonts and small paint helpers for the "Prism Control" design
//! (imported from claude.ai/design). Dark editorial surface, cyan accent,
//! Inter Tight for text and JetBrains Mono for numerics.

use egui::{
    Color32, Context, FontData, FontDefinitions, FontFamily, FontId, Mesh, Pos2, Rect, Shape,
    Stroke,
};

mod tokens;
mod type_scale;

pub use tokens::*;
pub use type_scale::*;

// ── Surfaces ─────────────────────────────────────────────────────────────────
pub const BODY: Color32 = hex(0x090b11);
#[expect(dead_code, reason = "theme token reserved for native window chrome")]
pub const WIN_TOP: Color32 = hex(0x0e0d15);
pub const TITLE_BG: Color32 = hex(0x0b0e15);
pub const SIDEBAR_BG: Color32 = hex(0x0c0f17);
/// Raised cards throughout the application.
pub const CARD_BG: Color32 = hex(0x181521);
/// Dialog, modal, menu, and popup shells.
pub const MODAL_BG: Color32 = hex(0x110f18);
pub const INNER_BG: Color32 = hex(0x0d1119);
pub const ROW_ACTIVE: Color32 = hex(0x211c2c);

// ── Borders ──────────────────────────────────────────────────────────────────
pub const BORDER: Color32 = hex(0x262231);
pub const BORDER_SOFT: Color32 = hex(0x1e1a27);
pub const BORDER_INNER: Color32 = hex(0x2b2637);
/// Subtle chrome divider (title bar / sidebar edges) — barely above the bg.
pub const DIVIDER: Color32 = hex(0x0c0b12);

// ── Text ─────────────────────────────────────────────────────────────────────
pub const TEXT: Color32 = hex(0xe7ebf3);
pub const TEXT_BRIGHT: Color32 = hex(0xdfe6f2);
pub const TEXT_DIM: Color32 = hex(0x9aa3b8);
pub const TEXT_MUT: Color32 = hex(0x7a8398);
pub const TEXT_FAINT: Color32 = hex(0x5d6679);
pub const TEXT_FAINT2: Color32 = hex(0x4e576b);

// ── Accent + status ──────────────────────────────────────────────────────────
pub const CYAN: Color32 = hex(0x8b6fd8);
pub const TRAFFIC_RED: Color32 = hex(0xe05a6e);
pub const TRAFFIC_YELLOW: Color32 = hex(0xe0b34f);
pub const TRAFFIC_GREEN: Color32 = hex(0x55c98a);
pub const ONLINE: Color32 = hex(0x55c98a);
pub const ONLINE_TEXT: Color32 = hex(0x83c9a4);
pub const OFFLINE: Color32 = hex(0xe05a6e);
pub const OFFLINE_TEXT: Color32 = hex(0xc98b95);

// ── Stat + battery accents ───────────────────────────────────────────────────
pub const STAT_CYAN: Color32 = hex(0x5fb8d6);
pub const STAT_PURPLE: Color32 = hex(0x9b7fe0);
pub const STAT_GREEN: Color32 = hex(0x47c98f);
pub const STAT_AMBER: Color32 = hex(0xd9a94f);

/// Logo mark gradient stops (an RGB spectrum).
pub const LOGO_STOPS: [Color32; 6] = [
    hex(0xd96aa8),
    hex(0xd9a94f),
    hex(0x47c98f),
    hex(0x5fb8d6),
    hex(0x9b7fe0),
    hex(0xd96aa8),
];

/// Per-sensor accent palette, shared by the Home dashboard and the Cooling
/// page so the same sensor index always reads the same color.
pub const SENSOR_HUES: [Color32; 4] = [STAT_CYAN, STAT_GREEN, hex(0x9b7fe0), STAT_AMBER];

/// Accent color for sensor `i` (wraps the [`SENSOR_HUES`] palette).
pub fn sensor_hue(i: usize) -> Color32 {
    SENSOR_HUES[i % SENSOR_HUES.len()]
}

/// Vibrant per-device accent palette (assigned by device type / id hash).
pub const DEVICE_HUES: [Color32; 10] = [
    hex(0x5fb8d6),
    hex(0x9b7fe0),
    hex(0x4fc2c2),
    hex(0x6b94e0),
    hex(0x47c98f),
    hex(0xd96aa8),
    hex(0x8b7fe0),
    hex(0xd97088),
    hex(0xd9a94f),
    hex(0x4fc2c2),
];

/// A device's accent color (its type's slot in [`DEVICE_HUES`]).
pub fn device_color(d: &halod_shared::types::WireDevice) -> Color32 {
    DEVICE_HUES[crate::domain::models::device::hue_index(d)]
}

/// Battery accent color for a classified [`crate::domain::models::device::BatteryLevel`].
pub fn battery_color(level: u8, charging: bool) -> Color32 {
    use crate::domain::models::device::BatteryLevel;
    match crate::domain::models::device::battery_level(level, charging) {
        BatteryLevel::Ok => TRAFFIC_GREEN,
        BatteryLevel::Low => STAT_AMBER,
        BatteryLevel::Critical => OFFLINE,
    }
}

pub const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// A color at fractional opacity, like CSS `rgba(c, a)`.
pub fn a(c: Color32, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (alpha * 255.0).round() as u8)
}

// ── Fonts ────────────────────────────────────────────────────────────────────

macro_rules! font {
    ($defs:expr, $key:literal, $path:literal) => {
        $defs.font_data.insert(
            $key.to_owned(),
            std::sync::Arc::new(FontData::from_static(include_bytes!($path))),
        );
    };
}

fn font_definitions() -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    font!(defs, "it400", "../../../../assets/fonts/InterTight-400.ttf");
    font!(defs, "it600", "../../../../assets/fonts/InterTight-600.ttf");
    font!(defs, "it700", "../../../../assets/fonts/InterTight-700.ttf");
    font!(
        defs,
        "jm400",
        "../../../../assets/fonts/JetBrainsMono-400.ttf"
    );
    font!(
        defs,
        "jm600",
        "../../../../assets/fonts/JetBrainsMono-600.ttf"
    );
    font!(
        defs,
        "jm700",
        "../../../../assets/fonts/JetBrainsMono-700.ttf"
    );
    // The exact NotoSans face the daemon's LCD renderer embeds, so the LCD
    // editor preview's Sans text matches the device pixel-for-pixel.
    font!(
        defs,
        "noto",
        "../../../../assets/fonts/NotoSans-Regular.ttf"
    );

    // Our custom faces lack symbol glyphs (×, ▾, →, …), so append egui's
    // Monospace fallback chain (Hack covers the missing shapes/arrows).
    let fallback = defs
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let with_fallback = |key: &str| {
        let mut v = vec![key.to_owned()];
        v.extend(fallback.iter().cloned());
        v
    };
    let fam = |defs: &mut FontDefinitions, name: &str, key: &str| {
        defs.families
            .insert(FontFamily::Name(name.into()), with_fallback(key));
    };
    defs.families
        .insert(FontFamily::Proportional, with_fallback("it400"));
    defs.families
        .insert(FontFamily::Monospace, with_fallback("jm400"));
    fam(&mut defs, "semibold", "it600");
    fam(&mut defs, "bold", "it700");
    fam(&mut defs, "mono", "jm600");
    fam(&mut defs, "mono-bold", "jm700");
    // LCD editor preview faces — the exact weights the daemon's renderer embeds
    // (NotoSans-Regular / JetBrainsMono-Regular / InterTight-400), so the inline
    // text editor matches the device-rendered sprite.
    fam(&mut defs, "lcd_sans", "noto");
    fam(&mut defs, "lcd_mono", "jm400");
    fam(&mut defs, "lcd_inter", "it400");

    defs
}

pub fn install_fonts(ctx: &Context) {
    ctx.set_fonts(font_definitions());
}

pub fn install_fonts_with_system<'a>(
    ctx: &Context,
    families: impl IntoIterator<Item = &'a str>,
) -> std::collections::HashSet<String> {
    let mut defs = font_definitions();
    let mut loaded = std::collections::HashSet::new();
    let fallback = defs
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    for family_name in families {
        let Some((bytes, index)) = halod_shared::system_fonts::data(family_name) else {
            continue;
        };
        let key = format!("lcd_system_font:{family_name}");
        let mut data = FontData::from_owned(bytes);
        data.index = index;
        defs.font_data
            .insert(key.clone(), std::sync::Arc::new(data));
        let mut family = vec![key];
        family.extend(fallback.iter().cloned());
        defs.families
            .insert(FontFamily::Name(family_name.into()), family);
        loaded.insert(family_name.to_owned());
    }
    ctx.set_fonts(defs);
    loaded
}

/// Main content background (`window` gradient bottom stop).
pub const MAIN_BG: Color32 = hex(0x090b12);

/// Install fonts and the base dark visuals.
pub fn install(ctx: &Context) {
    install_fonts(ctx);
    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = MAIN_BG;
    v.extreme_bg_color = INNER_BG;
    v.selection.bg_fill = a(CYAN, 0.25);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, DIVIDER);

    // Context menus and native egui windows share the same raised shell as the
    // application's explicit modal component.
    v.window_fill = MODAL_BG;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = egui::CornerRadius::same(11);
    v.menu_corner_radius = egui::CornerRadius::same(11);
    v.widgets.inactive.weak_bg_fill = INNER_BG;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, hex(0x2d2b3d));
    v.widgets.inactive.corner_radius = egui::CornerRadius::same(7);
    v.widgets.hovered.weak_bg_fill = hex(0x191826);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, hex(0x2d2b3d));
    v.widgets.hovered.corner_radius = egui::CornerRadius::same(7);
    v.widgets.active.weak_bg_fill = hex(0x191826);
    v.widgets.active.bg_stroke = Stroke::new(1.0, CYAN);
    v.widgets.active.corner_radius = egui::CornerRadius::same(7);

    ctx.set_visuals(v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.scroll = egui::style::ScrollStyle::solid();
        s.spacing.menu_margin = egui::Margin::same(6);
        s.spacing.button_padding = egui::vec2(9.0, 7.0);
    });
}

pub fn body(size: f32) -> FontId {
    FontId::proportional(size)
}
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}
pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}
pub fn mono_semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("mono".into()))
}
pub fn mono_bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("mono-bold".into()))
}

// ── Paint helpers ────────────────────────────────────────────────────────────

/// Fill a rect with a horizontal multi-stop gradient (used for the prism mark).
pub fn h_gradient(painter: &egui::Painter, rect: Rect, stops: &[Color32]) {
    if stops.len() < 2 {
        return;
    }
    let mut mesh = Mesh::default();
    for (i, &c) in stops.iter().enumerate() {
        let t = i as f32 / (stops.len() - 1) as f32;
        let x = rect.left() + rect.width() * t;
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(Pos2::new(x, rect.top()), c);
        mesh.colored_vertex(Pos2::new(x, rect.bottom()), c);
        if i > 0 {
            mesh.add_triangle(base - 2, base - 1, base);
            mesh.add_triangle(base - 1, base, base + 1);
        }
    }
    painter.add(Shape::mesh(mesh));
}

/// Draw the small rounded RGB-spectrum logo mark.
#[expect(dead_code, reason = "reusable compact logo renderer")]
pub fn logo_mark(painter: &egui::Painter, rect: Rect) {
    h_gradient(&painter.with_clip_rect(rect), rect, &LOGO_STOPS);
}

/// The "Drift" mark's conic-gradient stops (`conic-gradient(from 210deg,
/// #a78bfa, #e879f9, #7c5cff, #a78bfa)`), matching `assets/icon.svg`. First
/// and last are equal so the loop closes seamlessly.
pub const DRIFT_STOPS: [Color32; 4] = [hex(0xa78bfa), hex(0xe879f9), hex(0x7c5cff), hex(0xa78bfa)];

const DRIFT_START_DEG: f32 = 210.0;

/// Color of the Drift conic gradient at fractional turn `t` (wraps mod 1).
pub fn drift_conic(t: f32) -> Color32 {
    let t = t.rem_euclid(1.0);
    let segs = (DRIFT_STOPS.len() - 1) as f32;
    let seg = t * segs;
    let i = (seg.floor() as usize).min(DRIFT_STOPS.len() - 2);
    lerp_color(DRIFT_STOPS[i], DRIFT_STOPS[i + 1], seg - i as f32)
}

/// A conic-gradient annulus mesh: at each angle around `center`, a radial fan
/// through `bands` — `(radius = factor·size, alpha)` pairs — colored by
/// [`drift_conic`]. Feathered bands (alpha 0 at the ends) read as a soft glow;
/// a single opaque `inner_r`→`outer_r` pair reads as the crisp ring.
fn drift_ring(painter: &egui::Painter, center: Pos2, start: f32, bands: &[(f32, f32)]) {
    const N: usize = 256;
    let cols = bands.len() as u32;
    let mut mesh = Mesh::default();
    for i in 0..=N {
        let f = i as f32 / N as f32;
        let ang = start + f * std::f32::consts::TAU;
        let (s, c) = ang.sin_cos();
        let dir = egui::Vec2::new(c, s);
        let hue = drift_conic(f);
        let base = mesh.vertices.len() as u32;
        for &(r, alpha) in bands {
            mesh.colored_vertex(center + dir * r, a(hue, alpha));
        }
        if i > 0 {
            for b in 0..cols - 1 {
                let (p0, p1) = (base - cols + b, base - cols + b + 1);
                let (c0, c1) = (base + b, base + b + 1);
                mesh.add_triangle(p0, p1, c0);
                mesh.add_triangle(p1, c0, c1);
            }
        }
    }
    painter.add(Shape::mesh(mesh));
}

/// Draw the Halo "Drift" mark filling `rect`, rendered natively as meshes so it
/// stays crisp at any size/DPI. A static crisp conic donut sits in front; a
/// soft copy of the same conic gradient rotates slowly behind it (9s loop) so
/// only the outer glow's hues drift — no pulsing or scaling, per the spec.
/// `time` is the monotonic egui clock; keep requesting repaints to animate it.
pub fn logo_icon(painter: &egui::Painter, ctx: &Context, rect: Rect, time: f32) {
    let center = rect.center();
    let sz = rect.width().min(rect.height());
    let start = DRIFT_START_DEG.to_radians();
    // Raw meshes bypass egui's tessellator, so its edge feathering doesn't
    // apply — ramp the donut's inner/outer edges to alpha 0 across ~1 physical
    // pixel ourselves to anti-alias them.
    let fw = 0.6 / ctx.pixels_per_point();
    // Moving glow: the same conic gradient, slowly rotating. A gaussian radial
    // falloff (low peak, long tail to zero) emulates a blur so it fades out with
    // no visible rim — the brightest hues just drift around the outside.
    let rot = time / 9.0 * std::f32::consts::TAU;
    let (peak, rc, sigma) = (0.30, sz * 0.48, sz * 0.20);
    let glow: Vec<(f32, f32)> = (0..=16)
        .map(|i| {
            let r = sz * (0.16 + 0.90 * i as f32 / 16.0);
            let z = (r - rc) / sigma;
            (r, peak * (-0.5 * z * z).exp())
        })
        .collect();
    drift_ring(painter, center, start + rot, &glow);
    // Static crisp donut (icon.svg geometry: outer r=0.48, hole r=0.32).
    let (inner, outer) = (sz * 0.32, sz * 0.48);
    drift_ring(
        painter,
        center,
        start,
        &[
            (inner - fw, 0.0),
            (inner + fw, 1.0),
            (outer - fw, 1.0),
            (outer + fw, 0.0),
        ],
    );
}

/// A soft halo that hugs a rounded-rect's shape and feathers outward (a
/// colored drop shadow with no offset) — for glowing buttons. Unlike [`glow`],
/// this follows the rect outline instead of being a circular blob.
pub fn halo(painter: &egui::Painter, rect: Rect, rounding: f32, color: Color32, blur: f32) {
    let shadow = egui::epaint::Shadow {
        offset: [0, 0],
        blur: blur as u8,
        spread: 1,
        color,
    };
    painter.add(shadow.as_shape(rect, rounding));
}

/// Soft radial glow: a smooth gradient fan, opaque-ish at the center and fully
/// transparent at the rim (no visible banding).
pub fn glow(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32, strength: f32) {
    glow_ellipse(painter, center, radius, radius, color, strength);
}

/// Like [`glow`] but elliptical — a wide, short dome reads as a far subtler
/// wash than a full circle when anchored at a card's top edge.
pub fn glow_ellipse(
    painter: &egui::Painter,
    center: Pos2,
    rx: f32,
    ry: f32,
    color: Color32,
    strength: f32,
) {
    const N: usize = 48;
    let mut mesh = Mesh::default();
    mesh.colored_vertex(center, a(color, strength));
    let edge = a(color, 0.0);
    for i in 0..=N {
        let ang = std::f32::consts::TAU * i as f32 / N as f32;
        mesh.colored_vertex(
            center + egui::Vec2::new(ang.cos() * rx, ang.sin() * ry),
            edge,
        );
        if i > 0 {
            let n = mesh.vertices.len() as u32;
            mesh.add_triangle(0, n - 2, n - 1);
        }
    }
    painter.add(Shape::mesh(mesh));
}

/// Repaint the four corner cut-outs of a rounded rect with `bg`. egui has no
/// rounded clip, so glows/fills clipped to the sharp bounding rect bleed into
/// the corners; masking them back restores the rounded silhouette. Draw the
/// rounded border *after* this for a clean edge.
pub fn round_corners(painter: &egui::Painter, rect: Rect, r: f32, bg: Color32) {
    use std::f32::consts::PI;
    let (l, t, ri, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    // (outer corner, arc center, start angle, end angle)
    let corners = [
        (Pos2::new(l, t), Pos2::new(l + r, t + r), PI, 1.5 * PI),
        (
            Pos2::new(ri, t),
            Pos2::new(ri - r, t + r),
            1.5 * PI,
            2.0 * PI,
        ),
        (Pos2::new(ri, b), Pos2::new(ri - r, b - r), 0.0, 0.5 * PI),
        (Pos2::new(l, b), Pos2::new(l + r, b - r), 0.5 * PI, PI),
    ];
    const N: usize = 8;
    for (c, center, a0, a1) in corners {
        let mut mesh = Mesh::default();
        mesh.colored_vertex(c, bg);
        for i in 0..=N {
            let ang = a0 + (a1 - a0) * i as f32 / N as f32;
            mesh.colored_vertex(center + egui::Vec2::new(ang.cos() * r, ang.sin() * r), bg);
            if i > 0 {
                let n = mesh.vertices.len() as u32;
                mesh.add_triangle(0, n - 2, n - 1);
            }
        }
        painter.add(Shape::mesh(mesh));
    }
}

/// Animated "aurora": a few hue-shifted [`glow_ellipse`] blobs drifting around
/// `anchor`, fading in with `t` (0..1). `time` is the monotonic clock,
/// `rx`/`ry` the blob radii, and `spread` scales how far they wander. The caller
/// should clip the painter and keep requesting repaints while `t > 0`.
#[allow(clippy::too_many_arguments)]
pub fn aurora(
    painter: &egui::Painter,
    anchor: Pos2,
    rx: f32,
    ry: f32,
    spread: f32,
    color: Color32,
    t: f32,
    time: f32,
) {
    let base = egui::ecolor::Hsva::from(color);
    // (drift phase, hue offset, oscillation speed, peak strength)
    let layers = [
        (0.0_f32, 0.00_f32, 0.7_f32, 0.14_f32),
        (2.1, 0.07, 1.1, 0.11),
        (4.2, -0.06, 0.9, 0.09),
    ];
    for (k, &(ph, dh, sp, st)) in layers.iter().enumerate() {
        let osc = (time * sp + ph).sin();
        let osc2 = (time * sp * 0.6 + ph * 1.7).cos();
        let cx = anchor.x - spread * (0.06 + 0.16 * k as f32) + osc * spread * 0.14;
        let cy = anchor.y + osc2 * ry * 0.15;
        let col = egui::ecolor::Hsva::new(
            (base.h + dh + osc * 0.03).rem_euclid(1.0),
            base.s.clamp(0.4, 1.0),
            base.v.max(0.75),
            1.0,
        );
        let strength = st * t * (0.65 + 0.35 * osc2);
        glow_ellipse(
            painter,
            Pos2::new(cx, cy),
            rx,
            ry,
            Color32::from(col),
            strength,
        );
    }
}

/// Linear blend between two colors (component-wise, unmultiplied).
pub fn lerp_color(x: Color32, y: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        m(x.r(), y.r()),
        m(x.g(), y.g()),
        m(x.b(), y.b()),
        m(x.a(), y.a()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_conic_hits_stops_and_wraps() {
        // Segment boundaries land exactly on the gradient stops.
        assert_eq!(drift_conic(0.0), DRIFT_STOPS[0]);
        assert_eq!(drift_conic(1.0 / 3.0), DRIFT_STOPS[1]);
        assert_eq!(drift_conic(2.0 / 3.0), DRIFT_STOPS[2]);
        // The loop closes seamlessly: 1.0 wraps to the first stop.
        assert_eq!(drift_conic(1.0), DRIFT_STOPS[0]);
        assert_eq!(drift_conic(1.25), drift_conic(0.25));
        assert_eq!(drift_conic(-0.25), drift_conic(0.75));
    }

    #[test]
    fn device_hues_len_matches_model_hue_count() {
        // Pinned against domain::models::device::DEVICE_HUE_COUNT, which
        // every `hue_index()` result is bounds-checked against.
        assert_eq!(
            DEVICE_HUES.len(),
            crate::domain::models::device::DEVICE_HUE_COUNT
        );
    }
}
