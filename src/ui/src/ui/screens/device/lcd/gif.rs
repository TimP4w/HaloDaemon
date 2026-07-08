// SPDX-License-Identifier: GPL-3.0-or-later
//! GIF decoding/streaming and the on-disk image library texture cache.

use std::path::{Path, PathBuf};
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use egui::Color32;

use crate::ui::screens::device::{DeviceUi, GifFrame, GifTex, TabCtx};

// ── Textures ──────────────────────────────────────────────────────────────────

/// Build an egui texture from a raw RGBA8 buffer.
pub(super) fn rgba_texture(
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
fn load_tex_from_bytes(
    ctx: &egui::Context,
    bytes: &[u8],
    name: &str,
) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    Some(rgba_texture(ctx, name, &img.into_raw(), w, h))
}

// ── On-disk library ───────────────────────────────────────────────────────────

/// Path to the daemon's LCD image library. The daemon reports its own config
/// dir, so this is the exact directory it wrote the file to. `None` before the
/// daemon's state (and thus its config dir) has arrived.
pub(super) fn lcd_images_dir(ctx: &TabCtx) -> Option<PathBuf> {
    let dir = ctx.state.config_dir.as_str();
    (!dir.is_empty()).then(|| Path::new(dir).join(halod_shared::types::LCD_IMAGES_SUBDIR))
}

/// Read + decode a library file (PNG/JPEG, or a GIF's first frame) into a
/// texture, straight off disk. `None` if the dir is unknown or the read/decode
/// fails.
pub(super) fn load_tex_from_file(
    ctx: &TabCtx,
    egui_ctx: &egui::Context,
    filename: &str,
) -> Option<egui::TextureHandle> {
    let bytes = std::fs::read(lcd_images_dir(ctx)?.join(filename)).ok()?;
    load_tex_from_bytes(egui_ctx, &bytes, filename)
}

// ── GIF decoding ────────────────────────────────────────────────────────────

/// GIF frame delay in milliseconds from a `numer/denom` pair. Treats a zero
/// denominator or any sub-20 ms delay as 100 ms — the browser convention for
/// GIFs that under-report their timing (avoids a seizure-fast animation).
fn gif_delay_ms(numer: u32, denom: u32) -> u32 {
    if denom == 0 || numer / denom < 20 {
        100
    } else {
        numer / denom
    }
}

/// Decode `bytes` frame by frame, handing each to `on_frame`. Stops early when
/// `on_frame` returns `false` (its consumer went away). Isolated from the
/// threading/egui plumbing so it can be unit-tested directly.
fn stream_gif_frames(bytes: &[u8], mut on_frame: impl FnMut(GifFrame) -> bool) {
    use image::codecs::gif::GifDecoder;
    use image::AnimationDecoder;
    let Ok(decoder) = GifDecoder::new(std::io::Cursor::new(bytes)) else {
        return;
    };
    for frame in decoder.into_frames() {
        let Ok(frame) = frame else { break };
        let (numer, denom) = frame.delay().numer_denom_ms();
        let delay_ms = gif_delay_ms(numer, denom);
        let img = frame.into_buffer();
        let (w, h) = (img.width() as usize, img.height() as usize);
        if !on_frame((img.into_raw(), w, h, delay_ms)) {
            break;
        }
    }
}

/// Decode `path` on a background thread, streaming frames back and waking egui
/// after each so the first frame paints before the whole GIF is decoded.
pub(super) fn spawn_gif_stream(
    ctx: &egui::Context,
    path: PathBuf,
) -> std::sync::mpsc::Receiver<GifFrame> {
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let Ok(bytes) = std::fs::read(&path) else {
            return;
        };
        stream_gif_frames(&bytes, |frame| {
            let sent = tx.send(frame).is_ok();
            ctx.request_repaint();
            sent
        });
    });
    rx
}

/// Decode an animated GIF into one texture per frame, or `None` if the file
/// isn't an animated GIF (static images use the plain thumbnail cache).
pub(super) fn decode_gif_tex(
    ctx: &TabCtx,
    egui_ctx: &egui::Context,
    filename: &str,
) -> Option<GifTex> {
    let bytes = std::fs::read(lcd_images_dir(ctx)?.join(filename)).ok()?;
    if !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return None;
    }
    let mut frames = Vec::new();
    let mut delays = Vec::new();
    stream_gif_frames(&bytes, |(data, w, h, delay_ms)| {
        let name = format!("{filename}#gif{}", frames.len());
        frames.push(rgba_texture(egui_ctx, &name, &data, w, h));
        delays.push(delay_ms as f64);
        true
    });
    if frames.len() <= 1 {
        return None;
    }
    let total_ms = delays.iter().sum();
    Some(GifTex {
        frames,
        delays,
        total_ms,
    })
}

