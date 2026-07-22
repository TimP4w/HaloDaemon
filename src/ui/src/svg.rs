// SPDX-License-Identifier: GPL-3.0-or-later
//! SVG rasterization for the bundled icon set, the tray/window icon, and
//! desktop-entry icons picked up from the system icon theme.
//!
//! `resvg` is built without its `text` feature, so `<text>` elements do not
//! render. Every SVG this crate ships is path-only (pinned by a test in
//! `ui/src/ui/icons.rs`).

use resvg::{tiny_skia, usvg};

/// Rasterize `bytes`, scaled so the long edge is `target` px. `None` renders at
/// the SVG's own size.
pub fn rasterize(bytes: &[u8], target: Option<u32>) -> Option<tiny_skia::Pixmap> {
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default()).ok()?;
    let size = tree.size().to_int_size();
    let long_edge = size.width().max(size.height()) as f32;
    if long_edge <= 0.0 {
        return None;
    }
    let scale = target.map_or(1.0, |target| target as f32 / long_edge);
    let w = ((size.width() as f32 * scale).ceil() as u32).max(1);
    let h = ((size.height() as f32 * scale).ceil() as u32).max(1);
    let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some(pixmap)
}

/// Rasterize to an egui texture image, long edge `target` px.
pub fn color_image(bytes: &[u8], target: u32) -> Option<egui::ColorImage> {
    let pixmap = rasterize(bytes, Some(target))?;
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [pixmap.width() as usize, pixmap.height() as usize],
        pixmap.data(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQUARE: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="#f00"/></svg>"##;
    const WIDE: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 10"><rect width="20" height="10" fill="#f00"/></svg>"##;

    #[test]
    fn scales_the_long_edge_to_the_target_and_keeps_aspect() {
        let pixmap = rasterize(WIDE, Some(64)).expect("renders");
        assert_eq!((pixmap.width(), pixmap.height()), (64, 32));
    }

    #[test]
    fn renders_at_native_size_without_a_target() {
        let pixmap = rasterize(SQUARE, None).expect("renders");
        assert_eq!((pixmap.width(), pixmap.height()), (10, 10));
    }

    #[test]
    fn rejects_data_that_is_not_svg() {
        assert!(rasterize(b"not an svg", Some(16)).is_none());
    }
}
