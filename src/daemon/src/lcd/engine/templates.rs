// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;
use std::path::Path;

use image::{Rgba, RgbaImage};

use halod_shared::types::{RgbColor, ScreenShape, Sensor};

static FONT_BYTES: &[u8] = include_bytes!("../../../../assets/fonts/NotoSans-Regular.ttf");
static MONO_FONT_BYTES: &[u8] =
    include_bytes!("../../../../assets/fonts/JetBrainsMono-Regular.ttf");
static INTER_FONT_BYTES: &[u8] = include_bytes!("../../../../assets/fonts/InterTight-400.ttf");

pub(super) fn load_font_arc() -> ab_glyph::FontArc {
    ab_glyph::FontArc::try_from_slice(FONT_BYTES)
        .expect("bundled font NotoSans-Regular.ttf is valid")
}

pub(super) fn load_mono_font_arc() -> ab_glyph::FontArc {
    ab_glyph::FontArc::try_from_slice(MONO_FONT_BYTES)
        .expect("bundled font JetBrainsMono-Regular.ttf is valid")
}

pub(super) fn load_inter_font_arc() -> ab_glyph::FontArc {
    ab_glyph::FontArc::try_from_slice(INTER_FONT_BYTES)
        .expect("bundled font InterTight-400.ttf is valid")
}

pub(super) fn load_system_font_arc(family: &str) -> Option<ab_glyph::FontArc> {
    let (bytes, index) = halod_shared::system_fonts::data(family)?;
    let font = ab_glyph::FontVec::try_from_vec_and_index(bytes, index).ok()?;
    Some(ab_glyph::FontArc::new(font))
}

pub struct TemplateCtx<'a> {
    pub width: u32,
    pub height: u32,
    pub screen_shape: ScreenShape,
    /// Seconds since the engine started.
    pub t: f64,
    /// Live sensor readings keyed by sensor id.
    pub sensors: &'a HashMap<String, Sensor>,
}

// ── Background (shared optional image/gif backdrop) ─────────────────────────────

struct GifFrame {
    image: RgbaImage,
    delay_ms: f64,
}

/// Decode image bytes into `(frame, delay_ms)` pairs — one infinitely-held
/// frame for a static image, one per frame for an animated GIF.
pub(super) fn decode_image_frames(data: &[u8]) -> Result<Vec<(RgbaImage, f64)>, String> {
    use image::{codecs::gif::GifDecoder, AnimationDecoder, ImageDecoder};
    use std::io::Cursor;
    if !(data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
        // Decode under dimension limits: `data` is an uploaded LCD background,
        // attacker-controlled, and a valid header can declare enormous dimensions
        // that balloon allocation before any downscale (decompression bomb).
        let img = crate::util::image::decode_limited(data)
            .map_err(|e| e.to_string())?
            .to_rgba8();
        return Ok(vec![(img, f64::INFINITY)]);
    }
    let mut decoder = GifDecoder::new(Cursor::new(data)).map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(crate::util::image::MAX_DECODE_DIM);
    limits.max_image_height = Some(crate::util::image::MAX_DECODE_DIM);
    decoder.set_limits(limits).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for frame in decoder.into_frames() {
        let frame = frame.map_err(|e| e.to_string())?;
        let (num, den) = frame.delay().numer_denom_ms();
        let delay_ms = if den == 0 {
            100.0
        } else {
            (num as f64 / den as f64).max(10.0)
        };
        out.push((frame.into_buffer(), delay_ms));
    }
    Ok(out)
}

