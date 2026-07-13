// SPDX-License-Identifier: GPL-3.0-or-later
//! Sidebar nav and device-type icons, rasterized from the bundled SVG set in
//! `assets/icons/` and cached as egui textures. Device glyphs are rasterized
//! as alpha masks so `draw_device` can tint them any color.

use egui::{Color32, Pos2, Rect, Stroke, Ui, Vec2};
use halod_shared::types::DeviceType;
use std::cell::RefCell;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Home,
    Lighting,
    Cooling,
    Settings,
    Plugins,
    Integrations,
}

impl Icon {
    fn svg(self) -> &'static [u8] {
        match self {
            Icon::Home => include_bytes!("../../assets/icons/home.svg"),
            Icon::Lighting => include_bytes!("../../assets/icons/rgb.svg"),
            Icon::Cooling => include_bytes!("../../assets/icons/cooling.svg"),
            Icon::Settings => include_bytes!("../../assets/icons/settings.svg"),
            Icon::Plugins => include_bytes!("../../assets/icons/plugins.svg"),
            Icon::Integrations => include_bytes!("../../assets/icons/integrations.svg"),
        }
    }

    fn key(self) -> &'static str {
        match self {
            Icon::Home => "nav_home",
            Icon::Lighting => "nav_lighting",
            Icon::Cooling => "nav_cooling",
            Icon::Settings => "nav_settings",
            Icon::Plugins => "nav_plugins",
            Icon::Integrations => "nav_integrations",
        }
    }
}

fn device_svg(ty: DeviceType) -> &'static [u8] {
    match ty {
        DeviceType::Keyboard => include_bytes!("../../assets/icons/devices/keyboard.svg"),
        DeviceType::Mouse => include_bytes!("../../assets/icons/devices/mouse.svg"),
        DeviceType::Headset => include_bytes!("../../assets/icons/devices/headset.svg"),
        DeviceType::Monitor => include_bytes!("../../assets/icons/devices/monitor.svg"),
        DeviceType::Gpu => include_bytes!("../../assets/icons/devices/gpu.svg"),
        DeviceType::LedStrip => include_bytes!("../../assets/icons/devices/led_strip.svg"),
        DeviceType::Ram => include_bytes!("../../assets/icons/devices/ram.svg"),
        DeviceType::Fan => include_bytes!("../../assets/icons/devices/fan.svg"),
        DeviceType::AIO => include_bytes!("../../assets/icons/devices/aio.svg"),
        DeviceType::Hub => include_bytes!("../../assets/icons/devices/hub.svg"),
        DeviceType::Dongle => include_bytes!("../../assets/icons/devices/dongle.svg"),
        DeviceType::Speaker => include_bytes!("../../assets/icons/devices/speaker.svg"),
        DeviceType::Sensor => include_bytes!("../../assets/icons/devices/sensor.svg"),
        DeviceType::Motherboard => include_bytes!("../../assets/icons/devices/motherboard.svg"),
        DeviceType::Other => include_bytes!("../../assets/icons/devices/other.svg"),
    }
}

#[expect(dead_code, reason = "complete icon coverage table validated by tests")]
const DEVICE_TYPES: [DeviceType; 15] = [
    DeviceType::Keyboard,
    DeviceType::Mouse,
    DeviceType::Headset,
    DeviceType::Monitor,
    DeviceType::Gpu,
    DeviceType::LedStrip,
    DeviceType::Ram,
    DeviceType::Fan,
    DeviceType::AIO,
    DeviceType::Hub,
    DeviceType::Dongle,
    DeviceType::Speaker,
    DeviceType::Sensor,
    DeviceType::Motherboard,
    DeviceType::Other,
];

thread_local! {
    static TEXTURES: RefCell<HashMap<Icon, egui::TextureHandle>> = RefCell::new(HashMap::new());
    static DEVICE_TEXTURES: RefCell<HashMap<DeviceType, egui::TextureHandle>> =
        RefCell::new(HashMap::new());
}

/// Resolution the nav SVGs rasterize to — oversampled past the ~16px draw size
/// so the linear downscale stays crisp on HiDPI.
const ICON_PX: u32 = 64;

