//! Image resize/compression for the LCD path — reducing an uploaded image to a
//! panel's native resolution. Depends on the `image`/`gif` crates, so it stays
//! daemon-side; the pure filename/format helpers live in
//! `halod_shared::types` (`validate_image_filename`, `sniff_ext`) so the GUI
//! can pre-validate before uploading.

use anyhow::Result;
use halod_shared::types::sniff_ext;

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
    if src_w == width && src_h == height {
        return Ok(data.to_vec());
    }

    const MAX_GIF_DIM: u32 = 8192;
    if src_w > MAX_GIF_DIM || src_h > MAX_GIF_DIM {
        anyhow::bail!("GIF dimensions {src_w}×{src_h} exceed the {MAX_GIF_DIM} px limit");
    }

    // Composite all frames to RGBA before resizing
    let decoder =
        GifDecoder::new(Cursor::new(data)).map_err(|e| anyhow::anyhow!("GIF decode: {e}"))?;

    let mut out = Vec::new();
    let mut encoder = GifEncoder::new_with_speed(&mut out, 30);
    encoder
        .set_repeat(image::codecs::gif::Repeat::Infinite)
        .map_err(|e| anyhow::anyhow!("GIF set repeat: {e}"))?;

    const MAX_GIF_FRAMES: usize = 500;
    let total_frames = count_gif_frames(data)?;
    if total_frames > MAX_GIF_FRAMES {
        anyhow::bail!("GIF exceeds the {MAX_GIF_FRAMES} frame limit");
    }
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

    // Decode under explicit dimension limits: `data` is attacker-controlled
    // (uploaded over IPC), and a valid header can declare enormous dimensions
    // that balloon allocation. 8192² is far above any LCD panel.
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    reader.limits(limits);
    let img = reader.decode()?;
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
}