/// Index of the frame visible at time `t` seconds, by cumulative delay.
pub(super) fn frame_at_ms(delays: &[f64], total_ms: f64, t: f64) -> usize {
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

/// Optional image/GIF backdrop; frames are decoded once.
#[derive(Default)]
pub struct Background {
    /// Frames, resized in place to the last `canvas()` resolution.
    frames: Vec<GifFrame>,
    total_ms: f64,
    /// 0–100 darkening applied to the image, for overlay legibility.
    dim: f64,
    /// Source `(filename, images_dir)` for reloads at new sizes; `None` when no image.
    source: Option<(String, std::path::PathBuf)>,
    /// The `(w, h)` `frames` are currently resized to; `None` until the first
    /// `canvas()` call.
    sized: Option<(u32, u32)>,
}

impl Background {
    /// Build directly from a filename + dim, bypassing the params map — used by
    /// templates whose background isn't sourced from the shared `image`/`dim`
    /// param pair (e.g. the custom template's `BgKind::Image`).
    pub(super) fn new(filename: &str, dim: f64, images_dir: &Path) -> Self {
        let frames = Self::load_frames(filename, images_dir);
        let total_ms = frames.iter().map(|f| f.delay_ms).sum();
        let source = (!filename.is_empty() && !frames.is_empty())
            .then(|| (filename.to_string(), images_dir.to_path_buf()));
        Self {
            frames,
            total_ms,
            dim: dim.clamp(0.0, 100.0),
            source,
            sized: None,
        }
    }

    fn load_frames(filename: &str, images_dir: &Path) -> Vec<GifFrame> {
        if filename.is_empty() {
            return Vec::new();
        }
        if halod_shared::types::validate_image_filename(filename).is_err() {
            log::warn!("[LCD bg] rejected image filename: {filename}");
            return Vec::new();
        }
        let path = images_dir.join(filename);
        let data = match crate::util::image::read_image_bounded(&path) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("[LCD bg] cannot read {filename}: {e}");
                return Vec::new();
            }
        };
        Self::decode_frames(&data).unwrap_or_else(|e| {
            log::warn!("[LCD bg] decode {filename} failed: {e}");
            Vec::new()
        })
    }

    fn decode_frames(data: &[u8]) -> Result<Vec<GifFrame>, String> {
        Ok(decode_image_frames(data)?
            .into_iter()
            .map(|(image, delay_ms)| GifFrame { image, delay_ms })
            .collect())
    }

    /// Index of the frame visible at engine time `t` seconds, by cumulative delay.
    fn frame_at(&self, t: f64) -> usize {
        let delays: Vec<f64> = self.frames.iter().map(|f| f.delay_ms).collect();
        frame_at_ms(&delays, self.total_ms, t)
    }

    pub(super) fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    /// Resize frames to `w`×`h`; reloads from source on a later resize to avoid compounding.
    fn resize_to(&mut self, w: u32, h: u32) {
        if self.sized.is_some() {
            if let Some((filename, images_dir)) = &self.source {
                self.frames = Self::load_frames(filename, images_dir);
            }
        }
        for frame in &mut self.frames {
            if frame.image.width() != w || frame.image.height() != h {
                frame.image = image::imageops::resize(
                    &frame.image,
                    w,
                    h,
                    image::imageops::FilterType::Triangle,
                );
            }
        }
        self.sized = Some((w, h));
    }

    /// Base canvas at `w`×`h`; frames sized once, solid fill when no image.
    pub(super) fn canvas(&mut self, w: u32, h: u32, t: f64, solid: RgbColor) -> RgbaImage {
        if self.frames.is_empty() {
            return RgbaImage::from_pixel(w, h, rgba(solid));
        }
        if self.sized != Some((w, h)) {
            self.resize_to(w, h);
        }
        let mut img = self.frames[self.frame_at(t)].image.clone();
        let dim = (self.dim / 100.0).clamp(0.0, 1.0);
        if dim > 0.0 {
            let a = (dim * 255.0) as u16;
            for px in img.pixels_mut() {
                px.0[0] = ((px.0[0] as u16 * (255 - a)) / 255) as u8;
                px.0[1] = ((px.0[1] as u16 * (255 - a)) / 255) as u8;
                px.0[2] = ((px.0[2] as u16 * (255 - a)) / 255) as u8;
                px.0[3] = 255;
            }
        }
        img
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub(super) fn rgba(c: RgbColor) -> Rgba<u8> {
    Rgba([c.r, c.g, c.b, 255])
}

/// Scale every channel of `c` by `factor` (used for the unfilled gauge track).
pub(super) fn dim_color(c: RgbColor, factor: f32) -> RgbColor {
    let scale = |v: u8| (v as f32 * factor).round().clamp(0.0, 255.0) as u8;
    RgbColor {
        r: scale(c.r),
        g: scale(c.g),
        b: scale(c.b),
    }
}

/// Encode a frame as PNG into a reusable buffer for the UI preview broadcast.
///
/// The device itself never sees this — it gets raw RGBA streamed by the LCD
/// engine. PNG is only for the UI preview. Fast compression keeps the per-tick
/// cost low. `buf` is cleared before writing so the caller can reuse it across
/// ticks.
pub fn encode_png_into(img: &RgbaImage, buf: &mut Vec<u8>) -> Result<(), String> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::{ExtendedColorType, ImageEncoder};

    buf.clear();
    PngEncoder::new_with_quality(buf, CompressionType::Fast, FilterType::NoFilter)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(|e| format!("PNG encode: {e}"))?;
    Ok(())
}

