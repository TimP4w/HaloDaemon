// SPDX-License-Identifier: GPL-3.0-or-later
//! Built-in pixmap effects are reserved for daemon-owned host capabilities.
//!
//! `screen_sampler` needs platform capture handles that the Lua effect sandbox
//! deliberately cannot access. The hidden designer pixmap shares the daemon's
//! typed designer model and must remain available without an installed plugin.
//! Portable visual effects belong in the official effect plugin, not here.

use std::collections::HashMap;

use halod_shared::effect_designer::{self, DesignerParams};
use halod_shared::types::{Animation, EffectParamDescriptor, EffectParamValue, ParamKind};

use super::super::color::srgb_to_linear;
use tiny_skia::Pixmap;

pub trait FrameSource: Send {
    fn render(&mut self, pixmap: &mut Pixmap, t: f32, dt: f32);
}

struct ScreenSamplerEffect {
    handle: Option<super::screen_capture::CaptureHandle>,
}

impl ScreenSamplerEffect {
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

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Box<dyn FrameSource> {
        let monitors = super::screen_capture::list_monitors();
        let monitor_index = match params.get("monitor") {
            Some(EffectParamValue::Str(label)) => monitors
                .iter()
                .position(|m| &m.label == label)
                .unwrap_or_else(|| {
                    log::warn!(
                        "screen_sampler: monitor '{label}' not found, falling back to monitor 0"
                    );
                    0
                }),
            _ => 0,
        };
        let handle = match super::screen_capture::start_capture(monitor_index) {
            Ok(h) => Some(h),
            Err(e) => {
                log::error!("screen_sampler: capture failed to start: {e}");
                None
            }
        };
        Box::new(Self { handle })
    }
}

impl FrameSource for ScreenSamplerEffect {
    fn render(&mut self, pixmap: &mut Pixmap, _t: f32, _dt: f32) {
        let Some(frame) = self.handle.as_mut().and_then(|h| h.latest_frame()) else {
            pixmap.fill(tiny_skia::Color::BLACK);
            return;
        };

        blit_letterboxed(&frame, pixmap);
    }
}

// Converts sRGB to linear only on sampled pixels (not the whole source frame)
// for perf at 5K+ resolutions, and honours frame.rotation.
fn blit_letterboxed(frame: &super::screen_capture::RawFrame, pixmap: &mut Pixmap) {
    use super::screen_capture::FrameRotation;

    pixmap.fill(tiny_skia::Color::BLACK);
    let valid_stride = frame
        .width
        .checked_mul(4)
        .is_some_and(|packed| frame.stride >= packed);
    let valid_len = frame
        .stride
        .checked_mul(frame.height)
        .is_some_and(|bytes| frame.data.len() >= bytes as usize);
    if frame.width == 0 || frame.height == 0 || !valid_stride || !valid_len {
        return;
    }

    let dst_w = pixmap.width();
    let dst_h = pixmap.height();
    let (logical_w, logical_h) = match frame.rotation {
        FrameRotation::None | FrameRotation::Cw180 => (frame.width, frame.height),
        FrameRotation::Cw90 | FrameRotation::Cw270 => (frame.height, frame.width),
    };

    let scale = (dst_w as f32 / logical_w as f32).min(dst_h as f32 / logical_h as f32);
    let scaled_w = ((logical_w as f32 * scale).round() as u32).min(dst_w);
    let scaled_h = ((logical_h as f32 * scale).round() as u32).min(dst_h);
    let off_x = (dst_w - scaled_w) / 2;
    let off_y = (dst_h - scaled_h) / 2;

    let fw = frame.width;
    let fh = frame.height;
    let data = pixmap.data_mut();
    for dy in 0..scaled_h {
        for dx in 0..scaled_w {
            let lx = ((dx as f32 / scale) as u32).min(logical_w.saturating_sub(1));
            let ly = ((dy as f32 / scale) as u32).min(logical_h.saturating_sub(1));
            let (sx, sy) = match frame.rotation {
                FrameRotation::None => (lx, ly),
                FrameRotation::Cw90 => (ly, fh.saturating_sub(1).saturating_sub(lx)),
                FrameRotation::Cw180 => (
                    fw.saturating_sub(1).saturating_sub(lx),
                    fh.saturating_sub(1).saturating_sub(ly),
                ),
                FrameRotation::Cw270 => (fw.saturating_sub(1).saturating_sub(ly), lx),
            };
            // frame.data is raw BGRA; use stride, not width * 4, to skip
            // compositor row padding.
            let si = (sy * frame.stride + sx * 4) as usize;
            let lin_r = (srgb_to_linear(frame.data[si + 2]) * 255.0).round() as u8;
            let lin_g = (srgb_to_linear(frame.data[si + 1]) * 255.0).round() as u8;
            let lin_b = (srgb_to_linear(frame.data[si]) * 255.0).round() as u8;
            let di = ((off_y + dy) * dst_w + (off_x + dx)) as usize * 4;
            data[di] = lin_r;
            data[di + 1] = lin_g;
            data[di + 2] = lin_b;
            data[di + 3] = 255;
        }
    }
}

