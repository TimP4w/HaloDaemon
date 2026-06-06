use anyhow::{anyhow, Result};
use base64::Engine as _;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

use super::{persist_device_state, require_device_owned};
use crate::ipc::ClientHandle;
use crate::state::AppState;
use halod_protocol::types::RgbState;

/// If the device is currently controlled by the LCD engine, remove it from the engine config
/// so the engine stops overwriting the image we're about to set.
async fn deactivate_engine_for_device(app: &Arc<AppState>, device_id: &str) {
    let mut cfg = app.config.write().await;
    let profile = cfg.active_profile_data_mut();
    if profile.lcd_engine_state.device_templates.remove(device_id).is_some() {
        drop(cfg);
        if let Some(engines) = app.engines.get() {
            engines.lcd.remove_device(device_id).await;
        }
        app.request_config_save(app.config.read().await.clone());
    }
}

/// Re-apply the device's saved RGB state after an LCD image upload.
/// Uploading an image resets the Kraken's LED ring to white; this restores the saved state.
/// Skipped when the current state is Engine (controlled externally) or not yet set.
async fn restore_rgb(device: &dyn crate::drivers::Device) {
    if let Some(rgb) = device.as_rgb() {
        if let Some(state) = rgb.current_state() {
            if !matches!(state, RgbState::Engine) {
                if let Err(e) = rgb.apply(state).await {
                    log::warn!("[LCD] RGB restore failed after image upload: {e}");
                }
            }
        }
    }
}

fn sniff_ext(data: &[u8]) -> &'static str {
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        "gif"
    } else if data.starts_with(&[0xFF, 0xD8]) {
        "jpg"
    } else {
        "png"
    }
}

/// Resize a GIF to exact LCD dimensions.
///
/// Decodes as fully-composited RGBA frames so that sub-rect delta frames (the GIF
/// optimisation where only changed pixels are stored) are composited correctly before
/// resizing.  The old indexed-pixel path stretched sub-rect frames to full LCD size,
/// which caused the "wobbly / skipping" artefacts.
pub fn resize_gif(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    use image::codecs::gif::{GifDecoder, GifEncoder};
    use image::AnimationDecoder;
    use std::io::Cursor;

    // Read canvas dimensions from the GIF header for the fast-path check.
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

    // Decode as composited RGBA frames — handles sub-rect frames, transparency,
    // and all GIF disposal methods correctly.
    let decoder = GifDecoder::new(Cursor::new(data))
        .map_err(|e| anyhow::anyhow!("GIF decode: {e}"))?;

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
    }
    drop(encoder);

    log::debug!(
        "[LCD] resize_gif: {} frames {}×{} → {}×{} ({} bytes)",
        frame_count, src_w, src_h, width, height, out.len()
    );
    Ok(out)
}

/// Compress an image for disk storage at LCD resolution.
/// - GIFs: each frame composited to RGBA and resized to LCD dims (see `resize_gif`).
/// - Static images already within LCD resolution: kept as-is.
/// - Static images larger than LCD: resized to LCD dims and saved as JPEG.
fn compress_for_storage(data: &[u8], width: u32, height: u32) -> Result<(Vec<u8>, &'static str)> {
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        let resized = resize_gif(data, width, height)?;
        return Ok((resized, "gif"));
    }

    let img = image::load_from_memory(data)?;
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

/// Upload a new image (binary data arrives as base64 in `msg["data_b64"]`).
/// Saves the file to lcd_images_dir and applies it to the device.
pub async fn set_screen_image(msg: Value, app: Arc<AppState>, client: ClientHandle) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let lcd = device
        .as_lcd()
        .ok_or_else(|| anyhow!("device does not support LCD"))?;

    let b64 = msg["data_b64"]
        .as_str()
        .ok_or_else(|| anyhow!("missing data_b64"))?;
    let data = base64::engine::general_purpose::STANDARD.decode(b64)?;
    log::debug!("[LCD] set_screen_image: received {} bytes for device {}", data.len(), device.id());

    let desc = lcd.lcd_descriptor();
    let (width, height) = (desc.width, desc.height);

    // Compression is CPU-intensive (Lanczos3 resize). Run it off the async thread
    // so the IPC loop stays responsive during processing.
    let (compressed, ext) = tokio::task::spawn_blocking(move || {
        compress_for_storage(&data, width, height)
    })
    .await??;
    log::debug!("[LCD] compressed to {} bytes ({})", compressed.len(), ext);

    let filename = format!("{}.{}", Uuid::new_v4(), ext);
    let dir = crate::config::lcd_images_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(&filename), &compressed)?;
    log::info!("[LCD] saved image as {}", filename);

    deactivate_engine_for_device(&app, &device.id()).await;
    lcd.set_image(&compressed).await?;
    restore_rgb(device.as_ref()).await;
    lcd.set_active_image_filename(Some(filename)).await;
    persist_device_state(&app, device.as_ref()).await;

    let req_id = msg["request_id"].as_str().unwrap_or("").to_string();
    client.send_json(&json!({ "type": "image_uploaded", "request_id": req_id }));
    Ok(())
}

