// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use halod_shared::effect_designer::{self, DesignerParams};
use halod_shared::types::{
    Animation, EffectParamDescriptor, EffectParamValue, ParamKind, RgbColor, DEFAULT_SAMPLE_RADIUS,
};

use super::super::color::{hue_to_srgb, srgb_to_linear};
use crate::services::audio::{self, AudioHandle, SpectrumFrame};
use crate::util::effect_params::{param_bool, param_color, param_f64, param_str};
use tiny_skia::{Color, Pixmap};

pub trait FrameSource: Send {
    fn render(&mut self, pixmap: &mut Pixmap, t: f32, dt: f32);
}

trait Effect: Send {
    fn descriptor() -> Animation
    where
        Self: Sized;
    fn from_params(params: &HashMap<String, EffectParamValue>) -> Option<Box<dyn FrameSource>>
    where
        Self: Sized;
}

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
        let color = param_color(params, "color", RgbColor { r: 255, g: 0, b: 0 });
        Some(Box::new(Self { color }))
    }
}

impl FrameSource for StaticColorEffect {
    fn render(&mut self, pixmap: &mut Pixmap, _t: f32, _dt: f32) {
        let r = (srgb_to_linear(self.color.r) * 255.0).round() as u8;
        let g = (srgb_to_linear(self.color.g) * 255.0).round() as u8;
        let b = (srgb_to_linear(self.color.b) * 255.0).round() as u8;
        pixmap.fill(Color::from_rgba8(r, g, b, 255));
    }
}

struct RainbowEffect {
    speed: f32,
    scale: f32,
}

impl Effect for RainbowEffect {
    fn descriptor() -> Animation {
        Animation {
            id: "rainbow".to_string(),
            name: "Rainbow Wave".to_string(),
            params: vec![
                EffectParamDescriptor {
                    id: "speed".to_string(),
                    label: "Speed".to_string(),
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 2.0,
                        step: 0.05,
                    },
                    default: EffectParamValue::Float(0.2),
                },
                EffectParamDescriptor {
                    id: "scale".to_string(),
                    label: "Scale".to_string(),
                    kind: ParamKind::Range {
                        min: 0.5,
                        max: 5.0,
                        step: 0.5,
                    },
                    default: EffectParamValue::Float(1.0),
                },
            ],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Option<Box<dyn FrameSource>> {
        let speed = param_f64(params, "speed", 0.2) as f32;
        let scale = param_f64(params, "scale", 1.0) as f32;
        Some(Box::new(Self { speed, scale }))
    }
}

impl FrameSource for RainbowEffect {
    fn render(&mut self, pixmap: &mut Pixmap, t: f32, _dt: f32) {
        let width = pixmap.width();
        let height = pixmap.height();
        let offset = t * self.speed;

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
    }
}

struct ScreenSamplerEffect {
    handle: Option<super::screen_capture::CaptureHandle>,
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
        Some(Box::new(Self { handle }))
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