/// Decode at most one uncached Image-widget GIF per frame into
/// `st.lcd.gif_widget_tex`, mirroring [`decode_next_thumb`]'s one-per-frame budget.
pub(super) fn decode_next_gif<'a>(
    ui: &egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    filenames: impl Iterator<Item = &'a String>,
) {
    if lcd_images_dir(ctx).is_none() {
        return;
    }
    for filename in filenames {
        if st.lcd.gif_widget_tex.contains_key(filename) {
            continue;
        }
        let tex = decode_gif_tex(ctx, ui.ctx(), filename);
        st.lcd.gif_widget_tex.insert(filename.clone(), tex);
        ui.ctx().request_repaint();
        break;
    }
}

/// Which `gif_widget_tex` keys survive a prune: only filenames still
/// referenced by a current Image widget. Mirrors the daemon's
/// `image_cache` prune so a long-lived editor tab doesn't accumulate GPU
/// textures for GIFs removed from the layout. Pure so it's unit-tested.
pub(super) fn retained_gif_filenames<'a>(
    current: impl Iterator<Item = &'a String>,
    referenced: &std::collections::HashSet<String>,
) -> Vec<String> {
    current
        .filter(|f| referenced.contains(f.as_str()))
        .cloned()
        .collect()
}

/// Index of the GIF frame visible at time `t` seconds, by cumulative delay.
pub(super) fn gif_frame_at(delays: &[f64], total_ms: f64, t: f64) -> usize {
    if delays.len() <= 1 || !total_ms.is_finite() || total_ms <= 0.0 {
        return 0;
    }
    let mut pos = (t * 1000.0).rem_euclid(total_ms);
    for (i, &d) in delays.iter().enumerate() {
        if pos < d {
            return i;
        }
        pos -= d;
    }
    delays.len() - 1
}

/// Decode at most one uncached library file per frame into
/// `st.lcd.image_cache`, to avoid a lag spike on open. `requested` marks a
/// filename attempted so a failed decode isn't retried every frame.
pub(super) fn decode_next_thumb<'a>(
    ui: &egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    filenames: impl Iterator<Item = &'a String>,
) {
    if lcd_images_dir(ctx).is_none() {
        return;
    }
    for filename in filenames {
        if st.lcd.image_cache.contains_key(filename) || !st.lcd.requested.insert(filename.clone()) {
            continue;
        }
        let tex = if filename == halod_shared::lcd_custom::LOGO_IMAGE {
            rasterize_logo_tex(ui.ctx())
        } else {
            load_tex_from_file(ctx, ui.ctx(), filename)
        };
        if let Some(tex) = tex {
            st.lcd.image_cache.insert(filename.clone(), tex);
        }
        ui.ctx().request_repaint(); // decode the next file next frame
        break; // one decode per frame
    }
}

/// Rasterize the bundled logo SVG into a texture for the sentinel image widget.
fn rasterize_logo_tex(egui_ctx: &egui::Context) -> Option<egui::TextureHandle> {
    use resvg::{tiny_skia, usvg};
    const SIDE: u32 = 256;
    let tree = usvg::Tree::from_data(
        halod_shared::lcd_custom::LOGO_SVG,
        &usvg::Options::default(),
    )
    .ok()?;
    let size = tree.size().to_int_size();
    let long_edge = size.width().max(size.height()) as f32;
    if long_edge <= 0.0 {
        return None;
    }
    let scale = SIDE as f32 / long_edge;
    let mut pixmap = tiny_skia::Pixmap::new(SIDE, SIDE)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some(rgba_texture(
        egui_ctx,
        halod_shared::lcd_custom::LOGO_IMAGE,
        pixmap.data(),
        SIDE as usize,
        SIDE as usize,
    ))
}

/// Drop any GIF animation state. A no-op once already cleared, so it's cheap to
/// call every frame from the non-GIF preview branches.
pub(super) fn clear_gif(st: &mut DeviceUi) {
    if st.lcd.gif_source.is_empty() && st.lcd.gif_frames.is_empty() && st.lcd.gif_rx.is_none() {
        return;
    }
    st.lcd.gif_source.clear();
    st.lcd.gif_frames.clear();
    st.lcd.gif_tex.clear();
    st.lcd.gif_rx = None;
    st.lcd.gif_started = false;
    st.lcd.gif_idx = 0;
    st.lcd.gif_advance_at = None;
}

