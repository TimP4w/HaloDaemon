use std::collections::HashMap;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use image::{Rgba, RgbaImage};
use imageproc::drawing::{draw_filled_circle_mut, draw_text_mut};

use halod_protocol::types::{
    EffectParamDescriptor, EffectParamValue, LcdEngineTemplateDescriptor, ParamKind, RgbColor,
    Sensor,
};

static FONT_BYTES: &[u8] = include_bytes!("../../../assets/NotoSans-Regular.ttf");

fn load_font() -> FontRef<'static> {
    FontRef::try_from_slice(FONT_BYTES).expect("bundled font is valid")
}

pub struct TemplateCtx<'a> {
    pub width: u32,
    pub height: u32,
    /// Seconds since the engine started.
    pub t: f64,
    /// Monotonically increasing per-device frame counter.
    pub frame: u64,
    /// Live sensor readings keyed by sensor id.
    pub sensors: &'a HashMap<String, Sensor>,
}

/// An LCD engine template: a configurable, sensor-aware animation.
///
/// Mirrors the canvas engine's `Effect` trait — each template advertises a
/// parameter schema via `descriptor()` and is instantiated from a parameter map
/// via `from_params()`.
pub trait LcdTemplate: Send {
    /// Static schema: id, display name, and the parameter descriptors the UI
    /// renders widgets from.
    fn descriptor() -> LcdEngineTemplateDescriptor
    where
        Self: Sized;

    /// Build a live instance from a parameter map. Missing/mistyped params fall
    /// back to the descriptor defaults.
    fn from_params(params: &HashMap<String, EffectParamValue>) -> Box<dyn LcdTemplate>
    where
        Self: Sized;

    /// Render one frame as a raw RGBA8 image at `ctx.width`×`ctx.height`.
    /// The LCD engine streams this straight to the panel (type-0x08 path) and
    /// encodes a separate PNG copy for the UI preview.
    fn render(&mut self, ctx: &TemplateCtx) -> Result<RgbaImage, String>;
}

// ── Param extraction helpers ──────────────────────────────────────────────────

fn param_color(params: &HashMap<String, EffectParamValue>, id: &str, default: RgbColor) -> RgbColor {
    match params.get(id) {
        Some(EffectParamValue::Color(c)) => *c,
        _ => default,
    }
}

fn param_f64(params: &HashMap<String, EffectParamValue>, id: &str, default: f64) -> f64 {
    match params.get(id) {
        Some(EffectParamValue::Float(f)) => *f,
        _ => default,
    }
}

fn param_str(params: &HashMap<String, EffectParamValue>, id: &str, default: &str) -> String {
    match params.get(id) {
        Some(EffectParamValue::Str(s)) => s.clone(),
        _ => default.to_string(),
    }
}

// ── FrameCounterTemplate ──────────────────────────────────────────────────────

pub struct FrameCounterTemplate {
    font: FontRef<'static>,
}

impl Default for FrameCounterTemplate {
    fn default() -> Self {
        Self { font: load_font() }
    }
}

impl LcdTemplate for FrameCounterTemplate {
    fn descriptor() -> LcdEngineTemplateDescriptor {
        LcdEngineTemplateDescriptor {
            id: "frame_counter".to_string(),
            name: "Frame Counter".to_string(),
            params: vec![],
        }
    }

    fn from_params(_params: &HashMap<String, EffectParamValue>) -> Box<dyn LcdTemplate> {
        Box::new(Self::default())
    }

    fn render(&mut self, ctx: &TemplateCtx) -> Result<RgbaImage, String> {
        let mut img = RgbaImage::new(ctx.width, ctx.height);
        for px in img.pixels_mut() {
            *px = Rgba([0, 0, 0, 255]);
        }

        let text = format!("Frame {}", ctx.frame);
        let scale = PxScale::from(ctx.height as f32 * 0.20);

        let (x, y) = center_text(&self.font, scale, &text, ctx.width, ctx.height);
        draw_text_mut(&mut img, Rgba([255, 255, 255, 255]), x, y, scale, &self.font, &text);

        Ok(img)
    }
}

// ── SingleInfographicTemplate ─────────────────────────────────────────────────

/// A single sensor reading shown as a big number with a label, surrounded by a
/// 270° ring gauge (gap at the bottom) whose dot tracks the value.
pub struct SingleInfographicTemplate {
    font: FontRef<'static>,
    sensor_id: String,
    label: String,
    min: f64,
    max: f64,
    background: RgbColor,
    number: RgbColor,
    text: RgbColor,
    ring: RgbColor,
}

