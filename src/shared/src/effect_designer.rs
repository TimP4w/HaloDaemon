//! Math and parameter schema for the Designer procedural LED effect.

use std::collections::HashMap;

use crate::types::{EffectParamDescriptor, EffectParamValue, ParamKind, RgbColor};

pub const DESIGNER_EFFECT_ID: &str = "designer";
/// Same math as `DESIGNER_EFFECT_ID`, but rendered spatially across the canvas pixmap.
pub const DESIGNER_PIXMAP_EFFECT_ID: &str = "designer_pixmap";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generator {
    Sine,
    Pulse,
    Comet,
    Sawtooth,
    Twinkle,
    Noise,
    Rain,
}

impl Generator {
    pub const ALL: [Generator; 7] = [
        Generator::Sine,
        Generator::Pulse,
        Generator::Comet,
        Generator::Sawtooth,
        Generator::Twinkle,
        Generator::Noise,
        Generator::Rain,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Generator::Sine => "sine",
            Generator::Pulse => "pulse",
            Generator::Comet => "comet",
            Generator::Sawtooth => "sawtooth",
            Generator::Twinkle => "twinkle",
            Generator::Noise => "noise",
            Generator::Rain => "rain",
        }
    }

    pub fn from_str_or_default(s: &str) -> Self {
        match s {
            "pulse" => Generator::Pulse,
            "comet" => Generator::Comet,
            "sawtooth" => Generator::Sawtooth,
            "twinkle" => Generator::Twinkle,
            "noise" => Generator::Noise,
            "rain" => Generator::Rain,
            _ => Generator::Sine,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Reverse,
    Center,
}

impl Direction {
    pub const ALL: [Direction; 3] = [Direction::Forward, Direction::Reverse, Direction::Center];

    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Forward => "forward",
            Direction::Reverse => "reverse",
            Direction::Center => "center",
        }
    }

    pub fn from_str_or_default(s: &str) -> Self {
        match s {
            "reverse" => Direction::Reverse,
            "center" => Direction::Center,
            _ => Direction::Forward,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Solid,
    Gradient,
    Spectrum,
}

impl ColorMode {
    pub const ALL: [ColorMode; 3] = [ColorMode::Solid, ColorMode::Gradient, ColorMode::Spectrum];

    pub fn as_str(self) -> &'static str {
        match self {
            ColorMode::Solid => "solid",
            ColorMode::Gradient => "gradient",
            ColorMode::Spectrum => "spectrum",
        }
    }

    pub fn from_str_or_default(s: &str) -> Self {
        match s {
            "solid" => ColorMode::Solid,
            "spectrum" => ColorMode::Spectrum,
            _ => ColorMode::Gradient,
        }
    }
}

/// How multi-ring channels are treated for motion: as one chain or independent rings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingScope {
    /// Motion sweeps once across the whole zone, nose-to-tail through every ring.
    Zone,
    /// Motion repeats independently in each ring.
    PerRing,
}

impl RingScope {
    pub const ALL: [RingScope; 2] = [RingScope::Zone, RingScope::PerRing];