/// Apply an image already present in the library by filename.
pub async fn set_screen_image_from_library(msg: Value, app: Arc<AppState>, client: ClientHandle) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let lcd = device
        .as_lcd()
        .ok_or_else(|| anyhow!("device does not support LCD"))?;

    let filename = msg["filename"]
        .as_str()
        .ok_or_else(|| anyhow!("missing filename"))?
        .to_owned();

    // Sanitize: reject path separators.
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        anyhow::bail!("invalid filename");
    }

    let path = crate::config::lcd_images_dir().join(&filename);
    let data = std::fs::read(&path).map_err(|e| anyhow!("image not found: {e}"))?;
    log::info!("[LCD] applying library image {} ({} bytes) to device {}", filename, data.len(), device.id());

    deactivate_engine_for_device(&app, &device.id()).await;
    lcd.set_image(&data).await?;
    restore_rgb(device.as_ref()).await;
    lcd.set_active_image_filename(Some(filename)).await;
    persist_device_state(&app, device.as_ref()).await;

    let req_id = msg["request_id"].as_str().unwrap_or("").to_string();
    client.send_json(&json!({ "type": "image_uploaded", "request_id": req_id }));
    Ok(())
}

pub async fn set_screen_rotation(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let lcd = device
        .as_lcd()
        .ok_or_else(|| anyhow!("device does not support LCD"))?;
    let degrees = msg["degrees"]
        .as_u64()
        .ok_or_else(|| anyhow!("missing degrees"))? as u32;
    if ![0u32, 90, 180, 270].contains(&degrees) {
        anyhow::bail!("degrees must be 0, 90, 180, or 270");
    }
    log::info!("[LCD] set_screen_rotation: {}° for device {}", degrees, device.id());
    lcd.set_rotation(degrees).await?;
    persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

pub async fn set_screen_brightness(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let lcd = device
        .as_lcd()
        .ok_or_else(|| anyhow!("device does not support LCD"))?;
    let brightness = msg["brightness"]
        .as_u64()
        .ok_or_else(|| anyhow!("missing brightness"))? as u8;
    log::info!("[LCD] set_screen_brightness: {} for device {}", brightness, device.id());
    lcd.set_brightness(brightness).await?;
    persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

pub async fn set_screen_default(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let lcd = device
        .as_lcd()
        .ok_or_else(|| anyhow!("device does not support LCD"))?;
    log::info!("[LCD] set_screen_default for device {}", device.id());
    deactivate_engine_for_device(&app, &device.id()).await;
    lcd.reset_to_default().await?;
    persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

/// Return a list of all images in the lcd_images_dir to the requesting client.
pub async fn list_lcd_images(_msg: Value, client: ClientHandle) -> Result<()> {
    let dir = crate::config::lcd_images_dir();
    let mut files: Vec<Value> = Vec::new();
    if dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                files.push(json!({ "name": name, "size_bytes": size }));
            }
        }
    }
    log::debug!("[LCD] list_lcd_images: {} files", files.len());
    client.send_json(&json!({ "type": "lcd_images", "files": files }));
    Ok(())
}

/// Delete a named image from the lcd_images_dir.
pub async fn delete_lcd_image(msg: Value) -> Result<()> {
    let filename = msg["filename"]
        .as_str()
        .ok_or_else(|| anyhow!("missing filename"))?;
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        anyhow::bail!("invalid filename");
    }
    let path = crate::config::lcd_images_dir().join(filename);
    match std::fs::remove_file(&path) {
        Ok(()) => log::info!("[LCD] deleted image {}", filename),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── sniff_ext ─────────────────────────────────────────────────────────

    #[test]
    fn sniff_ext_detects_gif89a() {
        assert_eq!(sniff_ext(b"GIF89aXXX"), "gif");
    }

    #[test]
    fn sniff_ext_detects_gif87a() {
        assert_eq!(sniff_ext(b"GIF87aXXX"), "gif");
    }

    #[test]
    fn sniff_ext_detects_jpeg() {
        assert_eq!(sniff_ext(&[0xFF, 0xD8, 0x00]), "jpg");
    }

    #[test]
    fn sniff_ext_defaults_to_png() {
        assert_eq!(sniff_ext(&[0x89, 0x50, 0x4E, 0x47]), "png");
        assert_eq!(sniff_ext(b"anything"), "png");
    }

    // ── resize_gif ────────────────────────────────────────────────────────

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
        let result = resize_gif(&gif_bytes, 4, 4).unwrap();
        assert_eq!(result, gif_bytes);
    }

    #[test]
    fn resize_gif_produces_output_with_correct_dimensions() {
        let gif_bytes = make_gif(8, 8);
        let result = resize_gif(&gif_bytes, 4, 4).unwrap();
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
        let result = resize_gif(&gif_bytes, 4, 4).unwrap();

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
        let (data, ext) = compress_for_storage(&png, 10, 10).unwrap();
        assert_eq!(ext, "png");
        assert_eq!(data, png);
    }

    #[test]
    fn compress_for_storage_reencodes_oversized_png_as_jpeg() {
        let png = make_png(20, 20);
        let (data, ext) = compress_for_storage(&png, 10, 10).unwrap();
        assert_eq!(ext, "jpg");
        // JPEG magic bytes
        assert_eq!(&data[..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn compress_for_storage_resizes_gif_to_target_dimensions() {
        let gif_bytes = make_gif(8, 8);
        let (data, ext) = compress_for_storage(&gif_bytes, 4, 4).unwrap();
        assert_eq!(ext, "gif");
        let mut opts = gif::DecodeOptions::new();
        opts.set_color_output(gif::ColorOutput::Indexed);
        let decoder = opts.read_info(std::io::Cursor::new(&data)).unwrap();
        assert_eq!(decoder.width(), 4);
        assert_eq!(decoder.height(), 4);
    }
}