/// Renders `DesignerParams` math across the canvas pixmap (not per-LED chain).
struct DesignerPixmapEffect {
    params: DesignerParams,
}

impl DesignerPixmapEffect {
    fn from_params(params: &HashMap<String, EffectParamValue>) -> Box<dyn FrameSource> {
        Box::new(Self {
            params: DesignerParams::from_params(params),
        })
    }

    fn pixel(&self, p: f32, ny: f32, t: f32) -> (u8, u8, u8) {
        let (r, g, b) = self.params.color(p, p, ny, t);
        let lin = |c: f32| {
            (srgb_to_linear((c.clamp(0.0, 1.0) * 255.0).round() as u8) * 255.0).round() as u8
        };
        (lin(r), lin(g), lin(b))
    }
}

impl FrameSource for DesignerPixmapEffect {
    fn render(&mut self, pixmap: &mut Pixmap, t: f32, _dt: f32) {
        let width = pixmap.width();
        let height = pixmap.height();
        let last_x = width.saturating_sub(1).max(1) as f32;
        let last_y = height.saturating_sub(1).max(1) as f32;
        let data = pixmap.data_mut();

        if self.params.generator == effect_designer::Generator::Twinkle {
            for y in 0..height {
                let ny = y as f32 / last_y;
                for x in 0..width {
                    let p = x as f32 / last_x;
                    let (r, g, b) = self.pixel(p, ny, t);
                    let idx = ((y * width + x) as usize) * 4;
                    data[idx] = r;
                    data[idx + 1] = g;
                    data[idx + 2] = b;
                    data[idx + 3] = 255;
                }
            }
        } else {
            for x in 0..width {
                let p = x as f32 / last_x;
                let (r, g, b) = self.pixel(p, 0.5, t);
                for y in 0..height {
                    let idx = ((y * width + x) as usize) * 4;
                    data[idx] = r;
                    data[idx + 1] = g;
                    data[idx + 2] = b;
                    data[idx + 3] = 255;
                }
            }
        }
    }
}

pub fn build_builtin(
    id: &str,
    params: &HashMap<String, EffectParamValue>,
) -> Option<Box<dyn FrameSource>> {
    match id {
        "screen_sampler" => Some(ScreenSamplerEffect::from_params(params)),
        effect_designer::DESIGNER_PIXMAP_EFFECT_ID => {
            Some(DesignerPixmapEffect::from_params(params))
        }
        _ => {
            log::warn!("Unknown pixmap effect id: {id}");
            None
        }
    }
}

pub fn builtin_descriptors() -> Vec<Animation> {
    vec![ScreenSamplerEffect::descriptor()]
}

#[cfg(test)]
mod tests {
    use super::super::screen_capture::{FrameRotation, RawFrame};
    use super::*;
    use std::sync::Arc;

    #[test]
    fn builtin_builder_dispatches_every_known_effect_id() {
        for id in ["screen_sampler", effect_designer::DESIGNER_PIXMAP_EFFECT_ID] {
            assert!(
                build_builtin(id, &HashMap::new()).is_some(),
                "{id} must build"
            );
        }
        assert!(
            build_builtin("does_not_exist", &HashMap::new()).is_none(),
            "unknown id must yield None"
        );
    }

    #[test]
    fn designer_pixmap_is_buildable_but_hidden_from_descriptors() {
        assert!(
            build_builtin(effect_designer::DESIGNER_PIXMAP_EFFECT_ID, &HashMap::new()).is_some()
        );
        assert!(!builtin_descriptors()
            .into_iter()
            .any(|d| d.id == effect_designer::DESIGNER_PIXMAP_EFFECT_ID));
    }

    #[test]
    fn designer_pixmap_column_matches_shared_designer_math() {
        let mut params = HashMap::new();
        params.insert(
            "generator".to_string(),
            EffectParamValue::Str("sawtooth".to_string()),
        );
        let dp = DesignerParams::from_params(&params);
        let mut src = DesignerPixmapEffect::from_params(&params);
        let mut pixmap = Pixmap::new(16, 4).unwrap();
        src.render(&mut pixmap, 0.7, 0.05);

        let x = 5u32;
        let p = x as f32 / 15.0;
        let (r, g, b) = dp.color(p, p, 0.5, 0.7);
        let lin = |c: f32| {
            (srgb_to_linear((c.clamp(0.0, 1.0) * 255.0).round() as u8) * 255.0).round() as u8
        };
        let idx = x as usize * 4;
        let data = pixmap.data();
        assert_eq!(data[idx], lin(r));
        assert_eq!(data[idx + 1], lin(g));
        assert_eq!(data[idx + 2], lin(b));
    }

