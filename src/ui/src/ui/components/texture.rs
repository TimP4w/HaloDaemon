// SPDX-License-Identifier: GPL-3.0-or-later
//! Decoding image data into egui textures — the single upload path every
//! logo/preview/thumbnail cache shares.

use egui::Color32;

/// Build an egui texture from a raw RGBA8 buffer.
pub fn rgba_texture(
    ctx: &egui::Context,
    name: &str,
    rgba: &[u8],
    w: usize,
    h: usize,
) -> egui::TextureHandle {
    let pixels: Vec<Color32> = rgba
        .chunks_exact(4)
        .map(|c| Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]))
        .collect();
    ctx.load_texture(
        name,
        egui::ColorImage::new([w, h], pixels),
        egui::TextureOptions::LINEAR,
    )
}

/// Decode encoded image bytes (PNG/JPEG/first GIF frame) into a texture.
pub fn tex_from_bytes(
    ctx: &egui::Context,
    bytes: &[u8],
    name: &str,
) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    Some(rgba_texture(ctx, name, &img.into_raw(), w, h))
}
