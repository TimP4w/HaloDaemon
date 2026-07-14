// SPDX-License-Identifier: GPL-3.0-or-later
//! Image resize/compression for the LCD path — reducing an uploaded image to a
//! panel's native resolution. Depends on the `image`/`gif` crates, so it stays
//! daemon-side; the pure filename/format helpers live in
//! `halod_shared::types` (`validate_image_filename`, `sniff_ext`) so the GUI
//! can pre-validate before uploading.

use anyhow::Result;
use halod_shared::types::{sniff_ext, validate_image_upload_size, MAX_LCD_IMAGE_BYTES};
use std::path::Path;

/// Read an image file, rejecting anything past [`MAX_LCD_IMAGE_BYTES`] by
/// metadata before the bytes are allocated. Mirrors `config::read_bounded`.
pub fn read_image_bounded(path: &Path) -> Result<Vec<u8>> {
    let link_meta = std::fs::symlink_metadata(path)?;
    if link_meta.file_type().is_symlink() || !link_meta.is_file() {
        anyhow::bail!(
            "image {} must be a regular, non-symlink file",
            path.display()
        );
    }
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_LCD_IMAGE_BYTES {
        anyhow::bail!(
            "image {} is too large ({} bytes)",
            path.display(),
            meta.len()
        );
    }
    let data = std::fs::read(path)?;
    let after = std::fs::symlink_metadata(path)?;
    if after.file_type().is_symlink() || !after.is_file() || after.len() != data.len() as u64 {
        anyhow::bail!("image {} changed while it was being read", path.display());
    }
    validate_image_upload_size(data.len() as u64).map_err(|e| anyhow::anyhow!(e))?;
    Ok(data)
}

/// Async sibling of [`read_image_bounded`] for the tokio-runtime usecases.
pub async fn read_image_bounded_async(path: &Path) -> Result<Vec<u8>> {
    let link_meta = tokio::fs::symlink_metadata(path).await?;
    if link_meta.file_type().is_symlink() || !link_meta.is_file() {
        anyhow::bail!(
            "image {} must be a regular, non-symlink file",
            path.display()
        );
    }
    let meta = tokio::fs::metadata(path).await?;
    if meta.len() > MAX_LCD_IMAGE_BYTES {
        anyhow::bail!(
            "image {} is too large ({} bytes)",
            path.display(),
            meta.len()
        );
    }
    let data = tokio::fs::read(path).await?;
    let after = tokio::fs::symlink_metadata(path).await?;
    if after.file_type().is_symlink() || !after.is_file() || after.len() != data.len() as u64 {
        anyhow::bail!("image {} changed while it was being read", path.display());
    }
    validate_image_upload_size(data.len() as u64).map_err(|e| anyhow::anyhow!(e))?;
    Ok(data)
}

/// Cap on the source dimensions the image decoder may allocate for. A valid
/// header can declare enormous width/height that balloon allocation *before* any
/// resize, and `data` is attacker-controlled (uploaded over IPC or shipped by a
/// plugin), so every in-memory decode goes through [`decode_limited`]. Far above
/// any real LCD panel.
pub const MAX_DECODE_DIM: u32 = 8192;
/// Upper bound for decoded source pixels (64 MiB as RGBA8), independent of
/// per-side dimensions. A 8192×8192 image is otherwise still excessive for
/// the small LCD renderers.
pub const MAX_DECODE_PIXELS: u64 = 16 * 1024 * 1024;
pub const MAX_DECODE_RGBA_BYTES: u64 = MAX_DECODE_PIXELS * 4;

/// Decode an image from memory under [`MAX_DECODE_DIM`] source-dimension limits.
/// Bounds decompression-bomb allocation that `image::load_from_memory` (which
/// applies no limits) would otherwise incur before a resize can shrink it.
pub fn decode_limited(data: &[u8]) -> Result<image::DynamicImage> {
    let header_reader =
        image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?;
    let (width, height) = header_reader.into_dimensions()?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_DECODE_PIXELS {
        anyhow::bail!("image dimensions {width}×{height} exceed the decoded pixel limit");
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIM);
    limits.max_image_height = Some(MAX_DECODE_DIM);
    limits.max_alloc = Some(MAX_DECODE_RGBA_BYTES);
    reader.limits(limits);
    Ok(reader.decode()?)
}