/// Convenience wrapper that allocates; prefer [`encode_png_into`] in hot paths.
#[cfg(test)]
pub(crate) fn encode_png(img: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    encode_png_into(img, &mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_rejects_traversal_filename() {
        let root = tempfile::tempdir().unwrap();
        let images_dir = root.path().join("lcd_images");
        std::fs::create_dir_all(&images_dir).unwrap();
        let img = RgbaImage::from_pixel(4, 4, Rgba([9, 9, 9, 255]));
        std::fs::write(root.path().join("escape.png"), encode_png(&img).unwrap()).unwrap();

        let bg = Background::new("../escape.png", 0.0, &images_dir);
        assert!(bg.frames.is_empty());

        // Positive control: the same bytes load fine from inside the dir.
        std::fs::write(images_dir.join("ok.png"), encode_png(&img).unwrap()).unwrap();
        assert!(!Background::new("ok.png", 0.0, &images_dir)
            .frames
            .is_empty());
    }

    fn write_test_bg(images_dir: &Path, name: &str, w: u32, h: u32) {
        std::fs::create_dir_all(images_dir).unwrap();
        let img = RgbaImage::from_pixel(w, h, Rgba([9, 9, 9, 255]));
        std::fs::write(images_dir.join(name), encode_png(&img).unwrap()).unwrap();
    }

    #[test]
    fn canvas_resizes_frames_in_place_once_and_reuses_for_same_size() {
        let root = tempfile::tempdir().unwrap();
        let images_dir = root.path().join("lcd_images");
        write_test_bg(&images_dir, "bg.png", 40, 20);

        let mut bg = Background::new("bg.png", 0.0, &images_dir);
        assert_eq!(bg.frames[0].image.dimensions(), (40, 20));

        let out = bg.canvas(10, 5, 0.0, RgbColor { r: 0, g: 0, b: 0 });
        assert_eq!(out.dimensions(), (10, 5));
        assert_eq!(
            bg.frames[0].image.dimensions(),
            (10, 5),
            "frames must be resized in place, not just the returned copy"
        );

        // A second same-size call reuses the stored frame rather than resizing again.
        let sized_before = bg.sized;
        let out2 = bg.canvas(10, 5, 0.0, RgbColor { r: 0, g: 0, b: 0 });
        assert_eq!(out2.dimensions(), (10, 5));
        assert_eq!(bg.sized, sized_before);
    }

    #[test]
    fn canvas_first_call_renders_from_already_decoded_frames_without_reloading() {
        let root = tempfile::tempdir().unwrap();
        let images_dir = root.path().join("lcd_images");
        write_test_bg(&images_dir, "bg.png", 40, 20);

        let mut bg = Background::new("bg.png", 0.0, &images_dir);
        // Delete the source after construction: a correct first `canvas()` call
        // renders from the frames already decoded in `new`, not by reloading —
        // reloading here would find nothing and panic indexing an empty `frames`.
        std::fs::remove_file(images_dir.join("bg.png")).unwrap();

        let out = bg.canvas(10, 5, 0.0, RgbColor { r: 0, g: 0, b: 0 });
        assert_eq!(out.dimensions(), (10, 5));
    }

    #[test]
    fn canvas_reloads_and_resizes_when_size_changes() {
        let root = tempfile::tempdir().unwrap();
        let images_dir = root.path().join("lcd_images");
        write_test_bg(&images_dir, "bg.png", 40, 20);

        let mut bg = Background::new("bg.png", 0.0, &images_dir);
        let _ = bg.canvas(10, 5, 0.0, RgbColor { r: 0, g: 0, b: 0 });
        assert_eq!(bg.frames[0].image.dimensions(), (10, 5));

        let out = bg.canvas(30, 15, 0.0, RgbColor { r: 0, g: 0, b: 0 });
        assert_eq!(out.dimensions(), (30, 15));
        assert_eq!(bg.frames[0].image.dimensions(), (30, 15));
    }

    #[test]
    fn encode_png_produces_png_magic() {
        let img = RgbaImage::from_pixel(8, 8, Rgba([1, 2, 3, 255]));
        let png = encode_png(&img).expect("encode failed");
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    }

    #[test]
    fn dim_color_scales_channels() {
        let dimmed = dim_color(
            RgbColor {
                r: 200,
                g: 100,
                b: 40,
            },
            0.25,
        );
        assert_eq!(
            dimmed,
            RgbColor {
                r: 50,
                g: 25,
                b: 10
            }
        );
    }

    /// Valid 1×1 PNG produced by the same `encode_png` helper used in production.
    fn tiny_png() -> Vec<u8> {
        let img = RgbaImage::from_pixel(1, 1, Rgba([0, 128, 255, 255]));
        encode_png(&img).expect("encode_png failed in test helper")
    }

    #[test]
    fn decode_frames_png_yields_single_infinite_frame() {
        let frames = Background::decode_frames(&tiny_png()).expect("decode failed");
        assert_eq!(frames.len(), 1, "PNG must produce exactly one frame");
        assert!(
            frames[0].delay_ms.is_infinite(),
            "static PNG frame must have infinite delay"
        );
    }

    #[test]
    fn decode_frames_unknown_format_returns_err() {
        let bad = b"not an image at all";
        assert!(Background::decode_frames(bad).is_err());
    }

    fn two_frame_bg(delay_a: f64, delay_b: f64) -> Background {
        let px = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 255]));
        let frames = vec![
            GifFrame {
                image: px.clone(),
                delay_ms: delay_a,
            },
            GifFrame {
                image: px,
                delay_ms: delay_b,
            },
        ];
        let total_ms = delay_a + delay_b;
        Background {
            frames,
            total_ms,
            dim: 0.0,
            ..Default::default()
        }
    }

    #[test]
    fn frame_at_returns_zero_for_single_frame() {
        let px = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 255]));
        let bg = Background {
            frames: vec![GifFrame {
                image: px,
                delay_ms: 100.0,
            }],
            total_ms: 100.0,
            dim: 0.0,
            ..Default::default()
        };
        assert_eq!(bg.frame_at(0.0), 0);
        assert_eq!(bg.frame_at(99.999), 0);
    }

    #[test]
    fn frame_at_two_frames_selects_correct_index() {
        // Frame 0: 100 ms, Frame 1: 200 ms → total 300 ms
        let bg = two_frame_bg(100.0, 200.0);

        // t=0 → position 0 ms → frame 0
        assert_eq!(bg.frame_at(0.0), 0);
        // t=0.099 → position 99 ms → still frame 0
        assert_eq!(bg.frame_at(0.099), 0);
        // t=0.1 → position 100 ms → frame 1
        assert_eq!(bg.frame_at(0.1), 1);
        // t=0.299 → position 299 ms → still frame 1
        assert_eq!(bg.frame_at(0.299), 1);
        // t=0.3 → wraps back to position 0 → frame 0
        assert_eq!(bg.frame_at(0.3), 0);
    }

    #[test]
    fn frame_at_infinite_total_always_returns_zero() {
        let px = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 255]));
        let bg = Background {
            frames: vec![
                GifFrame {
                    image: px.clone(),
                    delay_ms: f64::INFINITY,
                },
                GifFrame {
                    image: px,
                    delay_ms: 100.0,
                },
            ],
            total_ms: f64::INFINITY,
            dim: 0.0,
            ..Default::default()
        };
        assert_eq!(bg.frame_at(0.0), 0);
        assert_eq!(bg.frame_at(1000.0), 0);
    }
}