    #[test]
    fn builtin_descriptors_list_screen_sampler_only() {
        let ids: Vec<String> = builtin_descriptors().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["screen_sampler".to_string()]);
    }

    #[test]
    fn screen_sampler_descriptor_id_and_has_monitor_param() {
        let desc = ScreenSamplerEffect::descriptor();
        assert_eq!(desc.id, "screen_sampler");
        assert_eq!(desc.params.len(), 1);
        assert_eq!(desc.params[0].id, "monitor");
    }

    #[test]
    fn screen_sampler_from_params_no_params_builds() {
        let params = HashMap::new();
        let _ = ScreenSamplerEffect::from_params(&params);
    }

    #[test]
    fn screen_sampler_render_with_no_handle_fills_black() {
        let params = HashMap::new();
        let mut src = ScreenSamplerEffect::from_params(&params);
        let mut pixmap = Pixmap::new(4, 4).unwrap();
        pixmap.fill(tiny_skia::Color::WHITE);
        src.render(&mut pixmap, 0.0, 0.0);
        for chunk in pixmap.data().chunks(4) {
            assert_eq!(
                (chunk[0], chunk[1], chunk[2]),
                (0, 0, 0),
                "RGB channels should be black"
            );
        }
    }

    fn solid_frame(w: u32, h: u32, b: u8, g: u8, r: u8) -> RawFrame {
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
        RawFrame {
            width: w,
            height: h,
            stride,
            data: Arc::new(data),
            rotation: FrameRotation::None,
        }
    }

    fn frame_with_marker(
        w: u32,
        h: u32,
        white_x: u32,
        white_y: u32,
        rotation: FrameRotation,
    ) -> RawFrame {
        let stride = w * 4;
        let mut data = vec![0u8; (stride * h) as usize];
        let i = (white_y * stride + white_x * 4) as usize;
        data[i] = 255;
        data[i + 1] = 255;
        data[i + 2] = 255;
        data[i + 3] = 255;
        RawFrame {
            width: w,
            height: h,
            stride,
            data: Arc::new(data),
            rotation,
        }
    }

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
        let frame = solid_frame(2, 2, 0, 0, 0);
        let mut pixmap = Pixmap::new(2, 2).unwrap();
        pixmap.fill(tiny_skia::Color::WHITE);
        blit_letterboxed(&frame, &mut pixmap);
        let px = pixmap.data();
        for i in (3..px.len()).step_by(4) {
            assert_eq!(px[i], 255, "pixel at alpha channel was not 255");
        }
    }

    #[test]
    fn blit_letterboxed_wide_src_leaves_top_and_bottom_rows_black() {
        let frame = solid_frame(8, 2, 0, 0, 255);
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
        let frame = solid_frame(2, 8, 255, 0, 0);
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
        let frame = solid_frame(1, 1, 0, 0, 255);
        let mut pixmap = Pixmap::new(1, 1).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        let px = pixmap.data();
        assert_eq!(px[0], 255, "R channel");
        assert_eq!(px[1], 0, "G channel");
        assert_eq!(px[2], 0, "B channel");
        assert_eq!(px[3], 255, "A channel");
    }

    #[test]
    fn blit_letterboxed_rotation_none_preserves_position() {
        let frame = frame_with_marker(2, 2, 0, 0, FrameRotation::None);
        let mut pixmap = Pixmap::new(2, 2).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        assert_eq!(locate_white(&pixmap), Some((0, 0)));
    }

    #[test]
    fn blit_letterboxed_rotation_cw90_moves_top_left_to_top_right() {
        let frame = frame_with_marker(2, 2, 0, 0, FrameRotation::Cw90);
        let mut pixmap = Pixmap::new(2, 2).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        assert_eq!(locate_white(&pixmap), Some((1, 0)));
    }

    #[test]
    fn blit_letterboxed_rotation_cw180_moves_top_left_to_bottom_right() {
        let frame = frame_with_marker(2, 2, 0, 0, FrameRotation::Cw180);
        let mut pixmap = Pixmap::new(2, 2).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        assert_eq!(locate_white(&pixmap), Some((1, 1)));
    }

    #[test]
    fn blit_letterboxed_rotation_cw270_moves_top_left_to_bottom_left() {
        let frame = frame_with_marker(2, 2, 0, 0, FrameRotation::Cw270);
        let mut pixmap = Pixmap::new(2, 2).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        assert_eq!(locate_white(&pixmap), Some((0, 1)));
    }

    #[test]
    fn blit_letterboxed_rotation_cw90_swaps_logical_dimensions() {
        let frame = frame_with_marker(4, 2, 0, 0, FrameRotation::Cw90);
        let mut pixmap = Pixmap::new(2, 4).unwrap();
        blit_letterboxed(&frame, &mut pixmap);
        assert_eq!(locate_white(&pixmap), Some((1, 0)));
    }

    #[test]
    fn blit_letterboxed_rotation_does_not_panic_on_zero_marker() {
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