/// Count the frames of a GIF without compositing them: the LZW data is decoded
/// into a discarded buffer, which is far cheaper than the RGBA resize pass and
/// lets `resize_gif` report a real percentage.
fn count_gif_frames(data: &[u8]) -> Result<usize> {
    let mut decoder = gif::DecodeOptions::new()
        .read_info(std::io::Cursor::new(data))
        .map_err(|e| anyhow::anyhow!("GIF decode: {e}"))?;
    let mut n = 0usize;
    while decoder
        .next_frame_info()
        .map_err(|e| anyhow::anyhow!("GIF frame: {e}"))?
        .is_some()
    {
        n += 1;
    }
    Ok(n)
}

/// Resize a GIF to exact LCD dimensions. Decodes as fully-composited RGBA
/// frames so sub-rect delta frames are composited correctly before resizing.
/// `progress` is called with 0–100 as frames are re-encoded.
pub fn resize_gif(
    data: &[u8],
    width: u32,
    height: u32,
    mut progress: impl FnMut(u8),
) -> Result<Vec<u8>> {
    use image::codecs::gif::{GifDecoder, GifEncoder};
    use image::AnimationDecoder;
    use std::io::Cursor;

    let (src_w, src_h) = {
        let opts = gif::DecodeOptions::new();
        let hdr = opts
            .read_info(Cursor::new(data))
            .map_err(|e| anyhow::anyhow!("GIF decode: {e}"))?;
        (hdr.width() as u32, hdr.height() as u32)
    };
    if src_w == 0
        || src_h == 0
        || src_w > MAX_DECODE_DIM
        || src_h > MAX_DECODE_DIM
        || u64::from(src_w) * u64::from(src_h) > MAX_DECODE_PIXELS
    {
        anyhow::bail!("GIF dimensions {src_w}×{src_h} exceed the decoded pixel limit");
    }

    const MAX_GIF_FRAMES: usize = 500;
    let total_frames = count_gif_frames(data)?;
    if total_frames > MAX_GIF_FRAMES {
        anyhow::bail!("GIF exceeds the {MAX_GIF_FRAMES} frame limit");
    }
    if src_w == width && src_h == height {
        return Ok(data.to_vec());
    }

    // Composite all frames to RGBA before resizing
    let decoder =
        GifDecoder::new(Cursor::new(data)).map_err(|e| anyhow::anyhow!("GIF decode: {e}"))?;

    let mut out = Vec::new();
    let mut encoder = GifEncoder::new_with_speed(&mut out, 30);
    encoder
        .set_repeat(image::codecs::gif::Repeat::Infinite)
        .map_err(|e| anyhow::anyhow!("GIF set repeat: {e}"))?;

    let mut frame_count = 0usize;
    for frame in decoder.into_frames() {
        let frame = frame.map_err(|e| anyhow::anyhow!("GIF frame: {e}"))?;
        let delay = frame.delay();
        let resized = image::imageops::resize(
            frame.buffer(),
            width,
            height,
            image::imageops::FilterType::Nearest,
        );
        encoder
            .encode_frame(image::Frame::from_parts(resized, 0, 0, delay))
            .map_err(|e| anyhow::anyhow!("GIF encode frame: {e}"))?;
        frame_count += 1;
        progress((frame_count * 100 / total_frames.max(1)) as u8);
    }
    drop(encoder);

    log::debug!(
        "[LCD] resize_gif: {} frames {}×{} → {}×{} ({} bytes)",
        frame_count,
        src_w,
        src_h,
        width,
        height,
        out.len()
    );
    Ok(out)
}

/// Compress an image for disk storage at LCD resolution.
/// - GIFs: each frame composited to RGBA and resized to LCD dims (see `resize_gif`).
/// - Static images already within LCD resolution: kept as-is.
/// - Static images larger than LCD: resized to LCD dims and saved as JPEG.
/// `progress` is called with 0–100 while a GIF is re-encoded; static images
/// finish in one step and report nothing.
pub fn compress_for_storage(
    data: &[u8],
    width: u32,
    height: u32,
    progress: impl FnMut(u8),
) -> Result<(Vec<u8>, &'static str)> {
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        let resized = resize_gif(data, width, height, progress)?;
        return Ok((resized, "gif"));
    }

    let img = decode_limited(data)?;
    if img.width() <= width && img.height() <= height {
        return Ok((data.to_vec(), sniff_ext(data)));
    }

    use image::codecs::jpeg::JpegEncoder;
    use std::io::Cursor;
    let resized = img.resize_to_fill(width, height, image::imageops::FilterType::Lanczos3);
    let rgb = resized.to_rgb8();
    let mut out: Vec<u8> = Vec::new();
    JpegEncoder::new_with_quality(Cursor::new(&mut out), 85).encode_image(&rgb)?;
    Ok((out, "jpg"))
}