fn texture(ctx: &egui::Context, icon: Icon) -> egui::TextureHandle {
    TEXTURES.with(|cache| {
        cache
            .borrow_mut()
            .entry(icon)
            .or_insert_with(|| {
                let img = rasterize(icon.svg(), ICON_PX)
                    .unwrap_or_else(|| egui::ColorImage::from_rgba_unmultiplied([1, 1], &[0; 4]));
                ctx.load_texture(icon.key(), img, egui::TextureOptions::LINEAR)
            })
            .clone()
    })
}

fn render_svg(bytes: &[u8], target: u32) -> Option<resvg::tiny_skia::Pixmap> {
    use resvg::{tiny_skia, usvg};

    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default()).ok()?;
    let size = tree.size().to_int_size();
    let long_edge = size.width().max(size.height()) as f32;
    if long_edge <= 0.0 {
        return None;
    }
    let scale = target as f32 / long_edge;
    let w = (size.width() as f32 * scale).ceil() as u32;
    let h = (size.height() as f32 * scale).ceil() as u32;
    let mut pixmap = tiny_skia::Pixmap::new(w.max(1), h.max(1))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some(pixmap)
}

/// Rasterize SVG bytes to an RGBA image `target` px on the long edge.
fn rasterize(bytes: &[u8], target: u32) -> Option<egui::ColorImage> {
    let pixmap = render_svg(bytes, target)?;
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [pixmap.width() as usize, pixmap.height() as usize],
        pixmap.data(),
    ))
}

/// Rasterize SVG bytes to a white alpha mask, so egui's multiplicative tint
/// recolors the glyph regardless of the fill colors in the source SVG. The
/// device SVGs' viewBoxes are cropped tight to the artwork (enforced by test),
/// so the mask centers correctly without further cropping.
fn rasterize_mask(bytes: &[u8], target: u32) -> Option<egui::ColorImage> {
    let pixmap = render_svg(bytes, target)?;
    let rgba: Vec<u8> = pixmap
        .data()
        .chunks_exact(4)
        .flat_map(|px| [255, 255, 255, px[3]])
        .collect();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [pixmap.width() as usize, pixmap.height() as usize],
        &rgba,
    ))
}

fn device_texture(ctx: &egui::Context, ty: DeviceType) -> egui::TextureHandle {
    DEVICE_TEXTURES.with(|cache| {
        cache
            .borrow_mut()
            .entry(ty)
            .or_insert_with(|| {
                let img = rasterize_mask(device_svg(ty), ICON_PX)
                    .unwrap_or_else(|| egui::ColorImage::from_rgba_unmultiplied([1, 1], &[0; 4]));
                ctx.load_texture(format!("device_{ty:?}"), img, egui::TextureOptions::LINEAR)
            })
            .clone()
    })
}

/// Blit a nav icon centered in `rect`. `tint` multiplies the full-color icon —
/// pass an opacity-only tint to dim inactive rows; the icons carry their own hue.
pub fn draw(ui: &Ui, rect: Rect, icon: Icon, tint: Color32) {
    let tex = texture(ui.ctx(), icon);
    let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    ui.painter().image(tex.id(), rect, uv, tint);
}

/// Draw the bundled SVG glyph for a device type, tinted `color` and fitted
/// centered in `rect` preserving aspect ratio. Used by the shared device badge
/// so every card/row/blip shows an icon instead of a 2–3 letter code.
pub fn draw_device(p: &egui::Painter, rect: Rect, ty: DeviceType, color: Color32) {
    let tex = device_texture(p.ctx(), ty);
    let size = tex.size_vec2();
    let scale = (rect.width() / size.x).min(rect.height() / size.y);
    let fitted = Rect::from_center_size(rect.center(), size * scale);
    let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    p.image(tex.id(), fitted, uv, color);
}