    pixmap.fill(tiny_skia::Color::BLACK);

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

/// Source of the current `SpectrumFrame`: the live shared capture handle in
/// production, or a fixed frame injected directly in tests (bypassing the
/// live audio_capture singleton so renders are deterministic).
enum FrameProvider {
    Live(Arc<AudioHandle>),
    #[cfg(test)]
    Fixed(Box<SpectrumFrame>),
}

impl FrameProvider {
    fn latest(&self) -> SpectrumFrame {
        match self {
            Self::Live(handle) => handle.latest(),
            #[cfg(test)]
            Self::Fixed(frame) => **frame,
        }
    }
}

struct AudioSpectrumEffect {
    frames: FrameProvider,
    color_low: RgbColor,
    color_high: RgbColor,
    bars: bool,
}

impl Effect for AudioSpectrumEffect {
    fn descriptor() -> Animation {
        Animation {
            id: "audio_spectrum".to_string(),
            name: "Audio Spectrum".to_string(),
            params: vec![
                EffectParamDescriptor {
                    id: "color_low".to_string(),
                    label: "Low Color".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(RgbColor {
                        r: 0,
                        g: 120,
                        b: 255,
                    }),
                },
                EffectParamDescriptor {
                    id: "color_high".to_string(),
                    label: "High Color".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(RgbColor {
                        r: 255,
                        g: 0,
                        b: 120,
                    }),
                },
                EffectParamDescriptor {
                    id: "fill".to_string(),
                    label: "Fill".to_string(),
                    kind: ParamKind::Enum {
                        options: vec!["bars".to_string(), "solid".to_string()],
                    },
                    default: EffectParamValue::Str("bars".to_string()),
                },
            ],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Option<Box<dyn FrameSource>> {
        let color_low = param_color(
            params,
            "color_low",
            RgbColor {
                r: 0,
                g: 120,
                b: 255,
            },
        );
        let color_high = param_color(
            params,
            "color_high",
            RgbColor {
                r: 255,
                g: 0,
                b: 120,
            },
        );
        let bars = param_str(params, "fill", "bars") != "solid";
        Some(Box::new(Self {
            frames: FrameProvider::Live(audio::shared()),
            color_low,
            color_high,
            bars,
        }))
    }
}

#[cfg(test)]
impl AudioSpectrumEffect {
    fn with_frame(
        color_low: RgbColor,
        color_high: RgbColor,
        bars: bool,
        frame: SpectrumFrame,
    ) -> Self {
        Self {
            frames: FrameProvider::Fixed(Box::new(frame)),
            color_low,
            color_high,
            bars,
        }
    }
}

fn lerp_color(low: RgbColor, high: RgbColor, t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let lerp_channel = |a: u8, b: u8| -> u8 {
        let av = srgb_to_linear(a);
        let bv = srgb_to_linear(b);
        ((av + (bv - av) * t) * 255.0).round() as u8
    };
    (
        lerp_channel(low.r, high.r),
        lerp_channel(low.g, high.g),
        lerp_channel(low.b, high.b),
    )
}

impl FrameSource for AudioSpectrumEffect {
    fn render(&mut self, pixmap: &mut Pixmap, _t: f32, _dt: f32) {
        let frame = self.frames.latest();
        let width = pixmap.width();
        let height = pixmap.height();
        let bands = audio::BANDS;

        pixmap.fill(Color::BLACK);
        let data = pixmap.data_mut();

        for i in 0..bands {
            let x0 = (i as u32 * width) / bands as u32;
            let x1 = ((i as u32 + 1) * width) / bands as u32;
            if x0 >= x1 {
                continue;
            }
            let x_end = if self.bars && x1 - x0 >= 2 {
                x1 - 1
            } else {
                x1
            };

            let t = i as f32 / (bands - 1) as f32;
            let (r, g, b) = lerp_color(self.color_low, self.color_high, t);

            let amp = frame.bands[i].clamp(0.0, 1.0);
            let bar_h = (amp * height as f32).round() as u32;
            let y_start = height.saturating_sub(bar_h);

            for y in y_start..height {
                let row = (y * width) as usize * 4;
                for x in x0..x_end {
                    let px = row + x as usize * 4;
                    data[px] = r;
                    data[px + 1] = g;
                    data[px + 2] = b;
                    data[px + 3] = 255;
                }
            }
        }
    }
}

struct AudioWaveformEffect {
    frames: FrameProvider,
    color: RgbColor,
    thickness: f32,
    history: VecDeque<f32>,
    last_seq: Option<u64>,
}

impl Effect for AudioWaveformEffect {
    fn descriptor() -> Animation {
        Animation {
            id: "audio_waveform".to_string(),
            name: "Audio Waveform".to_string(),
            params: vec![
                EffectParamDescriptor {
                    id: "color".to_string(),
                    label: "Color".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(RgbColor {
                        r: 0,
                        g: 255,
                        b: 180,
                    }),
                },
                EffectParamDescriptor {
                    id: "thickness".to_string(),
                    label: "Thickness".to_string(),
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 8.0,
                        step: 1.0,
                    },
                    default: EffectParamValue::Float(2.0),
                },
            ],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Option<Box<dyn FrameSource>> {
        let color = param_color(
            params,
            "color",
            RgbColor {
                r: 0,
                g: 255,
                b: 180,
            },
        );
        let thickness = param_f64(params, "thickness", 2.0) as f32;
        Some(Box::new(Self {
            frames: FrameProvider::Live(audio::shared()),
            color,
            thickness,
            history: VecDeque::new(),
            last_seq: None,
        }))
    }
}

#[cfg(test)]
impl AudioWaveformEffect {
    fn with_frame(color: RgbColor, thickness: f32, frame: SpectrumFrame) -> Self {
        Self {
            frames: FrameProvider::Fixed(Box::new(frame)),
            color,
            thickness,
            history: VecDeque::new(),
            last_seq: None,
        }
    }
}

impl AudioWaveformEffect {
    /// Peak magnitude of the waveform window, signed by the sample at the
    /// window midpoint. Only samples on a new DSP hop (`seq` changed).
    fn push_sample(&mut self, frame: &SpectrumFrame, capacity: usize) {
        if self.last_seq == Some(frame.seq) {
            return;
        }
        self.last_seq = Some(frame.seq);

        let peak = frame
            .waveform
            .iter()
            .cloned()
            .fold(0.0f32, |acc, s| acc.max(s.abs()));
        let mid = frame.waveform[frame.waveform.len() / 2];
        let value = if mid < 0.0 { -peak } else { peak };

        self.history.push_back(value);
        while self.history.len() > capacity {
            self.history.pop_front();
        }
    }
}

impl FrameSource for AudioWaveformEffect {
    fn render(&mut self, pixmap: &mut Pixmap, _t: f32, _dt: f32) {
        let width = pixmap.width() as usize;
        let height = pixmap.height();

        let frame = self.frames.latest();
        self.push_sample(&frame, width);

        pixmap.fill(Color::BLACK);
        let data = pixmap.data_mut();

        let (r, g, b) = {
            let lin = |c: u8| (srgb_to_linear(c) * 255.0).round() as u8;
            (lin(self.color.r), lin(self.color.g), lin(self.color.b))
        };

        let half_h = height as f32 / 2.0;
        let start_x = width.saturating_sub(self.history.len());
        for (col, &v) in self.history.iter().enumerate() {
            let x = start_x + col;
            if x >= width {
                continue;
            }
            let half_len = v.abs().clamp(0.0, 1.0) * half_h;
            let y0 = (half_h - half_len).round().max(0.0) as u32;
            let y1 = (half_h + half_len).round().min(height as f32) as u32;
            let thick = self.thickness.max(1.0).round() as i64;
            let half_thick = thick / 2;
            let cx = x as i64;
            for dx in -half_thick..(thick - half_thick) {
                let px_x = cx + dx;
                if px_x < 0 || px_x as usize >= width {
                    continue;
                }
                for y in y0..y1.max(y0 + 1).min(height) {
                    let idx = (y as usize * width + px_x as usize) * 4;
                    data[idx] = r;
                    data[idx + 1] = g;
                    data[idx + 2] = b;
                    data[idx + 3] = 255;
                }
            }
        }
    }
}

struct RandomFlashEffect {
    cells: usize,
    interval: f32,
    decay: f32,
    random_color: bool,
    color: RgbColor,
}

/// Largest prime below 2^24 (f32's exact-integer ceiling).
const EPOCH_HASH_PRIME: u64 = 16_777_213;

fn random_flash_hash(seed: f32) -> f32 {
    let s = (seed * 12.9898).sin() * 43_758.547;
    s - s.floor()
}

fn epoch_seed(epoch: u64) -> f32 {
    (epoch % EPOCH_HASH_PRIME) as f32
}

fn random_flash_pick(seed: f32, cells: usize, prev: Option<usize>) -> usize {
    if cells <= 1 {
        return 0;
    }
    let idx = (random_flash_hash(seed) * cells as f32) as usize % cells;
    if Some(idx) == prev {
        (idx + 1) % cells
    } else {
        idx
    }
}

fn random_flash_color(seed: f32) -> RgbColor {
    let hue = random_flash_hash(seed);
    let (r, g, b) = hue_to_srgb(hue);
    RgbColor { r, g, b }
}

fn random_flash_pick_for_epoch(epoch: u64, cells: usize) -> usize {
    let prev = (epoch > 0).then(|| random_flash_pick(epoch_seed(epoch - 1), cells, None));
    random_flash_pick(epoch_seed(epoch), cells, prev)
}

impl RandomFlashEffect {
    fn with_settings(
        cells: usize,
        interval: f32,
        decay: f32,
        random_color: bool,
        color: RgbColor,
    ) -> Self {
        Self {
            cells,
            interval,
            decay,
            random_color,
            color,
        }
    }
}

impl Effect for RandomFlashEffect {
    fn descriptor() -> Animation {
        Animation {
            id: "random_flash".to_string(),
            name: "Random Flash".to_string(),
            params: vec![
                EffectParamDescriptor {
                    id: "cells".to_string(),
                    label: "Cells".to_string(),
                    kind: ParamKind::Range {
                        min: 2.0,
                        max: 8.0,
                        step: 1.0,
                    },
                    default: EffectParamValue::Float(4.0),
                },
                EffectParamDescriptor {
                    id: "interval".to_string(),
                    label: "Interval (s)".to_string(),
                    kind: ParamKind::Range {
                        min: 0.2,
                        max: 5.0,
                        step: 0.1,
                    },
                    default: EffectParamValue::Float(1.0),
                },
                EffectParamDescriptor {
                    id: "decay".to_string(),
                    label: "Decay (s)".to_string(),
                    kind: ParamKind::Range {
                        min: 0.05,
                        max: 3.0,
                        step: 0.05,
                    },
                    default: EffectParamValue::Float(0.6),
                },
                EffectParamDescriptor {
                    id: "random_color".to_string(),
                    label: "Random color per flash".to_string(),
                    kind: ParamKind::Boolean,
                    default: EffectParamValue::Bool(false),
                },
                EffectParamDescriptor {
                    id: "color".to_string(),
                    label: "Color".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(RgbColor {
                        r: 56,
                        g: 189,
                        b: 248,
                    }),
                },
            ],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Option<Box<dyn FrameSource>> {
        let cells = (param_f64(params, "cells", 4.0).round() as usize).clamp(2, 8);
        let interval = (param_f64(params, "interval", 1.0) as f32).clamp(0.2, 5.0);
        let decay = (param_f64(params, "decay", 0.6) as f32).clamp(0.05, 3.0);
        let random_color = param_bool(params, "random_color", false);
        let color = param_color(
            params,
            "color",
            RgbColor {
                r: 56,
                g: 189,
                b: 248,
            },
        );
        Some(Box::new(RandomFlashEffect::with_settings(
            cells,
            interval,
            decay,
            random_color,
            color,
        )))
    }
}

impl FrameSource for RandomFlashEffect {
    fn render(&mut self, pixmap: &mut Pixmap, t: f32, _dt: f32) {
        let epoch = (t.max(0.0) / self.interval).floor() as u64;
        let idx = random_flash_pick_for_epoch(epoch, self.cells);
        let lit_at = epoch as f32 * self.interval;
        let brightness = (-(t - lit_at).max(0.0) / self.decay).exp();
        let c = if self.random_color {
            random_flash_color(epoch_seed(epoch) * 7.0 + 3.1)
        } else {
            self.color
        };
        let lin = |ch: u8| (srgb_to_linear(ch) * brightness * 255.0).round() as u8;
        let (r, g, b) = (lin(c.r), lin(c.g), lin(c.b));

        let width = pixmap.width() as usize;
        let height = pixmap.height() as usize;
        pixmap.fill(Color::BLACK);
        let data = pixmap.data_mut();
        let inset = DEFAULT_SAMPLE_RADIUS.round() as usize;
        let x0_raw = idx * width / self.cells;
        let x1_raw = (idx + 1) * width / self.cells;
        let x0 = (x0_raw + inset).min(x1_raw);
        let x1 = x1_raw.saturating_sub(inset).max(x0);
        for y in 0..height {
            let row = y * width;
            for x in x0..x1 {
                let pi = (row + x) * 4;
                data[pi] = r;
                data[pi + 1] = g;
                data[pi + 2] = b;
                data[pi + 3] = 255;
            }
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

pub fn build(id: &str, params: &HashMap<String, EffectParamValue>) -> Option<Box<dyn FrameSource>> {
    match id {
        "static_color" => StaticColorEffect::from_params(params),
        "rainbow" => RainbowEffect::from_params(params),
        "screen_sampler" => ScreenSamplerEffect::from_params(params),
        "audio_spectrum" => AudioSpectrumEffect::from_params(params),
        "audio_waveform" => AudioWaveformEffect::from_params(params),
        "random_flash" => RandomFlashEffect::from_params(params),
        effect_designer::DESIGNER_PIXMAP_EFFECT_ID => {
            Some(DesignerPixmapEffect::from_params(params))
        }
        _ => {
            log::warn!("Unknown pixmap effect id: {id}");
            None
        }
    }
}

pub fn all_descriptors() -> Vec<Animation> {
    vec![
        StaticColorEffect::descriptor(),
        RainbowEffect::descriptor(),
        ScreenSamplerEffect::descriptor(),
        AudioSpectrumEffect::descriptor(),
        AudioWaveformEffect::descriptor(),
        RandomFlashEffect::descriptor(),
    ]
}

pub fn default_source() -> Box<dyn FrameSource> {
    StaticColorEffect::from_params(&HashMap::new()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::super::screen_capture::{FrameRotation, RawFrame};
    use super::*;
    use std::sync::Arc;

    fn first_pixel_rgb(pixmap: &Pixmap) -> (u8, u8, u8) {
        let d = pixmap.data();
        (d[0], d[1], d[2])
    }

    #[test]
    fn static_color_render_scales_into_midrange() {
        let mut src = StaticColorEffect::from_params(&{
            let mut p = HashMap::new();
            p.insert(
                "color".to_string(),
                EffectParamValue::Color(RgbColor {
                    r: 188,
                    g: 188,
                    b: 188,
                }),
            );
            p
        })
        .unwrap();
        let mut pixmap = Pixmap::new(4, 4).unwrap();
        src.render(&mut pixmap, 0.0, 0.0);
        let (r, g, b) = first_pixel_rgb(&pixmap);
        for c in [r, g, b] {
            assert!((100..=160).contains(&c), "channel {c} should be mid-range");
        }
    }

    #[test]
    fn build_dispatches_every_known_effect_id() {
        for id in [
            "static_color",
            "rainbow",
            "screen_sampler",
            "audio_spectrum",
            "audio_waveform",
            "random_flash",
            effect_designer::DESIGNER_PIXMAP_EFFECT_ID,
        ] {
            assert!(build(id, &HashMap::new()).is_some(), "{id} must build");
        }
        assert!(
            build("does_not_exist", &HashMap::new()).is_none(),
            "unknown id must yield None"
        );
    }

    #[test]
    fn designer_pixmap_is_buildable_but_hidden_from_descriptors() {
        assert!(build(effect_designer::DESIGNER_PIXMAP_EFFECT_ID, &HashMap::new()).is_some());
        assert!(!all_descriptors()
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
    fn empty_params_match_declared_descriptor_defaults() {
        for desc in all_descriptors() {
            if desc.id == "screen_sampler"
                || desc.id == "audio_spectrum"
                || desc.id == "audio_waveform"
            {
                continue;
            }
            let declared: HashMap<String, EffectParamValue> = desc
                .params
                .iter()
                .map(|p| (p.id.clone(), p.default.clone()))
                .collect();
            let mut empty = build(&desc.id, &HashMap::new()).unwrap();
            let mut explicit = build(&desc.id, &declared).unwrap();
            let mut a = Pixmap::new(16, 4).unwrap();
            let mut b = Pixmap::new(16, 4).unwrap();
            empty.render(&mut a, 0.4, 0.05);
            explicit.render(&mut b, 0.4, 0.05);
            assert_eq!(
                a.data(),
                b.data(),
                "{}: empty-param fallback disagrees with declared descriptor default",
                desc.id
            );
        }
    }

    #[test]
    fn all_descriptors_lists_every_effect() {
        let ids: Vec<String> = all_descriptors().into_iter().map(|d| d.id).collect();
        for id in [
            "static_color",
            "rainbow",
            "screen_sampler",
            "audio_spectrum",
            "audio_waveform",
            "random_flash",
        ] {
            assert!(
                ids.contains(&id.to_string()),
                "{id} missing from all_descriptors"
            );
        }
    }

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

    fn spectrum_frame_with_band0(v: f32) -> SpectrumFrame {
        let mut frame = SpectrumFrame::default();
        frame.bands[0] = v;
        frame
    }

    #[test]
    fn audio_spectrum_descriptor_id_and_defaults() {
        let desc = AudioSpectrumEffect::descriptor();
        assert_eq!(desc.id, "audio_spectrum");
        assert_eq!(desc.params.len(), 3);
    }

    #[test]
    fn audio_spectrum_band0_full_colors_bottom_left_like_color_low() {
        let color_low = RgbColor {
            r: 0,
            g: 120,
            b: 255,
        };
        let color_high = RgbColor {
            r: 255,
            g: 0,
            b: 120,
        };
        let frame = spectrum_frame_with_band0(1.0);
        let mut src = AudioSpectrumEffect::with_frame(color_low, color_high, true, frame);
        let mut pixmap = Pixmap::new(64, 8).unwrap();
        src.render(&mut pixmap, 0.0, 0.0);

        let data = pixmap.data();
        let bottom_left = (7 * 64) * 4;
        let (r, g, b) = (
            data[bottom_left],
            data[bottom_left + 1],
            data[bottom_left + 2],
        );
        let expected = lerp_color(color_low, color_high, 0.0);
        assert_eq!((r, g, b), expected);

        let top_right = 63 * 4;
        assert_eq!(
            (data[top_right], data[top_right + 1], data[top_right + 2]),
            (0, 0, 0)
        );
    }

    #[test]
    fn audio_spectrum_all_zero_bands_renders_all_black() {
        let mut src = AudioSpectrumEffect::with_frame(
            RgbColor {
                r: 0,
                g: 120,
                b: 255,
            },
            RgbColor {
                r: 255,
                g: 0,
                b: 120,
            },
            true,
            SpectrumFrame::default(),
        );
        let mut pixmap = Pixmap::new(64, 8).unwrap();
        pixmap.fill(Color::WHITE);
        src.render(&mut pixmap, 0.0, 0.0);
        for chunk in pixmap.data().chunks(4) {
            assert_eq!((chunk[0], chunk[1], chunk[2]), (0, 0, 0));
        }
    }

    proptest::proptest! {
        #[test]
        fn audio_spectrum_alpha_always_opaque_and_within_lerp_bounds(
            bands in proptest::collection::vec(0.0f32..=1.0, 64),
        ) {
            let color_low = RgbColor { r: 10, g: 20, b: 30 };
            let color_high = RgbColor { r: 200, g: 210, b: 220 };
            let mut frame = SpectrumFrame::default();
            frame.bands.copy_from_slice(&bands);
            let mut src = AudioSpectrumEffect::with_frame(color_low, color_high, true, frame);
            let mut pixmap = Pixmap::new(64, 8).unwrap();
            src.render(&mut pixmap, 0.0, 0.0);

            let max_r = color_low.r.max(color_high.r);
            let max_g = color_low.g.max(color_high.g);
            let max_b = color_low.b.max(color_high.b);

            for chunk in pixmap.data().chunks(4) {
                proptest::prop_assert_eq!(chunk[3], 255);
                proptest::prop_assert!(chunk[0] <= max_r.max(1));
                proptest::prop_assert!(chunk[1] <= max_g.max(1));
                proptest::prop_assert!(chunk[2] <= max_b.max(1));
            }
        }
    }

    fn spectrum_frame_with_seq(seq: u64) -> SpectrumFrame {
        let mut frame = SpectrumFrame {
            seq,
            ..SpectrumFrame::default()
        };
        frame.waveform[0] = 0.5;
        frame
    }

    #[test]
    fn audio_waveform_descriptor_id_and_defaults() {
        let desc = AudioWaveformEffect::descriptor();
        assert_eq!(desc.id, "audio_waveform");
        assert_eq!(desc.params.len(), 2);
        assert!(desc.params.iter().all(|p| p.id != "speed"));
    }

    #[test]
    fn audio_waveform_n_distinct_seq_frames_yield_n_nonblack_columns() {
        let color = RgbColor {
            r: 0,
            g: 255,
            b: 180,
        };
        let mut src = AudioWaveformEffect::with_frame(color, 1.0, spectrum_frame_with_seq(1));
        let mut pixmap = Pixmap::new(16, 8).unwrap();

        for seq in 1..=5u64 {
            src.frames = FrameProvider::Fixed(Box::new(spectrum_frame_with_seq(seq)));
            src.render(&mut pixmap, 0.0, 0.0);
        }

        let width = pixmap.width() as usize;
        let mut nonblack_cols = 0;
        for x in 0..width {
            let mut col_nonblack = false;
            for y in 0..pixmap.height() as usize {
                let i = (y * width + x) * 4;
                let d = pixmap.data();
                if (d[i], d[i + 1], d[i + 2]) != (0, 0, 0) {
                    col_nonblack = true;
                }
            }
            if col_nonblack {
                nonblack_cols += 1;
            }
        }
        assert_eq!(nonblack_cols, 5);
    }

    #[test]
    fn audio_waveform_same_seq_twice_does_not_grow_history() {
        let color = RgbColor {
            r: 0,
            g: 255,
            b: 180,
        };
        let mut src = AudioWaveformEffect::with_frame(color, 2.0, spectrum_frame_with_seq(7));
        let mut pixmap = Pixmap::new(16, 8).unwrap();

        src.render(&mut pixmap, 0.0, 0.0);
        assert_eq!(src.history.len(), 1);
        src.render(&mut pixmap, 0.0, 0.0);
        assert_eq!(src.history.len(), 1);
    }

    fn cell_pixel(pixmap: &Pixmap, cells: usize, cell: usize) -> (u8, u8, u8) {
        let width = pixmap.width() as usize;
        let x = cell * width / cells + (width / cells) / 2;
        let d = pixmap.data();
        let idx = x * 4;
        (d[idx], d[idx + 1], d[idx + 2])
    }

    fn test_flash_color() -> RgbColor {
        RgbColor {
            r: 56,
            g: 189,
            b: 248,
        }
    }

    #[test]
    fn random_flash_exactly_one_cell_is_at_peak_brightness_right_after_a_pick() {
        let mut src = RandomFlashEffect::with_settings(4, 1.0, 0.6, false, test_flash_color());
        let mut pixmap = Pixmap::new(80, 4).unwrap();
        src.render(&mut pixmap, 0.0, 0.0);
        let lit: Vec<usize> = (0..4)
            .filter(|&c| cell_pixel(&pixmap, 4, c) != (0, 0, 0))
            .collect();
        assert_eq!(
            lit.len(),
            1,
            "exactly one cell must be lit right after a pick"
        );
    }

    #[test]
    fn random_flash_decays_and_never_repicks_the_same_cell_twice_in_a_row() {
        let mut src = RandomFlashEffect::with_settings(3, 1.0, 0.3, false, test_flash_color());
        let mut pixmap = Pixmap::new(60, 4).unwrap();

        let first = random_flash_pick_for_epoch(0, 3);
        src.render(&mut pixmap, 0.0, 0.0);
        let bright_at_pick = cell_pixel(&pixmap, 3, first);
        assert_ne!(bright_at_pick, (0, 0, 0));

        src.render(&mut pixmap, 0.5, 0.5);
        let decayed = cell_pixel(&pixmap, 3, first);
        assert!(
            decayed.0 <= bright_at_pick.0 && decayed.1 <= bright_at_pick.1,
            "brightness must not increase without a new pick"
        );

        src.render(&mut pixmap, 1.0, 0.5);
        let second = random_flash_pick_for_epoch(1, 3);
        assert_ne!(second, first);
        let now_lit: Vec<usize> = (0..3)
            .filter(|&c| cell_pixel(&pixmap, 3, c) != (0, 0, 0))
            .collect();
        assert_eq!(now_lit, vec![second]);
    }

    #[test]
    fn random_flash_rebuilding_the_instance_survives_mid_pattern() {
        let mut continuous =
            RandomFlashEffect::with_settings(5, 0.4, 0.2, false, test_flash_color());
        let mut rebuilt = RandomFlashEffect::with_settings(5, 0.4, 0.2, false, test_flash_color());
        let mut pixmap_a = Pixmap::new(100, 4).unwrap();
        let mut pixmap_b = Pixmap::new(100, 4).unwrap();

        let mut t = 0.0f32;
        while t < 3.7 {
            continuous.render(&mut pixmap_a, t, 0.05);
            t += 0.05;
        }
        continuous.render(&mut pixmap_a, 3.7, 0.05);
        rebuilt.render(&mut pixmap_b, 3.7, 0.05);

        assert_eq!(pixmap_a.data(), pixmap_b.data());
    }

    #[test]
    fn random_flash_pick_never_repeats_the_previous_index() {
        for prev in 0..5usize {
            for seed in 0..50 {
                let idx = random_flash_pick(seed as f32, 5, Some(prev));
                assert_ne!(idx, prev);
                assert!(idx < 5);
            }
        }
    }

    #[test]
    fn random_flash_single_cell_always_picks_zero() {
        assert_eq!(random_flash_pick(3.7, 1, Some(0)), 0);
        assert_eq!(random_flash_pick(99.0, 1, None), 0);
    }
}