/// Decode a static image (PNG/JPEG/…) and resize it to `width`×`height`,
/// returning a raw `width*height*4` RGBA8 buffer. CPU-heavy (Lanczos3) — call
/// off the async runtime.
pub fn decode_static_image_rgba(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let img = decode_limited(data)?;
    let img = if img.width() == width && img.height() == height {
        img
    } else {
        img.resize_exact(width, height, image::imageops::FilterType::Lanczos3)
    };
    Ok(img.into_rgba8().into_raw())
}

/// Rotate a square RGBA8 buffer by a multiple of 90°. Non-multiples of 90 and
/// non-square buffers are returned unchanged (LCD panels rotate their built-in
/// display in firmware but not streamed frames, so we pre-rotate in software).
pub fn rotate_rgba_square(rgba: &[u8], size: u32, degrees: u32) -> Vec<u8> {
    let n = size as usize;
    let step = degrees % 360;
    if step == 0 || rgba.len() != n * n * 4 {
        return rgba.to_vec();
    }
    let mut out = vec![0u8; rgba.len()];
    for y in 0..n {
        for x in 0..n {
            let (cx, cy) = match step {
                90 => (n - 1 - y, x),
                180 => (n - 1 - x, n - 1 - y),
                270 => (y, n - 1 - x),
                _ => (x, y),
            };
            let s = (y * n + x) * 4;
            let d = (cy * n + cx) * 4;
            out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
        }
    }
    out
}