/// Pencil/edit glyph drawn flat-line inside `rect` (the bundled Inter subset
/// has no `✎`, which would otherwise render as a tofu square).
pub fn draw_pencil(p: &egui::Painter, rect: Rect, color: Color32) {
    let s = Stroke::new(1.4, color);
    let c = rect.center();
    let r = rect.width().min(rect.height()) * 0.5;
    let d = std::f32::consts::FRAC_1_SQRT_2;
    let dir = Vec2::new(d, -d); // nib (lower-left) → eraser (upper-right)
    let perp = Vec2::new(d, d);
    let l = r * 0.62;
    let w = r * 0.2;
    let eraser = c + dir * l;
    let nib_base = c - dir * l * 0.55;
    let nib_point = c - dir * l;
    let line = |a: Pos2, b: Pos2| p.line_segment([a, b], s);
    line(eraser + perp * w, nib_base + perp * w);
    line(eraser - perp * w, nib_base - perp * w);
    line(eraser + perp * w, eraser - perp * w);
    line(nib_base + perp * w, nib_point);
    line(nib_base - perp * w, nib_point);
}

/// Fork glyph drawn flat-line inside `rect` (the bundled Inter subset has no
/// `⑂`, which would otherwise render as a tofu square). A stem rising from the
/// base that forks into two prongs, with a node dot at each of the three tips.
pub fn draw_fork(p: &egui::Painter, rect: Rect, color: Color32) {
    let s = Stroke::new(rect.width().min(rect.height()) * 0.06, color);
    let c = rect.center();
    let r = rect.width().min(rect.height()) * 0.5;
    let base = c + Vec2::new(0.0, r * 0.62);
    let split = c + Vec2::new(0.0, -r * 0.04);
    let left = c + Vec2::new(-r * 0.5, -r * 0.62);
    let right = c + Vec2::new(r * 0.5, -r * 0.62);
    p.line_segment([base, split], s);
    p.line_segment([split, left], s);
    p.line_segment([split, right], s);
    for tip in [base, left, right] {
        p.circle_filled(tip, r * 0.16, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every bundled nav SVG must rasterize to a non-empty, partially opaque
    /// image at the target resolution — catches a missing/broken asset or a
    /// wrong include path at build time rather than as a blank sidebar glyph.
    #[test]
    fn every_nav_icon_rasterizes() {
        for icon in [
            Icon::Home,
            Icon::Lighting,
            Icon::Cooling,
            Icon::Settings,
            Icon::Integrations,
        ] {
            let img = rasterize(icon.svg(), ICON_PX)
                .unwrap_or_else(|| panic!("{} failed to rasterize", icon.key()));
            assert_eq!(img.width().max(img.height()), ICON_PX as usize);
            assert!(
                img.pixels.iter().any(|p| p.a() > 0),
                "{} rasterized fully transparent",
                icon.key()
            );
        }
    }

    /// Every device-type SVG must rasterize to a pure-white mask (so any tint
    /// survives egui's multiplicative tinting, even for non-white source
    /// fills) whose artwork is centered and fills the long axis — i.e. the
    /// asset's viewBox is a tight square around the drawing, so `draw_device`
    /// centers the visible glyph, not stray margins.
    #[test]
    fn every_device_icon_rasterizes_as_centered_white_mask() {
        for ty in DEVICE_TYPES {
            let img = rasterize_mask(device_svg(ty), ICON_PX)
                .unwrap_or_else(|| panic!("{ty:?} failed to rasterize"));
            let (w, h) = (img.width(), img.height());
            for px in img.pixels.iter().filter(|p| p.a() > 0) {
                let unmul = px.to_srgba_unmultiplied();
                assert_eq!(
                    (unmul[0], unmul[1], unmul[2]),
                    (255, 255, 255),
                    "{ty:?} mask has a non-white pixel"
                );
            }
            let (mut x0, mut x1, mut y0, mut y1) = (w, 0, h, 0);
            for y in 0..h {
                for x in 0..w {
                    if img.pixels[y * w + x].a() > 0 {
                        x0 = x0.min(x);
                        x1 = x1.max(x);
                        y0 = y0.min(y);
                        y1 = y1.max(y);
                    }
                }
            }
            assert!(x0 <= x1, "{ty:?} rasterized fully transparent");
            let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
            assert!(
                bw.max(bh) + 2 >= w.max(h),
                "{ty:?} viewBox not tight around the artwork"
            );
            let center_off = |lo: usize, hi: usize, dim: usize| {
                ((lo + hi + 1) as f32 / 2.0 - dim as f32 / 2.0).abs()
            };
            assert!(
                center_off(x0, x1, w) <= 1.5 && center_off(y0, y1, h) <= 1.5,
                "{ty:?} artwork not centered in its viewBox"
            );
        }
    }
}
