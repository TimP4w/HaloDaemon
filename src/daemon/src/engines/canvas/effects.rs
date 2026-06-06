use std::collections::HashMap;

use halod_protocol::types::{
    Animation, EffectParamDescriptor, EffectParamValue, ParamKind, RgbColor,
};
use tiny_skia::{Color, Pixmap};

// ── Color helpers ─────────────────────────────────────────────────────────────

pub(super) fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub(super) fn linear_to_srgb(c: f32) -> u8 {
    let c = if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (c.clamp(0.0, 1.0) * 255.0).round() as u8
}

pub(super) fn linear_to_led(c: f32, gamma: f32) -> u8 {
    (c.clamp(0.0, 1.0).powf(1.0 / gamma) * 255.0).round() as u8
}

// ── Traits ────────────────────────────────────────────────────────────────────

pub trait FrameSource: Send {
    fn render(&mut self, pixmap: &mut Pixmap, t: f32, dt: f32) -> Result<(), ()>;
}

trait Effect: Send {
    fn descriptor() -> Animation
    where
        Self: Sized;
    fn from_params(params: &HashMap<String, EffectParamValue>) -> Option<Box<dyn FrameSource>>
    where
        Self: Sized;
}

// ── StaticColorEffect ─────────────────────────────────────────────────────────

struct StaticColorEffect {
    color: RgbColor,
}