/// RGBA8 → BGR888 (drops alpha, reorders to B,G,R) — the raw uncompressed LCD
/// stream format.
pub fn rgba_to_bgr888(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact(4) {
        out.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    out
}

/// Encode a raw RGBA8 frame as a complete Q565 file (QOI-style RGB565 codec):
/// `b"q565"` magic, LE u16 width/height, encoded stream, `OP_END`. This is the
/// compressed LCD stream format some panels (e.g. NZXT Kraken type-0x08) expect.
pub fn rgba_to_q565(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    use q565::{
        encode::Q565EncodeContext,
        utils::{encode_rgb565_unchecked, rgb888_to_rgb565},
    };
    let pixels = (width as usize) * (height as usize);
    let expected = pixels * 4;
    if rgba.len() != expected {
        anyhow::bail!(
            "RGBA frame size mismatch: got {} bytes, expected {expected} for {width}x{height}",
            rgba.len()
        );
    }
    if width > u16::MAX as u32 || height > u16::MAX as u32 {
        anyhow::bail!("frame {width}x{height} exceeds Q565 u16 dimension limit");
    }
    let rgb565: Vec<u16> = rgba
        .chunks_exact(4)
        .map(|p| encode_rgb565_unchecked(rgb888_to_rgb565([p[0], p[1], p[2]])))
        .collect();
    let mut out = Vec::with_capacity(8 + pixels);
    if !Q565EncodeContext::encode_to_vec(width as u16, height as u16, &rgb565, &mut out) {
        anyhow::bail!("Q565 encode failed for {width}x{height}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── resize_gif ────────────────────────────────────────────────────────

    // Builds gif::Frame (a foreign struct) field-by-field for clarity.
    #[allow(clippy::field_reassign_with_default)]
    fn make_gif(w: u16, h: u16) -> Vec<u8> {
        let mut out = Vec::new();
        let palette = &[0u8, 0, 0, 255, 255, 255];
        let mut enc = gif::Encoder::new(&mut out, w, h, palette).unwrap();
        enc.set_repeat(gif::Repeat::Infinite).unwrap();
        let mut frame = gif::Frame::default();
        frame.width = w;
        frame.height = h;
        frame.buffer = std::borrow::Cow::Owned(vec![0u8; w as usize * h as usize]);
        enc.write_frame(&frame).unwrap();
        drop(enc);
        out
    }

    /// Builds a two-frame GIF where the second frame is a sub-rect delta update.
    #[allow(clippy::field_reassign_with_default)]
    fn make_gif_with_partial_frame(cw: u16, ch: u16) -> Vec<u8> {
        let mut out = Vec::new();
        let palette = &[0u8, 0, 0, 255, 255, 255]; // index 0 = black, 1 = white
        let mut enc = gif::Encoder::new(&mut out, cw, ch, palette).unwrap();
        enc.set_repeat(gif::Repeat::Infinite).unwrap();
        // Frame 1: full canvas, all white.
        let mut f1 = gif::Frame::default();
        f1.width = cw;
        f1.height = ch;
        f1.buffer = std::borrow::Cow::Owned(vec![1u8; cw as usize * ch as usize]);
        enc.write_frame(&f1).unwrap();
        // Frame 2: sub-rect (top-left quadrant), all black — delta frame.
        let mut f2 = gif::Frame::default();
        f2.left = 0;
        f2.top = 0;
        f2.width = cw / 2;
        f2.height = ch / 2;
        f2.buffer = std::borrow::Cow::Owned(vec![0u8; (cw / 2) as usize * (ch / 2) as usize]);
        enc.write_frame(&f2).unwrap();
        drop(enc);
        out
    }

    #[test]
    fn resize_gif_passthrough_when_already_at_target_size() {
        let gif_bytes = make_gif(4, 4);
        let result = resize_gif(&gif_bytes, 4, 4, |_| {}).unwrap();
        assert_eq!(result, gif_bytes);
    }

    #[test]
    fn resize_gif_reports_monotonic_progress_ending_at_100() {
        let gif_bytes = make_gif_with_partial_frame(8, 8); // 2 frames
        let mut reported = Vec::new();
        resize_gif(&gif_bytes, 4, 4, |p| reported.push(p)).unwrap();
        assert_eq!(reported.len(), 2, "one report per frame");
        assert!(reported.windows(2).all(|w| w[0] <= w[1]), "{reported:?}");
        assert_eq!(reported.last(), Some(&100));
    }

    #[test]
    fn decode_limited_rejects_oversized_source_dimensions() {
        // A header declaring dimensions above MAX_DECODE_DIM is refused before the
        // decoder allocates, even though the encoded bytes stay tiny; a small image
        // still decodes fine.
        let mut big = Vec::new();
        image::GrayImage::new(MAX_DECODE_DIM + 1, 1)
            .write_to(&mut std::io::Cursor::new(&mut big), image::ImageFormat::Png)
            .unwrap();
        assert!(decode_limited(&big).is_err());

        let mut small = Vec::new();
        image::GrayImage::new(8, 8)
            .write_to(
                &mut std::io::Cursor::new(&mut small),
                image::ImageFormat::Png,
            )
            .unwrap();
        assert!(decode_limited(&small).is_ok());
    }

    #[test]
    fn decode_limited_rejects_images_over_total_pixel_budget() {
        // Each side is below the dimension limit, but the decoded RGBA image
        // would exceed the 16 Mi-pixel allocation budget.
        let mut data = Vec::new();
        image::GrayImage::new(5000, 5000)
            .write_to(
                &mut std::io::Cursor::new(&mut data),
                image::ImageFormat::Png,
            )
            .unwrap();
        assert!(decode_limited(&data).is_err());
    }

    #[test]
    fn resize_gif_passthrough_reports_no_progress() {
        let gif_bytes = make_gif(4, 4);
        let mut reported = Vec::new();
        resize_gif(&gif_bytes, 4, 4, |p| reported.push(p)).unwrap();
        assert!(reported.is_empty());
    }

    #[test]
    fn resize_gif_produces_output_with_correct_dimensions() {
        let gif_bytes = make_gif(8, 8);
        let result = resize_gif(&gif_bytes, 4, 4, |_| {}).unwrap();
        let mut opts = gif::DecodeOptions::new();
        opts.set_color_output(gif::ColorOutput::Indexed);
        let decoder = opts.read_info(std::io::Cursor::new(&result)).unwrap();
        assert_eq!(decoder.width(), 4);
        assert_eq!(decoder.height(), 4);
    }

    #[test]
    fn resize_gif_composes_partial_frames_to_full_size() {
        // Sub-rect delta frames must be composited before resizing, not stretched.
        let gif_bytes = make_gif_with_partial_frame(8, 8);
        let result = resize_gif(&gif_bytes, 4, 4, |_| {}).unwrap();

        let mut opts = gif::DecodeOptions::new();
        opts.set_color_output(gif::ColorOutput::Indexed);
        let mut decoder = opts.read_info(std::io::Cursor::new(&result)).unwrap();
        assert_eq!(decoder.width(), 4);
        assert_eq!(decoder.height(), 4);

        // Every output frame must be full-size (no sub-rects).
        let mut frame_count = 0usize;
        while let Some(f) = decoder.read_next_frame().unwrap() {
            assert_eq!(f.left, 0, "frame {frame_count} left offset must be 0");
            assert_eq!(f.top, 0, "frame {frame_count} top offset must be 0");
            assert_eq!(f.width, 4, "frame {frame_count} width must be 4");
            assert_eq!(f.height, 4, "frame {frame_count} height must be 4");
            frame_count += 1;
        }
        assert_eq!(frame_count, 2);
    }

    // ── compress_for_storage ──────────────────────────────────────────────

    fn make_png(w: u32, h: u32) -> Vec<u8> {
        use image::ImageEncoder as _;
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200u8, 100, 50]));
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    #[test]
    fn read_image_bounded_reads_small_file_and_rejects_missing() {
        use std::io::Write as _;
        let png = make_png(4, 4);
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&png).unwrap();
        assert_eq!(read_image_bounded(f.path()).unwrap(), png);

        assert!(read_image_bounded(std::path::Path::new("no-such-image.png")).is_err());
    }

    #[test]
    fn compress_for_storage_keeps_small_png_as_is() {
        let png = make_png(4, 4);
        let (data, ext) = compress_for_storage(&png, 10, 10, |_| {}).unwrap();
        assert_eq!(ext, "png");
        assert_eq!(data, png);
    }

    #[test]
    fn compress_for_storage_reencodes_oversized_png_as_jpeg() {
        let png = make_png(20, 20);
        let (data, ext) = compress_for_storage(&png, 10, 10, |_| {}).unwrap();
        assert_eq!(ext, "jpg");
        // JPEG magic bytes
        assert_eq!(&data[..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn compress_for_storage_resizes_gif_to_target_dimensions() {
        let gif_bytes = make_gif(8, 8);
        let (data, ext) = compress_for_storage(&gif_bytes, 4, 4, |_| {}).unwrap();
        assert_eq!(ext, "gif");
        let mut opts = gif::DecodeOptions::new();
        opts.set_color_output(gif::ColorOutput::Indexed);
        let decoder = opts.read_info(std::io::Cursor::new(&data)).unwrap();
        assert_eq!(decoder.width(), 4);
        assert_eq!(decoder.height(), 4);
    }

    // ── codecs ────────────────────────────────────────────────────────────

    #[test]
    fn rgba_to_bgr888_reorders_and_drops_alpha() {
        let rgba = [10u8, 20, 30, 255, 40, 50, 60, 128];
        assert_eq!(rgba_to_bgr888(&rgba), vec![30, 20, 10, 60, 50, 40]);
    }

    #[test]
    fn rotate_rgba_square_90_moves_top_left_to_top_right() {
        let mut src = vec![0u8; 2 * 2 * 4];
        src[0..4].copy_from_slice(&[1, 1, 1, 255]);
        let out = rotate_rgba_square(&src, 2, 90);
        assert_eq!(&out[4..8], &[1, 1, 1, 255]);
        assert_eq!(&out[0..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn rotate_rgba_square_zero_and_bad_size_passthrough() {
        let src = vec![9u8; 2 * 2 * 4];
        assert_eq!(rotate_rgba_square(&src, 2, 0), src);
        assert_eq!(rotate_rgba_square(&src, 4, 90), src);
    }

    #[test]
    fn q565_payload_has_magic_dimensions_and_end() {
        let rgba = [255u8; 4 * 4 * 4];
        let out = rgba_to_q565(&rgba, 4, 4).unwrap();
        assert_eq!(&out[0..4], b"q565");
        assert_eq!(&out[4..8], &[4, 0, 4, 0]);
        assert_eq!(*out.last().unwrap(), 0xFF);
    }

    #[test]
    fn q565_rejects_size_mismatch() {
        assert!(rgba_to_q565(&[0u8; 4], 2, 2).is_err());
    }

    #[test]
    fn decode_static_image_rgba_resizes_to_native() {
        use image::ImageEncoder as _;
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255u8, 0, 0, 255]));
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 2, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        let rgba = decode_static_image_rgba(&png, 4, 4).unwrap();
        assert_eq!(rgba.len(), 4 * 4 * 4);
        assert!(rgba.chunks_exact(4).all(|px| px == [255, 0, 0, 255]));
    }

    proptest::proptest! {
        #[test]
        fn rotate_rgba_square_four_quarters_is_identity(
            pixels in proptest::collection::vec(proptest::num::u8::ANY, 5 * 5 * 4..=5 * 5 * 4)
        ) {
            let mut img = pixels.clone();
            for _ in 0..4 {
                img = rotate_rgba_square(&img, 5, 90);
            }
            proptest::prop_assert_eq!(img, pixels);
        }
    }
}