/// Advance the local GIF animation for `filename`, (re)starting a streaming
/// decode when the target changes. Shows the first decoded frame as soon as it
/// arrives, then flips frames on their per-frame delay.
pub(super) fn advance_gif(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    filename: &str,
    time: f64,
) {
    // (Re)start when the target GIF changes. Any in-flight decode for the prior
    // source is dropped (its thread notices the closed channel and exits).
    if st.lcd.gif_source != filename {
        st.lcd.gif_source = filename.to_string();
        st.lcd.gif_frames.clear();
        st.lcd.gif_tex.clear();
        st.lcd.gif_rx = None;
        st.lcd.gif_started = false;
        st.lcd.gif_idx = 0;
        st.lcd.gif_advance_at = None;
        st.lcd.preview_tex = None;
    }

    // Spawn the decode once per source; `gif_started` stops a zero-frame decode
    // (missing/corrupt file) from respawning a thread every frame.
    if !st.lcd.gif_started && st.lcd.gif_frames.is_empty() && st.lcd.gif_rx.is_none() {
        if let Some(dir) = lcd_images_dir(ctx) {
            st.lcd.gif_rx = Some(spawn_gif_stream(ui.ctx(), dir.join(filename)));
            st.lcd.gif_started = true;
        }
    }

    // Drain frames that streamed in since the last paint, building a texture
    // for each one so the frame-flip path below just does a cheap lookup.
    if let Some(rx) = &st.lcd.gif_rx {
        loop {
            match rx.try_recv() {
                Ok((rgba, w, h, delay_ms)) => {
                    st.lcd.gif_tex.push(rgba_texture(
                        ui.ctx(),
                        &format!("lcd_gif_{}", st.lcd.gif_frames.len()),
                        &rgba,
                        w,
                        h,
                    ));
                    st.lcd.gif_frames.push((rgba, w, h, delay_ms));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    st.lcd.gif_rx = None; // fully decoded
                    break;
                }
            }
        }
    }

    if st.lcd.gif_frames.is_empty() {
        return; // frame 0 still decoding — the spinner covers the gap
    }

    let should_advance = st.lcd.gif_advance_at.is_none_or(|due| time >= due);
    if should_advance {
        if st.lcd.gif_advance_at.is_some() {
            st.lcd.gif_idx = (st.lcd.gif_idx + 1) % st.lcd.gif_frames.len();
        }
        let idx = st.lcd.gif_idx;
        let delay_ms = st.lcd.gif_frames[idx].3;
        st.lcd.preview_tex = Some(st.lcd.gif_tex[idx].clone());
        st.lcd.gif_advance_at = Some(time + delay_ms as f64 / 1000.0);
    }

    // Wake when the frame is due to flip rather than repainting continuously.
    let remaining = st
        .lcd
        .gif_advance_at
        .map_or(0.0, |due| (due - time).max(0.0));
    ui.ctx()
        .request_repaint_after(Duration::from_secs_f64(remaining));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_gif_filenames_drops_unreferenced_and_keeps_referenced() {
        let current = [
            "a.gif".to_string(),
            "b.gif".to_string(),
            "c.gif".to_string(),
        ];
        let referenced: std::collections::HashSet<String> =
            ["a.gif".to_string(), "c.gif".to_string()].into();
        let kept = retained_gif_filenames(current.iter(), &referenced);
        assert_eq!(kept, vec!["a.gif".to_string(), "c.gif".to_string()]);
    }

    /// Encode a tiny `n`-frame GIF in memory for the decode tests.
    fn make_gif(n: usize) -> Vec<u8> {
        use image::codecs::gif::GifEncoder;
        use image::{Delay, Frame, Rgba, RgbaImage};
        let mut buf = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut buf);
            for i in 0..n {
                let img = RgbaImage::from_pixel(2, 2, Rgba([(i * 20) as u8, 0, 0, 255]));
                enc.encode_frame(Frame::from_parts(
                    img,
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                ))
                .unwrap();
            }
        }
        buf
    }

    #[test]
    fn stream_gif_frames_yields_every_frame_in_order() {
        let gif = make_gif(3);
        let mut frames: Vec<GifFrame> = Vec::new();
        stream_gif_frames(&gif, |f| {
            frames.push(f);
            true
        });
        assert_eq!(frames.len(), 3);
        for (_, w, h, delay) in &frames {
            assert_eq!((*w, *h, *delay), (2, 2, 100));
        }
        // A corrupt / non-GIF input yields nothing rather than panicking.
        let mut any = false;
        stream_gif_frames(b"not a gif", |_| {
            any = true;
            true
        });
        assert!(!any);
    }

    #[test]
    fn stream_gif_frames_stops_early_when_consumer_declines() {
        let gif = make_gif(5);
        let mut count = 0;
        stream_gif_frames(&gif, |_| {
            count += 1;
            count < 2 // stop after the second frame
        });
        assert_eq!(count, 2, "decode must halt once on_frame returns false");
    }

    #[test]
    fn gif_delay_clamps_fast_frames_to_100ms() {
        // Zero denominator → 100 ms sentinel (no divide-by-zero).
        assert_eq!(gif_delay_ms(40, 0), 100);
        // Sub-20 ms delays are bumped to 100 ms.
        assert_eq!(gif_delay_ms(10, 1), 100);
        assert_eq!(gif_delay_ms(0, 1), 100);
        // 20 ms and above pass through unchanged.
        assert_eq!(gif_delay_ms(20, 1), 20);
        assert_eq!(gif_delay_ms(100, 1), 100);
        assert_eq!(gif_delay_ms(60, 2), 30);
    }
}