impl SingleInfographicTemplate {
    const BACKGROUND: RgbColor = RgbColor { r: 18, g: 18, b: 22 };
    const NUMBER: RgbColor = RgbColor { r: 255, g: 255, b: 255 };
    const TEXT: RgbColor = RgbColor { r: 170, g: 170, b: 180 };
    const RING: RgbColor = RgbColor { r: 0, g: 200, b: 220 };

    /// Arc start, measured clockwise from 12 o'clock. 225° is the lower-left tip.
    const START_DEG: f32 = 225.0;
    /// Arc sweep — 270° leaves a 90° gap centred on the bottom.
    const SWEEP_DEG: f32 = 270.0;
}

impl LcdTemplate for SingleInfographicTemplate {
    fn descriptor() -> LcdEngineTemplateDescriptor {
        LcdEngineTemplateDescriptor {
            id: "single_infographic".to_string(),
            name: "Single Infographic".to_string(),
            params: vec![
                EffectParamDescriptor {
                    id: "sensor".to_string(),
                    label: "Sensor".to_string(),
                    kind: ParamKind::Sensor,
                    default: EffectParamValue::Str(String::new()),
                },
                EffectParamDescriptor {
                    id: "label".to_string(),
                    label: "Label text".to_string(),
                    kind: ParamKind::Text,
                    default: EffectParamValue::Str(String::new()),
                },
                EffectParamDescriptor {
                    id: "min".to_string(),
                    label: "Min value".to_string(),
                    kind: ParamKind::Range { min: 0.0, max: 1000.0, step: 1.0 },
                    default: EffectParamValue::Float(0.0),
                },
                EffectParamDescriptor {
                    id: "max".to_string(),
                    label: "Max value".to_string(),
                    kind: ParamKind::Range { min: 0.0, max: 1000.0, step: 1.0 },
                    default: EffectParamValue::Float(100.0),
                },
                EffectParamDescriptor {
                    id: "background".to_string(),
                    label: "Background".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(Self::BACKGROUND),
                },
                EffectParamDescriptor {
                    id: "number".to_string(),
                    label: "Number color".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(Self::NUMBER),
                },
                EffectParamDescriptor {
                    id: "text".to_string(),
                    label: "Label color".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(Self::TEXT),
                },
                EffectParamDescriptor {
                    id: "ring".to_string(),
                    label: "Ring color".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(Self::RING),
                },
            ],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Box<dyn LcdTemplate> {
        Box::new(Self {
            font: load_font(),
            sensor_id: param_str(params, "sensor", ""),
            label: param_str(params, "label", ""),
            min: param_f64(params, "min", 0.0),
            max: param_f64(params, "max", 100.0),
            background: param_color(params, "background", Self::BACKGROUND),
            number: param_color(params, "number", Self::NUMBER),
            text: param_color(params, "text", Self::TEXT),
            ring: param_color(params, "ring", Self::RING),
        })
    }

    fn render(&mut self, ctx: &TemplateCtx) -> Result<RgbaImage, String> {
        let (w, h) = (ctx.width, ctx.height);
        let mut img = RgbaImage::from_pixel(w, h, rgba(self.background));

        let sensor = ctx.sensors.get(&self.sensor_id);
        let span = self.max - self.min;
        let frac = match sensor {
            Some(s) if span.abs() > f64::EPSILON => {
                (((s.value - self.min) / span).clamp(0.0, 1.0)) as f32
            }
            _ => 0.0,
        };

        // ── Ring gauge ────────────────────────────────────────────────────────
        let cx = w as f32 / 2.0;
        let cy = h as f32 / 2.0;
        // Inset from the panel edge so the ring isn't flush with the border.
        let r_out = (w.min(h) as f32 / 2.0) * 0.80;
        let thickness = (r_out * 0.14).max(2.0);
        let r_in = r_out - thickness;

        let track = dim_color(self.ring, 0.25);
        for y in 0..h {
            for x in 0..w {
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                let r = (dx * dx + dy * dy).sqrt();
                if r < r_in || r > r_out {
                    continue;
                }
                let offset = (angle_cw_from_top(dx, dy) - Self::START_DEG).rem_euclid(360.0);
                if offset > Self::SWEEP_DEG {
                    continue; // bottom gap
                }
                let t = offset / Self::SWEEP_DEG;
                let c = if t <= frac { self.ring } else { track };
                img.put_pixel(x, y, rgba(c));
            }
        }

        // Dot marking the current value.
        let dot_ang = (Self::START_DEG + frac * Self::SWEEP_DEG).to_radians();
        let r_mid = (r_in + r_out) / 2.0;
        let dot_x = (cx + dot_ang.sin() * r_mid) as i32;
        let dot_y = (cy - dot_ang.cos() * r_mid) as i32;
        let dot_r = (thickness * 0.9).max(2.0) as i32;
        draw_filled_circle_mut(&mut img, (dot_x, dot_y), dot_r, rgba(self.ring));

        // ── Number (bold, centered) ───────────────────────────────────────────
        let number_text = match sensor {
            Some(s) => format_value(s.value),
            None => "--".to_string(),
        };
        let num_scale = PxScale::from(h as f32 * 0.26);
        let (nx, ny) = center_text(&self.font, num_scale, &number_text, w, h);
        draw_text_bold(&mut img, rgba(self.number), nx, ny, num_scale, &self.font, &number_text, 2);

        // ── Label (bold, below the number with breathing room) ────────────────
        let label_text = if !self.label.is_empty() {
            self.label.clone()
        } else {
            sensor.map(|s| s.name.clone()).unwrap_or_default()
        };
        if !label_text.is_empty() {
            let lbl_scale = PxScale::from(h as f32 * 0.12);
            let (lx, _) = center_text(&self.font, lbl_scale, &label_text, w, h);
            let ly = ny + (num_scale.y + h as f32 * 0.10) as i32;
            draw_text_bold(&mut img, rgba(self.text), lx, ly, lbl_scale, &self.font, &label_text, 1);
        }

        Ok(img)
    }
}

// ── Registry ──────────────────────────────────────────────────────────────────

pub fn build(
    id: &str,
    params: &HashMap<String, EffectParamValue>,
) -> Option<Box<dyn LcdTemplate>> {
    match id {
        "frame_counter" => Some(FrameCounterTemplate::from_params(params)),
        "single_infographic" => Some(SingleInfographicTemplate::from_params(params)),
        _ => {
            log::warn!("Unknown LCD template id: {id}");
            None
        }
    }
}

pub fn all_descriptors() -> Vec<LcdEngineTemplateDescriptor> {
    vec![
        FrameCounterTemplate::descriptor(),
        SingleInfographicTemplate::descriptor(),
    ]
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rgba(c: RgbColor) -> Rgba<u8> {
    Rgba([c.r, c.g, c.b, 255])
}

/// Scale every channel of `c` by `factor` (used for the unfilled gauge track).
fn dim_color(c: RgbColor, factor: f32) -> RgbColor {
    let scale = |v: u8| (v as f32 * factor).round().clamp(0.0, 255.0) as u8;
    RgbColor { r: scale(c.r), g: scale(c.g), b: scale(c.b) }
}

/// Angle of `(dx, dy)` in degrees, measured clockwise from 12 o'clock, in `[0, 360)`.
/// `dy` is in screen space (positive = down).
fn angle_cw_from_top(dx: f32, dy: f32) -> f32 {
    dx.atan2(-dy).to_degrees().rem_euclid(360.0)
}

/// Format a sensor reading for the big central number, e.g. `42°`.
fn format_value(value: f64) -> String {
    format!("{:.0}°", value)
}

/// Draw `text` repeatedly over a small offset grid to fake a bold weight —
/// the bundled font ships only a Regular face. `weight` is the offset radius
/// in pixels (1 ≈ semibold, 2 ≈ bold).
#[allow(clippy::too_many_arguments)]
fn draw_text_bold(
    img: &mut RgbaImage,
    color: Rgba<u8>,
    x: i32,
    y: i32,
    scale: PxScale,
    font: &FontRef,
    text: &str,
    weight: i32,
) {
    for ox in -weight..=weight {
        for oy in -weight..=weight {
            draw_text_mut(img, color, x + ox, y + oy, scale, font, text);
        }
    }
}

fn center_text(font: &FontRef, scale: PxScale, text: &str, w: u32, h: u32) -> (i32, i32) {
    let scaled = font.as_scaled(scale);
    let text_w: f32 = text
        .chars()
        .map(|c| scaled.h_advance(scaled.glyph_id(c)))
        .sum();
    let ascent = scaled.ascent();
    let descent = scaled.descent();
    let line_h = ascent - descent;

    let x = ((w as f32 - text_w) / 2.0).max(0.0) as i32;
    let y = ((h as f32 - line_h) / 2.0).max(0.0) as i32;
    (x, y)
}

/// Encode a frame as a PNG for the UI preview broadcast.
///
/// The device itself never sees this — it gets raw RGBA streamed by the LCD
/// engine. PNG is only for the UI's gdk pixbuf loader (format auto-detected).
/// Fast compression keeps the per-tick cost low.
pub fn encode_png(img: &RgbaImage) -> Result<Vec<u8>, String> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::{ExtendedColorType, ImageEncoder};

    let mut buf = Vec::new();
    PngEncoder::new_with_quality(&mut buf, CompressionType::Fast, FilterType::NoFilter)
        .write_image(img.as_raw(), img.width(), img.height(), ExtendedColorType::Rgba8)
        .map_err(|e| format!("PNG encode: {e}"))?;
    Ok(buf)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use halod_protocol::types::{SensorType, SensorUnit};

    fn sensor(id: &str, value: f64) -> Sensor {
        Sensor {
            id: id.to_string(),
            name: format!("{id} name"),
            value,
            unit: SensorUnit::Celsius,
            sensor_type: SensorType::default(),
            visibility: Default::default(),
        }
    }

    #[test]
    fn frame_counter_renders_native_size() {
        let sensors = HashMap::new();
        let ctx = TemplateCtx { width: 240, height: 240, t: 0.0, frame: 42, sensors: &sensors };
        let mut tmpl = FrameCounterTemplate::default();
        let img = tmpl.render(&ctx).expect("render failed");
        assert_eq!(img.dimensions(), (240, 240));
        assert_eq!(img.as_raw().len(), 240 * 240 * 4);
    }

    #[test]
    fn encode_png_produces_png_magic() {
        let img = RgbaImage::from_pixel(8, 8, Rgba([1, 2, 3, 255]));
        let png = encode_png(&img).expect("encode failed");
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    }

    #[test]
    fn available_templates_not_empty() {
        assert!(!all_descriptors().is_empty());
    }

    #[test]
    fn all_descriptors_includes_single_infographic() {
        assert!(all_descriptors().iter().any(|d| d.id == "single_infographic"));
    }

    #[test]
    fn build_frame_counter_succeeds() {
        assert!(build("frame_counter", &HashMap::new()).is_some());
    }

    #[test]
    fn build_single_infographic_succeeds() {
        assert!(build("single_infographic", &HashMap::new()).is_some());
    }

    #[test]
    fn build_unknown_returns_none() {
        assert!(build("nonexistent_template", &HashMap::new()).is_none());
    }

    #[test]
    fn infographic_descriptor_has_expected_params() {
        let desc = SingleInfographicTemplate::descriptor();
        assert_eq!(desc.id, "single_infographic");
        let ids: Vec<&str> = desc.params.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            ["sensor", "label", "min", "max", "background", "number", "text", "ring"]
        );
        // The sensor param is a Sensor picker; the label is free text.
        assert!(matches!(desc.params[0].kind, ParamKind::Sensor));
        assert!(matches!(desc.params[1].kind, ParamKind::Text));
    }

    #[test]
    fn infographic_renders_native_size_with_missing_sensor() {
        let sensors = HashMap::new();
        let ctx = TemplateCtx { width: 240, height: 240, t: 0.0, frame: 0, sensors: &sensors };
        let mut tmpl = SingleInfographicTemplate::from_params(&HashMap::new());
        let img = tmpl.render(&ctx).expect("render failed");
        assert_eq!(img.dimensions(), (240, 240));
        assert_eq!(img.as_raw().len(), 240 * 240 * 4);
    }

    #[test]
    fn infographic_renders_with_sensor_value() {
        let mut params = HashMap::new();
        params.insert("sensor".to_string(), EffectParamValue::Str("cpu".to_string()));
        params.insert("min".to_string(), EffectParamValue::Float(20.0));
        params.insert("max".to_string(), EffectParamValue::Float(90.0));

        let mut sensors = HashMap::new();
        sensors.insert("cpu".to_string(), sensor("cpu", 55.0));
        let ctx = TemplateCtx { width: 200, height: 200, t: 0.0, frame: 0, sensors: &sensors };

        let mut tmpl = SingleInfographicTemplate::from_params(&params);
        let img = tmpl.render(&ctx).expect("render failed");
        assert_eq!(img.dimensions(), (200, 200));
    }

    #[test]
    fn gauge_value_fraction_clamps() {
        // value below min and above max both clamp into [0, 1].
        let below = (((10.0_f64 - 20.0) / 70.0).clamp(0.0, 1.0)) as f32;
        let above = (((200.0_f64 - 20.0) / 70.0).clamp(0.0, 1.0)) as f32;
        assert_eq!(below, 0.0);
        assert_eq!(above, 1.0);
    }

    #[test]
    fn angle_cw_from_top_cardinal_directions() {
        assert!((angle_cw_from_top(0.0, -1.0) - 0.0).abs() < 1e-3); // up
        assert!((angle_cw_from_top(1.0, 0.0) - 90.0).abs() < 1e-3); // right
        assert!((angle_cw_from_top(0.0, 1.0) - 180.0).abs() < 1e-3); // down
        assert!((angle_cw_from_top(-1.0, 0.0) - 270.0).abs() < 1e-3); // left
    }

    #[test]
    fn dim_color_scales_channels() {
        let dimmed = dim_color(RgbColor { r: 200, g: 100, b: 40 }, 0.25);
        assert_eq!(dimmed, RgbColor { r: 50, g: 25, b: 10 });
    }
}