impl Effect for StaticColorEffect {
    fn descriptor() -> Animation {
        Animation {
            id: "static_color".to_string(),
            name: "Static Color".to_string(),
            params: vec![EffectParamDescriptor {
                id: "color".to_string(),
                label: "Color".to_string(),
                kind: ParamKind::Color,
                default: EffectParamValue::Color(RgbColor { r: 255, g: 0, b: 0 }),
            }],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Option<Box<dyn FrameSource>> {
        let color = match params.get("color") {
            Some(EffectParamValue::Color(c)) => *c,
            _ => RgbColor { r: 0, g: 0, b: 255 },
        };
        Some(Box::new(Self { color }))
    }
}

impl FrameSource for StaticColorEffect {
    fn render(&mut self, pixmap: &mut Pixmap, _t: f32, _dt: f32) -> Result<(), ()> {
        // tiny-skia uses linear premultiplied internally; scale [0,1] → [0,255].
        let r = (srgb_to_linear(self.color.r) * 255.0).round() as u8;
        let g = (srgb_to_linear(self.color.g) * 255.0).round() as u8;
        let b = (srgb_to_linear(self.color.b) * 255.0).round() as u8;
        pixmap.fill(Color::from_rgba8(r, g, b, 255));
        Ok(())
    }
}

// ── BreathingEffect ───────────────────────────────────────────────────────────

struct BreathingEffect {
    color: RgbColor,
    speed: f32,
}

impl Effect for BreathingEffect {
    fn descriptor() -> Animation {
        Animation {
            id: "breathing".to_string(),
            name: "Breathing".to_string(),
            params: vec![
                EffectParamDescriptor {
                    id: "color".to_string(),
                    label: "Color".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(RgbColor { r: 0, g: 128, b: 255 }),
                },
                EffectParamDescriptor {
                    id: "speed".to_string(),
                    label: "Speed".to_string(),
                    kind: ParamKind::Range { min: 0.1, max: 3.0, step: 0.1 },
                    default: EffectParamValue::Float(0.5),
                },
            ],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Option<Box<dyn FrameSource>> {
        let color = match params.get("color") {
            Some(EffectParamValue::Color(c)) => *c,
            _ => RgbColor { r: 0, g: 128, b: 255 },
        };
        let speed = match params.get("speed") {
            Some(EffectParamValue::Float(f)) => *f as f32,
            _ => 0.5,
        };
        Some(Box::new(Self { color, speed }))
    }
}

impl FrameSource for BreathingEffect {
    fn render(&mut self, pixmap: &mut Pixmap, t: f32, _dt: f32) -> Result<(), ()> {
        let phase = (t * self.speed * std::f32::consts::PI).sin();
        let brightness = phase * phase;
        let r = srgb_to_linear(self.color.r) * brightness;
        let g = srgb_to_linear(self.color.g) * brightness;
        let b = srgb_to_linear(self.color.b) * brightness;
        pixmap.fill(Color::from_rgba(r, g, b, 1.0).ok_or(())?);
        Ok(())
    }
}

// ── RainbowEffect ─────────────────────────────────────────────────────────────

/// Maps a hue in [0,1) to a fully-saturated, full-brightness sRGB color.
fn hue_to_srgb(hue: f32) -> (u8, u8, u8) {
    let h = (hue.rem_euclid(1.0)) * 6.0;
    let sector = h as i32;
    let f = h - sector as f32;
    let (r, g, b) = match sector {
        0 => (1.0, f, 0.0),
        1 => (1.0 - f, 1.0, 0.0),
        2 => (0.0, 1.0, f),
        3 => (0.0, 1.0 - f, 1.0),
        4 => (f, 0.0, 1.0),
        _ => (1.0, 0.0, 1.0 - f),
    };
    (
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    )
}

struct RainbowEffect {
    /// Hue cycles per second of scroll.
    speed: f32,
    /// Number of full rainbows visible across the canvas width.
    scale: f32,
}

impl Effect for RainbowEffect {
    fn descriptor() -> Animation {
        Animation {
            id: "rainbow".to_string(),
            name: "Rainbow".to_string(),
            params: vec![
                EffectParamDescriptor {
                    id: "speed".to_string(),
                    label: "Speed".to_string(),
                    kind: ParamKind::Range { min: 0.0, max: 2.0, step: 0.05 },
                    default: EffectParamValue::Float(0.2),
                },
                EffectParamDescriptor {
                    id: "scale".to_string(),
                    label: "Scale".to_string(),
                    kind: ParamKind::Range { min: 0.5, max: 5.0, step: 0.5 },
                    default: EffectParamValue::Float(1.0),
                },
            ],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Option<Box<dyn FrameSource>> {
        let speed = match params.get("speed") {
            Some(EffectParamValue::Float(f)) => *f as f32,
            _ => 0.2,
        };
        let scale = match params.get("scale") {
            Some(EffectParamValue::Float(f)) => *f as f32,
            _ => 1.0,
        };
        Some(Box::new(Self { speed, scale }))
    }
}

impl FrameSource for RainbowEffect {
    fn render(&mut self, pixmap: &mut Pixmap, t: f32, _dt: f32) -> Result<(), ()> {
        let width = pixmap.width();
        let height = pixmap.height();
        let offset = t * self.speed;

        // Hue depends only on the column, so compute one row then replicate it.
        let mut row = Vec::with_capacity(width as usize * 4);
        for x in 0..width {
            let hue = (x as f32 / width as f32) * self.scale + offset;
            let (sr, sg, sb) = hue_to_srgb(hue);
            row.push((srgb_to_linear(sr) * 255.0).round() as u8);
            row.push((srgb_to_linear(sg) * 255.0).round() as u8);
            row.push((srgb_to_linear(sb) * 255.0).round() as u8);
            row.push(255);
        }

        let data = pixmap.data_mut();
        for y in 0..height {
            let base = (y * width) as usize * 4;
            data[base..base + row.len()].copy_from_slice(&row);
        }
        Ok(())
    }
}

// ── ScreenSamplerEffect ───────────────────────────────────────────────────────

struct ScreenSamplerEffect {
    monitor_index: usize,
    handle: Option<super::screen_capture::CaptureHandle>,
    capture_failed: bool,
}

impl Effect for ScreenSamplerEffect {
    fn descriptor() -> Animation {
        let monitors = super::screen_capture::list_monitors();
        let options: Vec<String> = monitors.iter().map(|m| m.label.clone()).collect();
        let default_label = options.first().cloned().unwrap_or_default();
        Animation {
            id: "screen_sampler".to_string(),
            name: "Screen Sampler".to_string(),
            params: vec![EffectParamDescriptor {
                id: "monitor".to_string(),
                label: "Monitor".to_string(),
                kind: ParamKind::Enum { options },
                default: EffectParamValue::Str(default_label),
            }],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Option<Box<dyn FrameSource>> {
        let monitors = super::screen_capture::list_monitors();
        let monitor_index = match params.get("monitor") {
            Some(EffectParamValue::Str(label)) => {
                monitors.iter().position(|m| &m.label == label).unwrap_or(0)
            }
            _ => 0,
        };
        Some(Box::new(Self { monitor_index, handle: None, capture_failed: false }))
    }
}

impl FrameSource for ScreenSamplerEffect {
    fn render(&mut self, pixmap: &mut Pixmap, _t: f32, _dt: f32) -> Result<(), ()> {
        if self.handle.is_none() && !self.capture_failed {
            match super::screen_capture::start_capture(self.monitor_index) {
                Ok(h) => self.handle = Some(h),
                Err(e) => {
                    log::error!("Screen capture failed to start: {e}");
                    self.capture_failed = true;
                }
            }
        }

        let Some(frame) = self.handle.as_ref().and_then(|h| h.latest_frame()) else {
            pixmap.fill(tiny_skia::Color::BLACK);
            return Ok(());
        };

        blit_letterboxed(&frame, pixmap);
        Ok(())
    }
}

/// Scales `frame` (sRGB BGRA, stride-padded) to fit inside `pixmap` (linear premultiplied RGBA),
/// centered with black letterbox bars. Uses nearest-neighbor sampling.
/// Conversion from BGRA→linear happens only for the pixels actually sampled (~canvas resolution),
/// not for the full source frame, so this stays fast even at 5K+ source resolutions.
/// Honors `frame.rotation` so portrait monitors appear upright.
fn blit_letterboxed(frame: &super::screen_capture::RawFrame, pixmap: &mut Pixmap) {
    use super::screen_capture::FrameRotation;

    let dst_w = pixmap.width();
    let dst_h = pixmap.height();
    // Logical dimensions are the buffer dimensions after rotation is applied.
    let (logical_w, logical_h) = match frame.rotation {
        FrameRotation::None | FrameRotation::Cw180 => (frame.width, frame.height),
        FrameRotation::Cw90 | FrameRotation::Cw270 => (frame.height, frame.width),
    };

    let scale =
        (dst_w as f32 / logical_w as f32).min(dst_h as f32 / logical_h as f32);
    let scaled_w = ((logical_w as f32 * scale).round() as u32).min(dst_w);
    let scaled_h = ((logical_h as f32 * scale).round() as u32).min(dst_h);
    let off_x = (dst_w - scaled_w) / 2;
    let off_y = (dst_h - scaled_h) / 2;

    pixmap.fill(tiny_skia::Color::BLACK);

    let fw = frame.width;
    let fh = frame.height;
    let data = pixmap.data_mut();
    for dy in 0..scaled_h {
        for dx in 0..scaled_w {
            // Logical (post-rotation) coordinates.
            let lx = ((dx as f32 / scale) as u32).min(logical_w.saturating_sub(1));
            let ly = ((dy as f32 / scale) as u32).min(logical_h.saturating_sub(1));
            // Map logical → buffer coordinates by applying the inverse rotation.
            let (sx, sy) = match frame.rotation {
                FrameRotation::None => (lx, ly),
                FrameRotation::Cw90 => (ly, fh.saturating_sub(1).saturating_sub(lx)),
                FrameRotation::Cw180 => (
                    fw.saturating_sub(1).saturating_sub(lx),
                    fh.saturating_sub(1).saturating_sub(ly),
                ),
                FrameRotation::Cw270 => (fw.saturating_sub(1).saturating_sub(ly), lx),
            };
            // frame.data is raw BGRA; use stride to handle row padding from the compositor.
            let si = (sy * frame.stride + sx * 4) as usize;
            let lin_r = (srgb_to_linear(frame.data[si + 2]) * 255.0).round() as u8; // B G R A → R
            let lin_g = (srgb_to_linear(frame.data[si + 1]) * 255.0).round() as u8; //         G
            let lin_b = (srgb_to_linear(frame.data[si]) * 255.0).round() as u8;     //         B
            let di = ((off_y + dy) * dst_w + (off_x + dx)) as usize * 4;
            data[di] = lin_r;
            data[di + 1] = lin_g;
            data[di + 2] = lin_b;
            data[di + 3] = 255;
        }
    }
}

// ── Registry ──────────────────────────────────────────────────────────────────

pub fn build(id: &str, params: &HashMap<String, EffectParamValue>) -> Option<Box<dyn FrameSource>> {
    match id {
        "static_color" => StaticColorEffect::from_params(params),
        "breathing" => BreathingEffect::from_params(params),
        "rainbow" => RainbowEffect::from_params(params),
        "screen_sampler" => ScreenSamplerEffect::from_params(params),
        _ => {
            log::warn!("Unknown effect id: {id}");
            None
        }
    }
}

pub fn all_descriptors() -> Vec<Animation> {
    vec![
        StaticColorEffect::descriptor(),
        BreathingEffect::descriptor(),
        RainbowEffect::descriptor(),
        ScreenSamplerEffect::descriptor(),
    ]
}

pub fn default_source() -> Box<dyn FrameSource> {
    StaticColorEffect::from_params(&HashMap::new()).unwrap()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn breathing_effect_renders_without_panic() {
        let params = HashMap::new();
        let mut src = BreathingEffect::from_params(&params).unwrap();
        let mut pixmap = Pixmap::new(64, 64).unwrap();
        src.render(&mut pixmap, 0.0, 0.05).unwrap();
        src.render(&mut pixmap, 1.0, 0.05).unwrap();
    }

    // ── RainbowEffect ─────────────────────────────────────────────────────────

    #[test]
    fn hue_to_srgb_anchors_at_primary_colors() {
        assert_eq!(hue_to_srgb(0.0), (255, 0, 0), "hue 0 → red");
        assert_eq!(hue_to_srgb(1.0 / 3.0), (0, 255, 0), "hue 1/3 → green");
        assert_eq!(hue_to_srgb(2.0 / 3.0), (0, 0, 255), "hue 2/3 → blue");
        // Hue wraps: 1.0 maps back to red.
        assert_eq!(hue_to_srgb(1.0), (255, 0, 0), "hue 1.0 wraps to red");
        // Negative hue wraps too.
        assert_eq!(hue_to_srgb(-1.0), (255, 0, 0), "hue -1.0 wraps to red");
    }

    #[test]
    fn rainbow_descriptor_id_and_params() {
        let desc = RainbowEffect::descriptor();
        assert_eq!(desc.id, "rainbow");
        let ids: Vec<&str> = desc.params.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["speed", "scale"]);
    }

    #[test]
    fn rainbow_render_fills_every_pixel_opaque() {
        let mut src = RainbowEffect::from_params(&HashMap::new()).unwrap();
        let mut pixmap = Pixmap::new(32, 16).unwrap();
        src.render(&mut pixmap, 0.0, 0.05).unwrap();
        for chunk in pixmap.data().chunks(4) {
            assert_eq!(chunk[3], 255, "every pixel must be opaque");
        }
    }

    #[test]
    fn rainbow_render_varies_across_columns() {
        let mut src = RainbowEffect::from_params(&HashMap::new()).unwrap();
        let mut pixmap = Pixmap::new(64, 4).unwrap();
        src.render(&mut pixmap, 0.0, 0.05).unwrap();
        let data = pixmap.data();
        let first = &data[0..3];
        let mid = &data[32 * 4..32 * 4 + 3];
        assert_ne!(first, mid, "different columns must hold different hues");
    }

    #[test]
    fn rainbow_render_rows_are_identical() {
        let mut src = RainbowEffect::from_params(&HashMap::new()).unwrap();
        let mut pixmap = Pixmap::new(16, 8).unwrap();
        src.render(&mut pixmap, 0.3, 0.05).unwrap();
        let data = pixmap.data();
        let row_len = 16 * 4;
        let row0 = &data[0..row_len];
        let row5 = &data[5 * row_len..6 * row_len];
        assert_eq!(row0, row5, "rainbow is a horizontal gradient — rows must match");
    }

    // ── ScreenSamplerEffect ───────────────────────────────────────────────────

    #[test]
    fn screen_sampler_descriptor_id_and_has_monitor_param() {
        let desc = ScreenSamplerEffect::descriptor();
        assert_eq!(desc.id, "screen_sampler");
        assert_eq!(desc.params.len(), 1);
        assert_eq!(desc.params[0].id, "monitor");
    }

    #[test]
    fn screen_sampler_from_params_no_params_returns_some() {
        let params = HashMap::new();
        assert!(ScreenSamplerEffect::from_params(&params).is_some());
    }

    #[test]
    fn screen_sampler_render_with_no_handle_fills_black() {
        let params = HashMap::new();
        let mut src = ScreenSamplerEffect::from_params(&params).unwrap();
        let mut pixmap = Pixmap::new(4, 4).unwrap();
        pixmap.fill(tiny_skia::Color::WHITE);
        // No capture started → should fill black and succeed.
        src.render(&mut pixmap, 0.0, 0.0).unwrap();
        // fill(BLACK) sets RGBA=(0,0,0,255); check only the RGB channels.
        for chunk in pixmap.data().chunks(4) {
            assert_eq!((chunk[0], chunk[1], chunk[2]), (0, 0, 0), "RGB channels should be black");
        }
    }

    // ── blit_letterboxed ─────────────────────────────────────────────────────

    fn solid_frame(w: u32, h: u32, b: u8, g: u8, r: u8) -> crate::engines::canvas::screen_capture::RawFrame {
        use crate::engines::canvas::screen_capture::FrameRotation;
        let stride = w * 4;
        let mut data = vec![0u8; (stride * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = (y * stride + x * 4) as usize;
                data[i] = b;
                data[i + 1] = g;
                data[i + 2] = r;
                data[i + 3] = 255;
            }
        }
        crate::engines::canvas::screen_capture::RawFrame {
            width: w,
            height: h,
            stride,
            data: Arc::new(data),
            rotation: FrameRotation::None,
        }
    }

    /// Build a frame with a single white pixel at `(white_x, white_y)`, all
    /// other pixels black. Lets rotation tests assert where the marker lands.
    fn frame_with_marker(
        w: u32,
        h: u32,
        white_x: u32,
        white_y: u32,
        rotation: crate::engines::canvas::screen_capture::FrameRotation,
    ) -> crate::engines::canvas::screen_capture::RawFrame {
        let stride = w * 4;
        let mut data = vec![0u8; (stride * h) as usize];
        let i = (white_y * stride + white_x * 4) as usize;
        data[i] = 255;
        data[i + 1] = 255;
        data[i + 2] = 255;
        data[i + 3] = 255;
        crate::engines::canvas::screen_capture::RawFrame {
            width: w,
            height: h,
            stride,
            data: Arc::new(data),
            rotation,
        }
    }

    /// Find the (x, y) of the (first) white pixel in the pixmap. Returns None
    /// if no fully-white pixel exists. Tests use this to read back where the
    /// marker pixel from `frame_with_marker` ended up after the blit.
    fn locate_white(pixmap: &Pixmap) -> Option<(u32, u32)> {
        let w = pixmap.width();
        let h = pixmap.height();
        let data = pixmap.data();
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                if data[i] == 255 && data[i + 1] == 255 && data[i + 2] == 255 {
                    return Some((x, y));
                }
            }
        }
        None
    }

    #[test]
    fn blit_letterboxed_same_aspect_fills_entire_pixmap() {
        // 2×2 src → 2×2 dst: no black bars, all pixels written.
        let frame = solid_frame(2, 2, 0, 0, 0);
        let mut pixmap = Pixmap::new(2, 2).unwrap();
        pixmap.fill(tiny_skia::Color::WHITE);
        blit_letterboxed(&frame, &mut pixmap);
        let px = pixmap.data();
        // Alpha channel should be 255 everywhere (not left as 0 from fill=black then overwritten).
        for i in (3..px.len()).step_by(4) {
            assert_eq!(px[i], 255, "pixel at alpha channel was not 255");
        }
    }

    #[test]
    fn blit_letterboxed_wide_src_leaves_top_and_bottom_rows_black() {
        // src 8×2, dst 4×4: scale=0.5 → scaled 4×1, off_y=1
        // rows 0 and 2+ are never written → fill(BLACK) leaves them as RGB=0.
        let frame = solid_frame(8, 2, 0, 0, 255); // sRGB red in BGRA
        let mut pixmap = Pixmap::new(4, 4).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        let px = pixmap.data();
        let row_rgb = |r: u32| -> Vec<(u8, u8, u8)> {
            px[(r * 4 * 4) as usize..(r * 4 * 4 + 4 * 4) as usize]
                .chunks(4)
                .map(|c| (c[0], c[1], c[2]))
                .collect()
        };
        for &row in &[0u32, 2, 3] {
            for (r, g, b) in row_rgb(row) {
                assert_eq!((r, g, b), (0, 0, 0), "row {row} should be a black bar");
            }
        }
    }

    #[test]
    fn blit_letterboxed_tall_src_leaves_left_and_right_cols_black() {
        // src 2×8, dst 4×4: scale=0.5 → scaled 1×4, off_x=1
        // columns 0 and 2+ are never written → fill(BLACK) leaves them as RGB=0.
        let frame = solid_frame(2, 8, 255, 0, 0); // sRGB blue in BGRA
        let mut pixmap = Pixmap::new(4, 4).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        let px = pixmap.data();
        for row in 0..4u32 {
            let base = (row * 4 * 4) as usize;
            let col0 = (px[base], px[base + 1], px[base + 2]);
            let col2 = (px[base + 8], px[base + 9], px[base + 10]);
            let col3 = (px[base + 12], px[base + 13], px[base + 14]);
            assert_eq!(col0, (0, 0, 0), "col 0, row {row} should be black");
            assert_eq!(col2, (0, 0, 0), "col 2, row {row} should be black");
            assert_eq!(col3, (0, 0, 0), "col 3, row {row} should be black");
        }
    }

    #[test]
    fn blit_letterboxed_converts_bgra_to_rgba_channel_order() {
        // Frame has B=0, G=0, R=255, A=any → pixmap pixel should have R=255, G=0, B=0, A=255.
        // srgb_to_linear(255)*255 rounds to 255; srgb_to_linear(0)*255 = 0.
        let frame = solid_frame(1, 1, 0, 0, 255); // BGRA: B=0, G=0, R=255
        let mut pixmap = Pixmap::new(1, 1).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        let px = pixmap.data();
        assert_eq!(px[0], 255, "R channel"); // R
        assert_eq!(px[1], 0,   "G channel"); // G
        assert_eq!(px[2], 0,   "B channel"); // B
        assert_eq!(px[3], 255, "A channel"); // A
    }

    // ── blit_letterboxed rotation handling ───────────────────────────────────
    //
    // DXGI hands back the captured buffer in the source surface's natural
    // (landscape) pixel layout regardless of the user's display rotation. The
    // `FrameRotation` on `RawFrame` tells the blit how to interpret that
    // buffer. Each test puts a single white marker pixel at (0, 0) of a 2×2
    // source and verifies the marker lands where a viewer of the rotated
    // monitor would expect to see it.
    //
    //   natural buffer        Cw90              Cw180             Cw270
    //   ┌─────┐               ┌─────┐           ┌─────┐           ┌─────┐
    //   │W . .│               │. W .│           │. . .│           │. . .│
    //   │. . .│               │. . .│           │. . W│           │W . .│
    //   └─────┘               └─────┘           └─────┘           └─────┘

    #[test]
    fn blit_letterboxed_rotation_none_preserves_position() {
        use crate::engines::canvas::screen_capture::FrameRotation;
        let frame = frame_with_marker(2, 2, 0, 0, FrameRotation::None);
        let mut pixmap = Pixmap::new(2, 2).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        assert_eq!(locate_white(&pixmap), Some((0, 0)));
    }

    #[test]
    fn blit_letterboxed_rotation_cw90_moves_top_left_to_top_right() {
        use crate::engines::canvas::screen_capture::FrameRotation;
        let frame = frame_with_marker(2, 2, 0, 0, FrameRotation::Cw90);
        let mut pixmap = Pixmap::new(2, 2).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        assert_eq!(locate_white(&pixmap), Some((1, 0)));
    }

    #[test]
    fn blit_letterboxed_rotation_cw180_moves_top_left_to_bottom_right() {
        use crate::engines::canvas::screen_capture::FrameRotation;
        let frame = frame_with_marker(2, 2, 0, 0, FrameRotation::Cw180);
        let mut pixmap = Pixmap::new(2, 2).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        assert_eq!(locate_white(&pixmap), Some((1, 1)));
    }

    #[test]
    fn blit_letterboxed_rotation_cw270_moves_top_left_to_bottom_left() {
        use crate::engines::canvas::screen_capture::FrameRotation;
        let frame = frame_with_marker(2, 2, 0, 0, FrameRotation::Cw270);
        let mut pixmap = Pixmap::new(2, 2).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        assert_eq!(locate_white(&pixmap), Some((0, 1)));
    }

    #[test]
    fn blit_letterboxed_rotation_cw90_swaps_logical_dimensions() {
        // 4×2 landscape buffer + Cw90 → logical 2×4 portrait. A 2×4 pixmap
        // matches that aspect ratio exactly, so there are no letterbox bars
        // and we can verify the full rotated layout.
        use crate::engines::canvas::screen_capture::FrameRotation;
        let frame = frame_with_marker(4, 2, 0, 0, FrameRotation::Cw90);
        let mut pixmap = Pixmap::new(2, 4).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        // 4 wide × 2 tall, marker at (0,0), rotated 90° CW → marker at (1, 0)
        // in the 2 wide × 4 tall logical image.
        assert_eq!(locate_white(&pixmap), Some((1, 0)));
    }

    #[test]
    fn blit_letterboxed_rotation_does_not_panic_on_zero_marker() {
        // Letterbox + rotation must not index out of bounds when the source
        // dimensions are smaller than the destination.
        use crate::engines::canvas::screen_capture::FrameRotation;
        for rot in [
            FrameRotation::None,
            FrameRotation::Cw90,
            FrameRotation::Cw180,
            FrameRotation::Cw270,
        ] {
            let frame = frame_with_marker(3, 2, 1, 0, rot);
            let mut pixmap = Pixmap::new(8, 8).unwrap();
            blit_letterboxed(&frame, &mut pixmap);
        }
    }
}