    pub fn as_str(self) -> &'static str {
        match self {
            RingScope::Zone => "zone",
            RingScope::PerRing => "per_ring",
        }
    }

    pub fn from_str_or_default(s: &str) -> Self {
        match s {
            "zone" => RingScope::Zone,
            _ => RingScope::PerRing,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DesignerParams {
    pub generator: Generator,
    pub direction: Direction,
    pub speed: f32,
    pub density: f32,
    pub decay: f32,
    pub width: f32,
    pub sharpness: f32,
    pub color_mode: ColorMode,
    pub color_a: RgbColor,
    pub color_b: RgbColor,
    pub floor: f32,
    pub saturation: f32,
    pub hue_drift: f32,
    pub ring_scope: RingScope,
    /// Speed of independent hue rotation (0 = static, 100 = fast).
    pub color_cycle_speed: f32,
    /// Phase stagger per LED (0 = all in sync, 100 = max spread).
    pub phase_spread: f32,
    /// Base color the effect is modulated onto instead of black.
    pub ambient_color: RgbColor,
}

impl Default for DesignerParams {
    fn default() -> Self {
        Self {
            generator: Generator::Sine,
            direction: Direction::Forward,
            speed: 50.0,
            density: 2.0,
            decay: 60.0,
            width: 30.0,
            sharpness: 40.0,
            color_mode: ColorMode::Gradient,
            color_a: RgbColor {
                r: 0x5a,
                g: 0xd1,
                b: 0xe8,
            },
            color_b: RgbColor {
                r: 0xa7,
                g: 0x8b,
                b: 0xfa,
            },
            floor: 6.0,
            saturation: 90.0,
            hue_drift: 12.0,
            ring_scope: RingScope::PerRing,
            color_cycle_speed: 0.0,
            phase_spread: 0.0,
            ambient_color: RgbColor { r: 0, g: 0, b: 0 },
        }
    }
}

fn clamp_f32(v: f64, min: f32, max: f32) -> f32 {
    (v as f32).clamp(min, max)
}

fn param_f32(
    params: &HashMap<String, EffectParamValue>,
    key: &str,
    min: f32,
    max: f32,
    default: f32,
) -> f32 {
    match params.get(key) {
        Some(EffectParamValue::Float(v)) => clamp_f32(*v, min, max),
        _ => default,
    }
}

fn param_str<'a>(params: &'a HashMap<String, EffectParamValue>, key: &str) -> Option<&'a str> {
    match params.get(key) {
        Some(EffectParamValue::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn param_color(
    params: &HashMap<String, EffectParamValue>,
    key: &str,
    default: RgbColor,
) -> RgbColor {
    match params.get(key) {
        Some(EffectParamValue::Color(c)) => *c,
        _ => default,
    }
}

impl DesignerParams {
    /// Builds params from the wire map, clamping numerics and falling back to defaults.
    pub fn from_params(params: &HashMap<String, EffectParamValue>) -> Self {
        let d = Self::default();
        Self {
            generator: param_str(params, "generator")
                .map(Generator::from_str_or_default)
                .unwrap_or(d.generator),
            direction: param_str(params, "direction")
                .map(Direction::from_str_or_default)
                .unwrap_or(d.direction),
            speed: param_f32(params, "speed", 0.0, 100.0, d.speed),
            density: param_f32(params, "density", 1.0, 8.0, d.density),
            decay: param_f32(params, "decay", 0.0, 100.0, d.decay),
            width: param_f32(params, "width", 0.0, 100.0, d.width),
            sharpness: param_f32(params, "sharpness", 0.0, 100.0, d.sharpness),
            color_mode: param_str(params, "color_mode")
                .map(ColorMode::from_str_or_default)
                .unwrap_or(d.color_mode),
            color_a: param_color(params, "color_a", d.color_a),
            color_b: param_color(params, "color_b", d.color_b),
            floor: param_f32(params, "floor", 0.0, 100.0, d.floor),
            saturation: param_f32(params, "saturation", 0.0, 100.0, d.saturation),
            hue_drift: param_f32(params, "hue_drift", -100.0, 100.0, d.hue_drift),
            ring_scope: param_str(params, "ring_scope")
                .map(RingScope::from_str_or_default)
                .unwrap_or(d.ring_scope),
            color_cycle_speed: param_f32(
                params,
                "color_cycle_speed",
                0.0,
                100.0,
                d.color_cycle_speed,
            ),
            phase_spread: param_f32(params, "phase_spread", 0.0, 100.0, d.phase_spread),
            ambient_color: param_color(params, "ambient_color", d.ambient_color),
        }
    }

    pub fn to_params(&self) -> HashMap<String, EffectParamValue> {
        let mut m = HashMap::new();
        m.insert(
            "generator".to_string(),
            EffectParamValue::Str(self.generator.as_str().to_string()),
        );
        m.insert(
            "direction".to_string(),
            EffectParamValue::Str(self.direction.as_str().to_string()),
        );
        m.insert(
            "speed".to_string(),
            EffectParamValue::Float(self.speed as f64),
        );
        m.insert(
            "density".to_string(),
            EffectParamValue::Float(self.density as f64),
        );
        m.insert(
            "decay".to_string(),
            EffectParamValue::Float(self.decay as f64),
        );
        m.insert(
            "width".to_string(),
            EffectParamValue::Float(self.width as f64),
        );
        m.insert(
            "sharpness".to_string(),
            EffectParamValue::Float(self.sharpness as f64),
        );
        m.insert(
            "color_mode".to_string(),
            EffectParamValue::Str(self.color_mode.as_str().to_string()),
        );
        m.insert("color_a".to_string(), EffectParamValue::Color(self.color_a));
        m.insert("color_b".to_string(), EffectParamValue::Color(self.color_b));
        m.insert(
            "floor".to_string(),
            EffectParamValue::Float(self.floor as f64),
        );
        m.insert(
            "saturation".to_string(),
            EffectParamValue::Float(self.saturation as f64),
        );
        m.insert(
            "hue_drift".to_string(),
            EffectParamValue::Float(self.hue_drift as f64),
        );
        m.insert(
            "ring_scope".to_string(),
            EffectParamValue::Str(self.ring_scope.as_str().to_string()),
        );
        m.insert(
            "color_cycle_speed".to_string(),
            EffectParamValue::Float(self.color_cycle_speed as f64),
        );
        m.insert(
            "phase_spread".to_string(),
            EffectParamValue::Float(self.phase_spread as f64),
        );
        m.insert(
            "ambient_color".to_string(),
            EffectParamValue::Color(self.ambient_color),
        );
        m
    }
}

pub fn param_descriptors() -> Vec<EffectParamDescriptor> {
    let d = DesignerParams::default();
    vec![
        EffectParamDescriptor {
            id: "generator".to_string(),
            label: "Generator".to_string(),
            kind: ParamKind::Enum {
                options: Generator::ALL
                    .iter()
                    .map(|g| g.as_str().to_string())
                    .collect(),
            },
            default: EffectParamValue::Str(d.generator.as_str().to_string()),
        },
        EffectParamDescriptor {
            id: "direction".to_string(),
            label: "Direction".to_string(),
            kind: ParamKind::Enum {
                options: Direction::ALL
                    .iter()
                    .map(|d| d.as_str().to_string())
                    .collect(),
            },
            default: EffectParamValue::Str(d.direction.as_str().to_string()),
        },
        EffectParamDescriptor {
            id: "speed".to_string(),
            label: "Speed".to_string(),
            kind: ParamKind::Range {
                min: 0.0,
                max: 100.0,
                step: 1.0,
            },
            default: EffectParamValue::Float(d.speed as f64),
        },
        EffectParamDescriptor {
            id: "density".to_string(),
            label: "Repeats".to_string(),
            kind: ParamKind::Range {
                min: 1.0,
                max: 8.0,
                step: 1.0,
            },
            default: EffectParamValue::Float(d.density as f64),
        },
        EffectParamDescriptor {
            id: "decay".to_string(),
            label: "Decay".to_string(),
            kind: ParamKind::Range {
                min: 0.0,
                max: 100.0,
                step: 1.0,
            },
            default: EffectParamValue::Float(d.decay as f64),
        },
        EffectParamDescriptor {
            id: "width".to_string(),
            label: "Width".to_string(),
            kind: ParamKind::Range {
                min: 0.0,
                max: 100.0,
                step: 1.0,
            },
            default: EffectParamValue::Float(d.width as f64),
        },
        EffectParamDescriptor {
            id: "sharpness".to_string(),
            label: "Sharpness".to_string(),
            kind: ParamKind::Range {
                min: 0.0,
                max: 100.0,
                step: 1.0,
            },
            default: EffectParamValue::Float(d.sharpness as f64),
        },
        EffectParamDescriptor {
            id: "color_mode".to_string(),
            label: "Color mode".to_string(),
            kind: ParamKind::Enum {
                options: ColorMode::ALL
                    .iter()
                    .map(|c| c.as_str().to_string())
                    .collect(),
            },
            default: EffectParamValue::Str(d.color_mode.as_str().to_string()),
        },
        EffectParamDescriptor {
            id: "color_a".to_string(),
            label: "Color A".to_string(),
            kind: ParamKind::Color,
            default: EffectParamValue::Color(d.color_a),
        },
        EffectParamDescriptor {
            id: "color_b".to_string(),
            label: "Color B".to_string(),
            kind: ParamKind::Color,
            default: EffectParamValue::Color(d.color_b),
        },
        EffectParamDescriptor {
            id: "floor".to_string(),
            label: "Brightness floor".to_string(),
            kind: ParamKind::Range {
                min: 0.0,
                max: 100.0,
                step: 1.0,
            },
            default: EffectParamValue::Float(d.floor as f64),
        },
        EffectParamDescriptor {
            id: "saturation".to_string(),
            label: "Saturation".to_string(),
            kind: ParamKind::Range {
                min: 0.0,
                max: 100.0,
                step: 1.0,
            },
            default: EffectParamValue::Float(d.saturation as f64),
        },
        EffectParamDescriptor {
            id: "hue_drift".to_string(),
            label: "Hue drift".to_string(),
            kind: ParamKind::Range {
                min: -100.0,
                max: 100.0,
                step: 1.0,
            },
            default: EffectParamValue::Float(d.hue_drift as f64),
        },
        EffectParamDescriptor {
            id: "ring_scope".to_string(),
            label: "Ring scope".to_string(),
            kind: ParamKind::Enum {
                options: RingScope::ALL
                    .iter()
                    .map(|s| s.as_str().to_string())
                    .collect(),
            },
            default: EffectParamValue::Str(d.ring_scope.as_str().to_string()),
        },
        EffectParamDescriptor {
            id: "color_cycle_speed".to_string(),
            label: "Color cycle speed".to_string(),
            kind: ParamKind::Range {
                min: 0.0,
                max: 100.0,
                step: 1.0,
            },
            default: EffectParamValue::Float(d.color_cycle_speed as f64),
        },
        EffectParamDescriptor {
            id: "phase_spread".to_string(),
            label: "Phase spread".to_string(),
            kind: ParamKind::Range {
                min: 0.0,
                max: 100.0,
                step: 1.0,
            },
            default: EffectParamValue::Float(d.phase_spread as f64),
        },
        EffectParamDescriptor {
            id: "ambient_color".to_string(),
            label: "Ambient color".to_string(),
            kind: ParamKind::Color,
            default: EffectParamValue::Color(d.ambient_color),
        },
    ]
}

/// `[A-Za-z0-9 _-]`, non-empty, bounded — safe as a filename component.
pub fn validate_effect_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-'))
}

fn frac(x: f32) -> f32 {
    x - x.floor()
}

fn hash1(x: f32) -> f32 {
    let s = (x * 127.1).sin() * 43_758.547;
    frac(s)
}

fn value_noise(x: f32) -> f32 {
    let i = x.floor();
    let f = x - i;
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash1(i);
    let b = hash1(i + 1.0);
    a + (b - a) * u
}

impl DesignerParams {
    /// Brightness `b(p)` in `[0, 1]` for a LED at chain position `p`.
    /// `nx`/`ny` feed the twinkle/rain hash for spatial variation.
    pub fn brightness(&self, p: f32, nx: f32, ny: f32, t: f32) -> f32 {
        let s = self.speed / 100.0;
        let d = self.density.max(1.0);
        let (pp, sign) = match self.direction {
            Direction::Forward => (p, 1.0),
            Direction::Reverse => (p, -1.0),
            Direction::Center => ((2.0 * p - 1.0).abs(), -1.0),
        };
        let mv = t * (0.12 + s * 1.5);
        let sharp_pow = |b: f32, k: f32| b.max(0.0).powf(1.0 + (self.sharpness / 100.0) * k);

        // Phase spread: stagger each LED's phase based on its position.
        // At 100%, adjacent LEDs differ by up to ~1 full cycle.
        let phase_offset = p * self.phase_spread / 100.0;

        let b = match self.generator {
            Generator::Sine => {
                let ph = pp * d - sign * mv + phase_offset;
                let raw = 0.5 + 0.5 * (ph * std::f32::consts::TAU).sin();
                sharp_pow(raw, 4.0)
            }
            Generator::Sawtooth => {
                let raw = frac(pp * d - sign * mv + phase_offset);
                sharp_pow(raw, 3.0)
            }
            Generator::Pulse => {
                let gp = frac(pp * d - sign * mv + phase_offset);
                let dist = gp.min(1.0 - gp);
                let w = 0.02 + (self.width / 100.0) * 0.22;
                let core = (-(dist * dist) / (2.0 * w * w)).exp();
                let tail = (-dist / (0.015 + (self.decay / 100.0) * 0.5)).exp();
                let raw = core.max(tail * 0.85);
                sharp_pow(raw, 2.0)
            }
            Generator::Comet => {
                let gp = frac(pp * d - sign * mv + phase_offset);
                let w = 0.012 + (self.width / 100.0) * 0.06;
                let head = (-(gp * gp) / (2.0 * w * w)).exp();
                let tail = (-gp / (0.02 + (self.decay / 100.0) * 0.6)).exp();
                head.max(tail)
            }
            Generator::Twinkle => {
                let h = hash1(nx * 12.9898 + ny * 78.233);
                let rate = 0.4 + s * 2.2;
                let phase = t * rate + h;
                let cycle = phase.floor();
                let local = frac(phase);
                let dk = 6.0 - (self.decay / 100.0) * 5.0;
                let raw = (-local * dk).exp();
                let h2 = hash1(nx * 39.346 + ny * 11.135 + cycle * 7.919 + 3.3);
                let mask = if h2 < (0.25 + (d - 1.0) / 9.0) {
                    1.0
                } else {
                    0.1
                };
                let h3 = hash1(nx * 5.257 + ny * 17.833 + cycle * 4.271 + 9.1);
                let peak = 0.55 + 0.45 * h3;
                sharp_pow(raw * mask * peak, 1.5)
            }
            Generator::Noise => {
                let sc = 1.5 + d * 1.3;
                let raw = value_noise(pp * sc + sign * mv * 2.2 + phase_offset);
                sharp_pow(raw, 3.0)
            }
            Generator::Rain => {
                // Many simultaneous dots falling/flowing, each at a random
                // spatial position. Density controls how many drops are visible
                // at once, decay controls trail length.
                let count = (d * 8.0) as u32;
                let mut total = 0.0_f32;
                for i in 0..count {
                    let seed = hash1(nx * 37.17 + ny * 91.33 + i as f32 * 13.37);
                    // Each drop has a spatial anchor point that drifts.
                    let anchor = seed + t * (0.08 + s * 0.22);
                    // Drop cycles: a new drop triggers when its anchor crosses a
                    // boundary; compute local phase within this cycle.
                    let drop_phase = frac(anchor);
                    // Position along the zone where this drop currently is.
                    let drop_pos = frac(anchor * 0.7);
                    // How close is this LED to the drop's current position?
                    let dist_to_drop = (pp - drop_pos).abs();
                    let dist_wrapped = dist_to_drop.min(1.0 - dist_to_drop);
                    // Head + trail: bright core at the drop, fading tail behind.
                    let head_w = 0.018 + (self.width / 100.0) * 0.06;
                    let head = (-(dist_wrapped * dist_wrapped) / (2.0 * head_w * head_w)).exp();
                    let trail = (-dist_wrapped / (0.02 + (self.decay / 100.0) * 1.2)).exp();
                    // Drop is brightest mid-cycle, fades in/out at edges.
                    let life = 1.0 - ((drop_phase - 0.5).abs() * 2.0);
                    total += (head.max(trail * 0.85)) * life;
                }
                // Softer normalization: clamp instead of averaging so a single
                // nearby drop can fully light the LED.
                (total / (count as f32).max(1.0).sqrt()).clamp(0.0, 1.2)
            }
        };

        let fl = (self.floor / 100.0) * 0.85;
        (fl + b.clamp(0.0, 1.0) * (1.0 - fl)).clamp(0.0, 1.0)
    }

    fn hue_of(color: RgbColor) -> f32 {
        let r = color.r as f32 / 255.0;
        let g = color.g as f32 / 255.0;
        let b = color.b as f32 / 255.0;
        let mx = r.max(g).max(b);
        let mn = r.min(g).min(b);
        let delta = mx - mn;
        let mut h = if delta <= f32::EPSILON {
            0.0
        } else if mx == r {
            ((g - b) / delta) % 6.0
        } else if mx == g {
            (b - r) / delta + 2.0
        } else {
            (r - g) / delta + 4.0
        };
        h *= 60.0;
        (h % 360.0 + 360.0) % 360.0
    }

    /// sRGB color in `[0, 1]` per channel for the LED at position `p`.
    pub fn color(&self, p: f32, nx: f32, ny: f32, t: f32) -> (f32, f32, f32) {
        let b = self.brightness(p, nx, ny, t);
        let ambient = (
            self.ambient_color.r as f32 / 255.0,
            self.ambient_color.g as f32 / 255.0,
            self.ambient_color.b as f32 / 255.0,
        );
        let (r, g, bv) = match self.color_mode {
            ColorMode::Gradient => {
                let k = 0.08 + 0.92 * b;
                let mix =
                    |a: u8, c: u8| -> f32 { (a as f32 + (c as f32 - a as f32) * p) / 255.0 * k };
                (
                    mix(self.color_a.r, self.color_b.r),
                    mix(self.color_a.g, self.color_b.g),
                    mix(self.color_a.b, self.color_b.b),
                )
            }
            ColorMode::Solid | ColorMode::Spectrum => {
                let hd = self.hue_drift / 100.0;
                let sat = (self.saturation / 100.0).clamp(0.0, 1.0);
                let mut hue = Self::hue_of(self.color_a);
                if self.color_mode == ColorMode::Spectrum {
                    hue += p * 300.0 + t * 8.0;
                }
                hue += hd * (p * 140.0) + t * hd * 14.0;
                // Color cycle speed: independent hue rotation over time.
                let cc = self.color_cycle_speed / 100.0;
                hue += t * cc * 120.0;
                hue = (hue % 360.0 + 360.0) % 360.0;
                let l = (6.0 + b * 52.0) / 100.0;
                hsl_to_srgb(hue, sat, l.clamp(0.0, 1.0))
            }
        };
        // Additive ambient blend: ambient color shows through in dark regions
        // but never darkens the effect. With ambient black this is a no-op.
        let dark = 1.0 - b.clamp(0.0, 1.0);
        (
            (ambient.0 * dark + r).min(1.0),
            (ambient.1 * dark + g).min(1.0),
            (ambient.2 * dark + bv).min(1.0),
        )
    }
}

fn hsl_to_srgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s <= f32::EPSILON {
        return (l, l, l);
    }
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = if hp < 1.0 {
        (c, x, 0.0)
    } else if hp < 2.0 {
        (x, c, 0.0)
    } else if hp < 3.0 {
        (0.0, c, x)
    } else if hp < 4.0 {
        (0.0, x, c)
    } else if hp < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = l - c / 2.0;
    (
        (r1 + m).clamp(0.0, 1.0),
        (g1 + m).clamp(0.0, 1.0),
        (b1 + m).clamp(0.0, 1.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let p = DesignerParams::default();
        let params = p.to_params();
        let back = DesignerParams::from_params(&params);
        assert_eq!(p, back);
    }

    #[test]
    fn from_params_falls_back_to_default_on_missing_or_bad_types() {
        let mut params = HashMap::new();
        params.insert("speed".to_string(), EffectParamValue::Str("oops".into()));
        params.insert(
            "generator".to_string(),
            EffectParamValue::Str("bogus".into()),
        );
        let p = DesignerParams::from_params(&params);
        assert_eq!(p.speed, DesignerParams::default().speed);
        assert_eq!(p.generator, Generator::Sine);
    }

    #[test]
    fn from_params_clamps_out_of_range_numerics() {
        let mut params = HashMap::new();
        params.insert("speed".to_string(), EffectParamValue::Float(500.0));
        params.insert("hue_drift".to_string(), EffectParamValue::Float(-999.0));
        let p = DesignerParams::from_params(&params);
        assert_eq!(p.speed, 100.0);
        assert_eq!(p.hue_drift, -100.0);
    }

    #[test]
    fn validate_effect_name_rejects_traversal_and_leading_dot() {
        assert!(validate_effect_name("My Effect"));
        assert!(!validate_effect_name("../escape"));
        assert!(!validate_effect_name(".hidden"));
        assert!(!validate_effect_name(""));
        assert!(!validate_effect_name(&"a".repeat(65)));
    }

    #[test]
    fn floor_raises_black_brightness() {
        let p = DesignerParams {
            floor: 100.0,
            generator: Generator::Sine,
            ..Default::default()
        };
        // A point in the fully-dark part of the wave never drops below the
        // floor's contribution.
        let b = p.brightness(0.0, 0.0, 0.5, 0.0);
        assert!(b >= (p.floor / 100.0) * 0.85 - 1e-4, "b={b}");
    }

    #[test]
    fn center_direction_is_symmetric_for_non_twinkle_generators() {
        for g in [
            Generator::Sine,
            Generator::Sawtooth,
            Generator::Pulse,
            Generator::Comet,
            Generator::Noise,
        ] {
            let p = DesignerParams {
                direction: Direction::Center,
                generator: g,
                ..Default::default()
            };
            let a = p.brightness(0.25, 0.25, 0.5, 0.7);
            let b = p.brightness(0.75, 0.75, 0.5, 0.7);
            assert!((a - b).abs() < 1e-4, "generator={g:?} a={a} b={b}");
        }
    }

    #[test]
    fn twinkle_peaks_vary_between_cycles() {
        // Same LED, sampled at the peak of several successive twinkle
        // cycles: cycle-to-cycle randomness should give distinct peaks
        // rather than the same envelope repeating forever.
        let p = DesignerParams {
            generator: Generator::Twinkle,
            speed: 100.0,
            ..Default::default()
        };
        let rate = 0.4 + (p.speed / 100.0) * 2.2;
        let peaks: Vec<f32> = (0..5)
            .map(|cycle| p.brightness(0.5, 0.3, 0.7, cycle as f32 / rate))
            .collect();
        assert!(
            peaks.windows(2).any(|w| (w[0] - w[1]).abs() > 1e-3),
            "peaks did not vary across cycles: {peaks:?}"
        );
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    fn arb_params() -> impl Strategy<Value = DesignerParams> {
        (
            (
                prop::sample::select(Generator::ALL.to_vec()),
                prop::sample::select(Direction::ALL.to_vec()),
                0.0f32..100.0,
                1.0f32..8.0,
                0.0f32..100.0,
                0.0f32..100.0,
                0.0f32..100.0,
                prop::sample::select(ColorMode::ALL.to_vec()),
            ),
            (any::<u8>(), any::<u8>(), any::<u8>()),
            (any::<u8>(), any::<u8>(), any::<u8>()),
            (0.0f32..100.0, 0.0f32..100.0, -100.0f32..100.0),
            prop::sample::select(RingScope::ALL.to_vec()),
        )
            .prop_map(
                |(
                    (generator, direction, speed, density, decay, width, sharpness, color_mode),
                    (ar, ag, ab),
                    (br, bg, bb),
                    (floor, saturation, hue_drift),
                    ring_scope,
                )| DesignerParams {
                    generator,
                    direction,
                    speed,
                    density,
                    decay,
                    width,
                    sharpness,
                    color_mode,
                    color_a: RgbColor {
                        r: ar,
                        g: ag,
                        b: ab,
                    },
                    color_b: RgbColor {
                        r: br,
                        g: bg,
                        b: bb,
                    },
                    floor,
                    saturation,
                    hue_drift,
                    ring_scope,
                    color_cycle_speed: 0.0,
                    phase_spread: 0.0,
                    ambient_color: RgbColor { r: 0, g: 0, b: 0 },
                },
            )
    }

    proptest! {
        #[test]
        fn round_trip_identity(p in arb_params()) {
            let params = p.to_params();
            let back = DesignerParams::from_params(&params);
            prop_assert_eq!(p, back);
        }

        #[test]
        fn from_params_of_arbitrary_map_is_always_in_range(
            speed in prop::option::of(-1000.0f64..1000.0),
            density in prop::option::of(-1000.0f64..1000.0),
            hue_drift in prop::option::of(-1000.0f64..1000.0),
        ) {
            let mut m = HashMap::new();
            if let Some(v) = speed { m.insert("speed".to_string(), EffectParamValue::Float(v)); }
            if let Some(v) = density { m.insert("density".to_string(), EffectParamValue::Float(v)); }
            if let Some(v) = hue_drift { m.insert("hue_drift".to_string(), EffectParamValue::Float(v)); }
            let p = DesignerParams::from_params(&m);
            prop_assert!((0.0..=100.0).contains(&p.speed));
            prop_assert!((1.0..=8.0).contains(&p.density));
            prop_assert!((-100.0..=100.0).contains(&p.hue_drift));
        }

        #[test]
        fn brightness_and_color_stay_in_unit_gamut(
            p in arb_params(),
            pos in 0.0f32..1.0,
            nx in 0.0f32..1.0,
            ny in 0.0f32..1.0,
            t in 0.0f32..1000.0,
        ) {
            let b = p.brightness(pos, nx, ny, t);
            prop_assert!((0.0..=1.0).contains(&b), "brightness {b} out of range");
            let (r, g, bl) = p.color(pos, nx, ny, t);
            for ch in [r, g, bl] {
                prop_assert!(ch.is_finite() && (0.0..=1.0).contains(&ch), "channel {ch} out of gamut");
            }
        }
    }
}
