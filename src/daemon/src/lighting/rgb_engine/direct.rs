use std::collections::HashMap;
use std::sync::Arc;

use halod_shared::effect_designer::{self, DesignerParams, DESIGNER_EFFECT_ID};
use halod_shared::types::{
    Animation, ColorStep, EffectParamDescriptor, EffectParamValue, ParamKind, RgbColor,
};

use super::color::{hue_to_srgb, srgb_to_linear, LinearColor};
use crate::services::audio::{self, AudioHandle, SpectrumFrame};
use crate::util::effect_params::{param_bool, param_color, param_f64, param_steps, param_str};

/// Direct effect id for [`SensorGradient`].
pub const SENSOR_GRADIENT_EFFECT_ID: &str = "sensor_gradient";
/// Direct effect id for [`SensorSteps`].
pub const SENSOR_STEPS_EFFECT_ID: &str = "sensor_steps";

// Direct effects compute `led_color` from the shared clock `t` — no pixmap needed.
pub trait DirectLedEffect: Send {
    fn tick(&mut self, _t: f32, _dt: f32) {}
    /// `p` is fractional chain position; `p_ring` is ring-local position
    /// (equal to `p` for single-ring zones). `nx`/`ny` feed the twinkle hash.
    fn led_color(&self, p: f32, p_ring: f32, nx: f32, ny: f32, t: f32) -> LinearColor;

    /// Sensor id this effect wants fed via `set_sensor_value` before each
    /// `tick()`, or `None` if it doesn't consume a sensor.
    fn sensor_id(&self) -> Option<&str> {
        None
    }
    /// Called once per engine tick, before `tick()`, with the latest reading
    /// for `sensor_id()` (`None` when the sensor is unset or unavailable).
    fn set_sensor_value(&mut self, _value: Option<f64>) {}
}

trait DirectEffect: DirectLedEffect {
    fn descriptor() -> Animation
    where
        Self: Sized;
    fn from_params(params: &HashMap<String, EffectParamValue>) -> Box<dyn DirectLedEffect>
    where
        Self: Sized;
}

struct Breathing {
    color: RgbColor,
    speed: f32,
}

impl DirectEffect for Breathing {
    fn descriptor() -> Animation {
        Animation {
            id: "breathing".to_string(),
            name: "Breathing".to_string(),
            params: vec![
                EffectParamDescriptor {
                    id: "color".to_string(),
                    label: "Color".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(RgbColor {
                        r: 0,
                        g: 128,
                        b: 255,
                    }),
                },
                EffectParamDescriptor {
                    id: "speed".to_string(),
                    label: "Speed".to_string(),
                    kind: ParamKind::Range {
                        min: 0.1,
                        max: 3.0,
                        step: 0.1,
                    },
                    default: EffectParamValue::Float(0.5),
                },
            ],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Box<dyn DirectLedEffect> {
        let color = param_color(
            params,
            "color",
            RgbColor {
                r: 0,
                g: 128,
                b: 255,
            },
        );
        let speed = param_f64(params, "speed", 0.5) as f32;
        Box::new(Self { color, speed })
    }
}

impl DirectLedEffect for Breathing {
    fn led_color(&self, _p: f32, _p_ring: f32, _nx: f32, _ny: f32, t: f32) -> LinearColor {
        let phase = (t * self.speed * std::f32::consts::PI).sin();
        let brightness = phase * phase;
        LinearColor {
            r: srgb_to_linear(self.color.r) * brightness,
            g: srgb_to_linear(self.color.g) * brightness,
            b: srgb_to_linear(self.color.b) * brightness,
        }
    }
}

struct AudioBeat {
    handle: Option<Arc<AudioHandle>>,
    color: RgbColor,
    decay: f32,
    sensitivity: f32,
    pulse: f32,
    last_seq: u64,
}

impl AudioBeat {
    fn tick_frame(&mut self, frame: SpectrumFrame, dt: f32) {
        let threshold = 0.6 - 0.5 * self.sensitivity;
        if frame.seq != self.last_seq && frame.flux >= threshold {
            self.pulse = 1.0;
        } else {
            self.pulse *= (-dt / (self.decay / 3.0)).exp();
        }
        self.last_seq = frame.seq;
    }

    #[cfg(test)]
    fn with_frame(color: RgbColor, decay: f32, sensitivity: f32) -> Self {
        Self {
            handle: None,
            color,
            decay,
            sensitivity,
            pulse: 0.0,
            last_seq: 0,
        }
    }
}

impl DirectEffect for AudioBeat {
    fn descriptor() -> Animation {
        Animation {
            id: "audio_beat".to_string(),
            name: "Audio Beat".to_string(),
            params: vec![
                EffectParamDescriptor {
                    id: "color".to_string(),
                    label: "Color".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(RgbColor {
                        r: 255,
                        g: 40,
                        b: 40,
                    }),
                },
                EffectParamDescriptor {
                    id: "decay".to_string(),
                    label: "Decay".to_string(),
                    kind: ParamKind::Range {
                        min: 0.1,
                        max: 2.0,
                        step: 0.05,
                    },
                    default: EffectParamValue::Float(0.4),
                },
                EffectParamDescriptor {
                    id: "sensitivity".to_string(),
                    label: "Sensitivity".to_string(),
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1.0,
                        step: 0.05,
                    },
                    default: EffectParamValue::Float(0.5),
                },
            ],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Box<dyn DirectLedEffect> {
        let color = param_color(
            params,
            "color",
            RgbColor {
                r: 255,
                g: 40,
                b: 40,
            },
        );
        let decay = param_f64(params, "decay", 0.4) as f32;
        let sensitivity = param_f64(params, "sensitivity", 0.5) as f32;
        Box::new(Self {
            handle: Some(audio::shared()),
            color,
            decay,
            sensitivity,
            pulse: 0.0,
            last_seq: 0,
        })
    }
}

impl DirectLedEffect for AudioBeat {
    fn tick(&mut self, _t: f32, dt: f32) {
        let frame = self.handle.as_ref().map(|h| h.latest()).unwrap_or_default();
        self.tick_frame(frame, dt);
    }

    fn led_color(&self, _p: f32, _p_ring: f32, _nx: f32, _ny: f32, _t: f32) -> LinearColor {
        LinearColor {
            r: srgb_to_linear(self.color.r) * self.pulse,
            g: srgb_to_linear(self.color.g) * self.pulse,
            b: srgb_to_linear(self.color.b) * self.pulse,
        }
    }
}

struct AudioLevel {
    handle: Option<Arc<AudioHandle>>,
    color: RgbColor,
    hue_shift: bool,
    smoothing: f32,
    sensitivity: f32,
    display_level: f32,
}

/// Longest smoothing time constant, in seconds, reached at `smoothing == 1.0`.
const AUDIO_LEVEL_MAX_TAU: f32 = 0.5;

/// Eases `current` toward `target` with time constant `tau` (seconds; `<= 0`
/// snaps instantly), so responsiveness is independent of the tick rate.
fn ease_toward(current: f32, target: f32, tau: f32, dt: f32) -> f32 {
    let alpha = if tau <= f32::EPSILON {
        1.0
    } else {
        1.0 - (-dt / tau).exp()
    };
    current + (target - current) * alpha
}

impl AudioLevel {
    fn tick_frame(&mut self, frame: SpectrumFrame, dt: f32) {
        let target = (frame.level * self.sensitivity).clamp(0.0, 1.0);
        let tau = self.smoothing.clamp(0.0, 1.0) * AUDIO_LEVEL_MAX_TAU;
        self.display_level = ease_toward(self.display_level, target, tau, dt);
    }

    #[cfg(test)]
    fn with_frame(color: RgbColor, hue_shift: bool, smoothing: f32, sensitivity: f32) -> Self {
        Self {
            handle: None,
            color,
            hue_shift,
            smoothing,
            sensitivity,
            display_level: 0.0,
        }
    }
}

impl DirectEffect for AudioLevel {
    fn descriptor() -> Animation {
        Animation {
            id: "audio_level".to_string(),
            name: "Audio Level".to_string(),
            params: vec![
                EffectParamDescriptor {
                    id: "color".to_string(),
                    label: "Color".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(RgbColor {
                        r: 0,
                        g: 200,
                        b: 120,
                    }),
                },
                EffectParamDescriptor {
                    id: "hue_shift".to_string(),
                    label: "Hue Shift".to_string(),
                    kind: ParamKind::Boolean,
                    default: EffectParamValue::Bool(false),
                },
                EffectParamDescriptor {
                    id: "smoothing".to_string(),
                    label: "Smoothing".to_string(),
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1.0,
                        step: 0.05,
                    },
                    default: EffectParamValue::Float(0.3),
                },
                EffectParamDescriptor {
                    id: "sensitivity".to_string(),
                    label: "Sensitivity".to_string(),
                    kind: ParamKind::Range {
                        min: 0.1,
                        max: 3.0,
                        step: 0.1,
                    },
                    default: EffectParamValue::Float(1.0),
                },
            ],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Box<dyn DirectLedEffect> {
        let color = param_color(
            params,
            "color",
            RgbColor {
                r: 0,
                g: 200,
                b: 120,
            },
        );
        let hue_shift = param_bool(params, "hue_shift", false);
        let smoothing = param_f64(params, "smoothing", 0.3) as f32;
        let sensitivity = param_f64(params, "sensitivity", 1.0) as f32;
        Box::new(Self {
            handle: Some(audio::shared()),
            color,
            hue_shift,
            smoothing,
            sensitivity,
            display_level: 0.0,
        })
    }
}

impl DirectLedEffect for AudioLevel {
    fn tick(&mut self, _t: f32, dt: f32) {
        let frame = self.handle.as_ref().map(|h| h.latest()).unwrap_or_default();
        self.tick_frame(frame, dt);
    }

    fn led_color(&self, _p: f32, _p_ring: f32, _nx: f32, _ny: f32, _t: f32) -> LinearColor {
        let brightness = self.display_level;
        if self.hue_shift {
            let hue = 0.66 - 0.66 * brightness;
            let (sr, sg, sb) = hue_to_srgb(hue);
            LinearColor {
                r: srgb_to_linear(sr) * brightness,
                g: srgb_to_linear(sg) * brightness,
                b: srgb_to_linear(sb) * brightness,
            }
        } else {
            LinearColor {
                r: srgb_to_linear(self.color.r) * brightness,
                g: srgb_to_linear(self.color.g) * brightness,
                b: srgb_to_linear(self.color.b) * brightness,
            }
        }
    }
}

/// Longest smoothing time constant, in seconds, reached at `smoothing == 1.0`.
const SENSOR_GRADIENT_MAX_TAU: f32 = 5.0;

/// Clamp bounds for the free-typed sensor-unit params (min/max/steps) — wide
/// enough for any sensor unit (°C, %, RPM, MHz).
const SENSOR_PARAM_MIN: f64 = -100_000.0;
const SENSOR_PARAM_MAX: f64 = 100_000.0;

// Fully saturated defaults: washed-out colors (e.g. `0xf83838`) read pink on
// LEDs, whose channels don't match a monitor's relative brightness.
const SENSOR_COOL_DEFAULT: RgbColor = RgbColor {
    r: 0,
    g: 128,
    b: 255,
};
const SENSOR_HOT_DEFAULT: RgbColor = RgbColor { r: 255, g: 0, b: 0 };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SensorEffectMode {
    Gradient,
    Meter,
}

impl SensorEffectMode {
    const ALL: [SensorEffectMode; 2] = [SensorEffectMode::Gradient, SensorEffectMode::Meter];

    fn as_str(self) -> &'static str {
        match self {
            SensorEffectMode::Gradient => "gradient",
            SensorEffectMode::Meter => "meter",
        }
    }

    fn from_str_or_default(s: &str) -> Self {
        match s {
            "meter" => SensorEffectMode::Meter,
            _ => SensorEffectMode::Gradient,
        }
    }
}

fn lerp_color(a: RgbColor, b: RgbColor, t: f32) -> RgbColor {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| -> u8 {
        (x as f32 + (y as f32 - x as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    RgbColor {
        r: mix(a.r, b.r),
        g: mix(a.g, b.g),
        b: mix(a.b, b.b),
    }
}

/// Colors a zone (or, in `meter` mode, fills it) by a live sensor reading
/// normalized against `[min, max]`, blended along a two-stop gradient.
struct SensorGradient {
    sensor: String,
    mode: SensorEffectMode,
    color_a: RgbColor,
    color_b: RgbColor,
    min: f32,
    max: f32,
    smoothing: f32,
    last_value: Option<f64>,
    /// Smoothed, normalized `[0,1]` reading.
    display_level: f32,
    /// Smoothed `[0,1]` factor that fades output to black when the sensor is
    /// unset or its reading is unavailable, and back up when it reappears.
    presence: f32,
}

impl DirectEffect for SensorGradient {
    fn descriptor() -> Animation {
        Animation {
            id: SENSOR_GRADIENT_EFFECT_ID.to_string(),
            name: "Sensor Gradient".to_string(),
            params: vec![
                EffectParamDescriptor {
                    id: "sensor".to_string(),
                    label: "Sensor".to_string(),
                    kind: ParamKind::Sensor,
                    default: EffectParamValue::Str(String::new()),
                },
                EffectParamDescriptor {
                    id: "mode".to_string(),
                    label: "Mode".to_string(),
                    kind: ParamKind::Enum {
                        options: SensorEffectMode::ALL
                            .iter()
                            .map(|m| m.as_str().to_string())
                            .collect(),
                    },
                    default: EffectParamValue::Str(SensorEffectMode::Gradient.as_str().to_string()),
                },
                EffectParamDescriptor {
                    id: "color_a".to_string(),
                    label: "Color A".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(SENSOR_COOL_DEFAULT),
                },
                EffectParamDescriptor {
                    id: "color_b".to_string(),
                    label: "Color B".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(SENSOR_HOT_DEFAULT),
                },
                EffectParamDescriptor {
                    id: "min".to_string(),
                    label: "Min".to_string(),
                    kind: ParamKind::Number {
                        min: SENSOR_PARAM_MIN,
                        max: SENSOR_PARAM_MAX,
                    },
                    default: EffectParamValue::Float(20.0),
                },
                EffectParamDescriptor {
                    id: "max".to_string(),
                    label: "Max".to_string(),
                    kind: ParamKind::Number {
                        min: SENSOR_PARAM_MIN,
                        max: SENSOR_PARAM_MAX,
                    },
                    default: EffectParamValue::Float(90.0),
                },
                EffectParamDescriptor {
                    id: "smoothing".to_string(),
                    label: "Smoothing".to_string(),
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1.0,
                        step: 0.05,
                    },
                    default: EffectParamValue::Float(0.3),
                },
            ],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Box<dyn DirectLedEffect> {
        let sensor = param_str(params, "sensor", "");
        let mode = SensorEffectMode::from_str_or_default(&param_str(params, "mode", ""));
        let color_a = param_color(params, "color_a", SENSOR_COOL_DEFAULT);
        let color_b = param_color(params, "color_b", SENSOR_HOT_DEFAULT);
        let min = param_f64(params, "min", 20.0) as f32;
        let max = param_f64(params, "max", 90.0) as f32;
        let smoothing = param_f64(params, "smoothing", 0.3) as f32;
        Box::new(Self {
            sensor,
            mode,
            color_a,
            color_b,
            min,
            max,
            smoothing,
            last_value: None,
            display_level: 0.0,
            presence: 0.0,
        })
    }
}

impl SensorGradient {
    fn normalized(&self, v: f32) -> f32 {
        let range = self.max - self.min;
        if range.abs() > f32::EPSILON {
            ((v - self.min) / range).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

impl DirectLedEffect for SensorGradient {
    fn tick(&mut self, _t: f32, dt: f32) {
        let (target_level, target_presence) = match self.last_value {
            Some(v) => (self.normalized(v as f32), 1.0),
            None => (self.display_level, 0.0),
        };
        let tau = self.smoothing.clamp(0.0, 1.0) * SENSOR_GRADIENT_MAX_TAU;
        self.display_level = ease_toward(self.display_level, target_level, tau, dt);
        self.presence = ease_toward(self.presence, target_presence, tau, dt);
    }

    fn led_color(&self, p: f32, _p_ring: f32, _nx: f32, _ny: f32, _t: f32) -> LinearColor {
        let c = match self.mode {
            SensorEffectMode::Gradient => {
                lerp_color(self.color_a, self.color_b, self.display_level)
            }
            SensorEffectMode::Meter => {
                if p > self.display_level {
                    RgbColor { r: 0, g: 0, b: 0 }
                } else {
                    lerp_color(self.color_a, self.color_b, p)
                }
            }
        };
        LinearColor {
            r: srgb_to_linear(c.r) * self.presence,
            g: srgb_to_linear(c.g) * self.presence,
            b: srgb_to_linear(c.b) * self.presence,
        }
    }

    fn sensor_id(&self) -> Option<&str> {
        (!self.sensor.is_empty()).then_some(self.sensor.as_str())
    }

    fn set_sensor_value(&mut self, value: Option<f64>) {
        self.last_value = value;
    }
}

/// Default step list: green → orange → red, in °C-ish thresholds.
fn default_steps() -> Vec<ColorStep> {
    vec![
        ColorStep {
            value: 40.0,
            color: RgbColor { r: 0, g: 255, b: 0 },
        },
        ColorStep {
            value: 60.0,
            color: RgbColor {
                r: 255,
                g: 140,
                b: 0,
            },
        },
        ColorStep {
            value: 80.0,
            color: RgbColor { r: 255, g: 0, b: 0 },
        },
    ]
}

/// Snaps a zone to the color of the highest step whose threshold the smoothed
/// sensor reading has reached; readings below every threshold take the first
/// step's color.
struct SensorSteps {
    sensor: String,
    /// Sorted ascending by threshold.
    steps: Vec<ColorStep>,
    smoothing: f32,
    last_value: Option<f64>,
    /// Smoothed reading, in sensor units. `None` until the first reading.
    display_value: Option<f32>,
    presence: f32,
}

impl SensorSteps {
    fn color_at(&self, value: f32) -> RgbColor {
        self.steps
            .iter()
            .rev()
            .find(|s| value >= s.value as f32)
            .or(self.steps.first())
            .map(|s| s.color)
            .unwrap_or(RgbColor { r: 0, g: 0, b: 0 })
    }
}

impl DirectEffect for SensorSteps {
    fn descriptor() -> Animation {
        Animation {
            id: SENSOR_STEPS_EFFECT_ID.to_string(),
            name: "Sensor Steps".to_string(),
            params: vec![
                EffectParamDescriptor {
                    id: "sensor".to_string(),
                    label: "Sensor".to_string(),
                    kind: ParamKind::Sensor,
                    default: EffectParamValue::Str(String::new()),
                },
                EffectParamDescriptor {
                    id: "steps".to_string(),
                    label: "Steps".to_string(),
                    kind: ParamKind::Steps,
                    default: EffectParamValue::Steps(default_steps()),
                },
                EffectParamDescriptor {
                    id: "smoothing".to_string(),
                    label: "Smoothing".to_string(),
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1.0,
                        step: 0.05,
                    },
                    default: EffectParamValue::Float(0.3),
                },
            ],
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Box<dyn DirectLedEffect> {
        let sensor = param_str(params, "sensor", "");
        let mut steps = param_steps(params, "steps", &default_steps());
        steps.sort_by(|a, b| a.value.total_cmp(&b.value));
        let smoothing = param_f64(params, "smoothing", 0.3) as f32;
        Box::new(Self {
            sensor,
            steps,
            smoothing,
            last_value: None,
            display_value: None,
            presence: 0.0,
        })
    }
}

impl DirectLedEffect for SensorSteps {
    fn tick(&mut self, _t: f32, dt: f32) {
        let tau = self.smoothing.clamp(0.0, 1.0) * SENSOR_GRADIENT_MAX_TAU;
        match (self.last_value, self.display_value) {
            // Snap to the first reading instead of easing up from zero.
            (Some(v), None) => self.display_value = Some(v as f32),
            (Some(v), Some(d)) => self.display_value = Some(ease_toward(d, v as f32, tau, dt)),
            (None, _) => {}
        }
        let target_presence = if self.last_value.is_some() { 1.0 } else { 0.0 };
        self.presence = ease_toward(self.presence, target_presence, tau, dt);
    }

    fn led_color(&self, _p: f32, _p_ring: f32, _nx: f32, _ny: f32, _t: f32) -> LinearColor {
        let c = match self.display_value {
            Some(v) => self.color_at(v),
            None => RgbColor { r: 0, g: 0, b: 0 },
        };
        LinearColor {
            r: srgb_to_linear(c.r) * self.presence,
            g: srgb_to_linear(c.g) * self.presence,
            b: srgb_to_linear(c.b) * self.presence,
        }
    }

    fn sensor_id(&self) -> Option<&str> {
        (!self.sensor.is_empty()).then_some(self.sensor.as_str())
    }

    fn set_sensor_value(&mut self, value: Option<f64>) {
        self.last_value = value;
    }
}

struct Designer {
    params: DesignerParams,
}

impl DirectEffect for Designer {
    fn descriptor() -> Animation {
        Animation {
            id: DESIGNER_EFFECT_ID.to_string(),
            name: "Designer".to_string(),
            params: effect_designer::param_descriptors(),
        }
    }

    fn from_params(params: &HashMap<String, EffectParamValue>) -> Box<dyn DirectLedEffect> {
        Box::new(Self {
            params: DesignerParams::from_params(params),
        })
    }
}

impl DirectLedEffect for Designer {
    fn led_color(&self, p: f32, p_ring: f32, nx: f32, ny: f32, t: f32) -> LinearColor {
        let pos = match self.params.ring_scope {
            effect_designer::RingScope::Zone => p,
            effect_designer::RingScope::PerRing => p_ring,
        };
        let (r, g, b) = self.params.color(pos, nx, ny, t);
        LinearColor {
            r: srgb_to_linear((r * 255.0).round() as u8),
            g: srgb_to_linear((g * 255.0).round() as u8),
            b: srgb_to_linear((b * 255.0).round() as u8),
        }
    }
}

struct Off;

impl DirectLedEffect for Off {
    fn led_color(&self, _p: f32, _p_ring: f32, _nx: f32, _ny: f32, _t: f32) -> LinearColor {
        LinearColor {
            r: 0.0,
            g: 0.0,
            b: 0.0,
        }
    }
}

pub fn off_effect() -> Box<dyn DirectLedEffect> {
    Box::new(Off)
}

pub fn build_direct(
    id: &str,
    params: &HashMap<String, EffectParamValue>,
) -> Option<Box<dyn DirectLedEffect>> {
    match id {
        "breathing" => Some(Breathing::from_params(params)),
        "audio_beat" => Some(AudioBeat::from_params(params)),
        "audio_level" => Some(AudioLevel::from_params(params)),
        SENSOR_GRADIENT_EFFECT_ID => Some(SensorGradient::from_params(params)),
        SENSOR_STEPS_EFFECT_ID => Some(SensorSteps::from_params(params)),
        DESIGNER_EFFECT_ID => Some(Designer::from_params(params)),
        _ => None,
    }
}

pub fn direct_descriptors() -> Vec<Animation> {
    vec![
        Breathing::descriptor(),
        AudioBeat::descriptor(),
        AudioLevel::descriptor(),
        SensorGradient::descriptor(),
        SensorSteps::descriptor(),
        Designer::descriptor(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, EffectParamValue)]) -> HashMap<String, EffectParamValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn ease_toward_snaps_instantly_at_zero_tau() {
        assert_eq!(ease_toward(0.0, 1.0, 0.0, 0.016), 1.0);
    }

    #[test]
    fn ease_toward_converges_monotonically_toward_target() {
        let mut v = 0.0;
        for _ in 0..300 {
            let next = ease_toward(v, 1.0, 0.3, 1.0 / 60.0);
            assert!(next >= v, "must not overshoot back down");
            v = next;
        }
        assert!((v - 1.0).abs() < 1e-3, "did not converge: {v}");
    }

    #[test]
    fn build_dispatches_known_ids_only() {
        assert!(build_direct("breathing", &HashMap::new()).is_some());
        assert!(build_direct("audio_beat", &HashMap::new()).is_some());
        assert!(build_direct("audio_level", &HashMap::new()).is_some());
        assert!(build_direct(SENSOR_GRADIENT_EFFECT_ID, &HashMap::new()).is_some());
        assert!(build_direct(SENSOR_STEPS_EFFECT_ID, &HashMap::new()).is_some());
        assert!(build_direct(DESIGNER_EFFECT_ID, &HashMap::new()).is_some());
        assert!(build_direct("rainbow", &HashMap::new()).is_none());
        assert!(build_direct("static_color", &HashMap::new()).is_none());
        assert!(build_direct("nope", &HashMap::new()).is_none());
    }

    #[test]
    fn off_effect_is_black_everywhere() {
        let fx = off_effect();
        for &(p, t) in &[(0.0f32, 0.0f32), (0.5, 3.7), (1.0, 100.0)] {
            let c = fx.led_color(p, p, p, p, t);
            assert_eq!((c.r, c.g, c.b), (0.0, 0.0, 0.0));
        }
    }

    #[test]
    fn descriptors_list_breathing_and_audio_effects() {
        let ids: Vec<String> = direct_descriptors().into_iter().map(|d| d.id).collect();
        assert!(ids.contains(&"breathing".to_string()));
        assert!(ids.contains(&"audio_beat".to_string()));
        assert!(ids.contains(&"audio_level".to_string()));
        assert!(ids.contains(&SENSOR_GRADIENT_EFFECT_ID.to_string()));
        assert!(ids.contains(&SENSOR_STEPS_EFFECT_ID.to_string()));
        assert!(ids.contains(&DESIGNER_EFFECT_ID.to_string()));
        assert!(!ids.contains(&"rainbow".to_string()));
        assert!(!ids.contains(&"static_color".to_string()));
    }

    #[test]
    fn breathing_is_black_at_phase_zero() {
        let fx = build_direct(
            "breathing",
            &params(&[("speed", EffectParamValue::Float(0.5))]),
        )
        .unwrap();
        let c = fx.led_color(0.5, 0.5, 0.5, 0.5, 0.0);
        assert!(c.r == 0.0 && c.g == 0.0 && c.b == 0.0);
    }

    #[test]
    fn breathing_zero_speed_stays_black() {
        let fx = build_direct(
            "breathing",
            &params(&[("speed", EffectParamValue::Float(0.0))]),
        )
        .unwrap();
        for &t in &[0.0, 1.0, 5.0, 100.0] {
            let c = fx.led_color(0.5, 0.5, 0.5, 0.5, t);
            assert!(c.r == 0.0 && c.g == 0.0 && c.b == 0.0, "t={t}");
        }
    }

    #[test]
    fn breathing_is_position_independent() {
        let fx = build_direct(
            "breathing",
            &params(&[("speed", EffectParamValue::Float(0.5))]),
        )
        .unwrap();
        let a = fx.led_color(0.5, 0.5, 0.0, 0.0, 0.7);
        let b = fx.led_color(0.5, 0.5, 0.9, 0.9, 0.7);
        assert_eq!((a.r, a.g, a.b), (b.r, b.g, b.b));
    }

    #[test]
    fn audio_beat_pulse_hits_one_on_loud_flux() {
        let mut fx = AudioBeat::with_frame(RgbColor { r: 255, g: 0, b: 0 }, 0.4, 0.5);
        let frame = SpectrumFrame {
            flux: 1.0,
            seq: 1,
            ..SpectrumFrame::default()
        };
        fx.tick_frame(frame, 0.016);
        assert_eq!(fx.pulse, 1.0);
    }

    #[test]
    fn audio_beat_pulse_decays_below_threshold_within_decay_seconds() {
        let mut fx = AudioBeat::with_frame(RgbColor { r: 255, g: 0, b: 0 }, 0.4, 0.5);
        let loud = SpectrumFrame {
            flux: 1.0,
            seq: 1,
            ..SpectrumFrame::default()
        };
        fx.tick_frame(loud, 0.016);
        assert_eq!(fx.pulse, 1.0);

        let silent = SpectrumFrame {
            flux: 0.0,
            seq: 2,
            ..SpectrumFrame::default()
        };
        let dt = 0.01;
        let mut elapsed = 0.0f32;
        while elapsed < fx.decay {
            fx.tick_frame(silent, dt);
            elapsed += dt;
        }
        assert!(fx.pulse < 0.05, "pulse={} after decay window", fx.pulse);
    }

    #[test]
    fn audio_beat_same_seq_twice_does_not_retrigger() {
        let mut fx = AudioBeat::with_frame(RgbColor { r: 255, g: 0, b: 0 }, 0.4, 0.5);
        let loud = SpectrumFrame {
            flux: 1.0,
            seq: 1,
            ..SpectrumFrame::default()
        };
        fx.tick_frame(loud, 0.016);
        assert_eq!(fx.pulse, 1.0);

        // Advance a bit so pulse decays away from 1.0, then feed the same
        // seq again — it must not re-trigger even though flux is still high.
        fx.tick_frame(loud, 0.05);
        assert!(fx.pulse < 1.0, "same seq must not retrigger the pulse");
    }

    #[test]
    fn audio_beat_black_on_silent_frames() {
        let mut fx = AudioBeat::with_frame(
            RgbColor {
                r: 255,
                g: 40,
                b: 40,
            },
            0.4,
            0.5,
        );
        fx.tick_frame(SpectrumFrame::default(), 0.016);
        let c = fx.led_color(0.5, 0.5, 0.5, 0.5, 0.0);
        assert_eq!((c.r, c.g, c.b), (0.0, 0.0, 0.0));
    }

    #[test]
    fn audio_level_converges_monotonically_to_constant_level() {
        let mut fx = AudioLevel::with_frame(
            RgbColor {
                r: 0,
                g: 200,
                b: 120,
            },
            false,
            0.3,
            1.0,
        );
        let l = 0.8;
        let frame = SpectrumFrame {
            level: l,
            ..SpectrumFrame::default()
        };
        let mut prev = fx.display_level;
        for _ in 0..200 {
            fx.tick_frame(frame, 1.0 / 60.0);
            assert!(fx.display_level >= prev - 1e-6, "display_level decreased");
            prev = fx.display_level;
        }
        assert!(
            (fx.display_level - l).abs() < 1e-3,
            "did not converge: {}",
            fx.display_level
        );
    }

    #[test]
    fn audio_level_output_channel_max_equals_level() {
        let mut fx = AudioLevel::with_frame(
            RgbColor {
                r: 0,
                g: 200,
                b: 120,
            },
            false,
            0.0,
            1.0,
        );
        let l = 0.42;
        let frame = SpectrumFrame {
            level: l,
            ..SpectrumFrame::default()
        };
        fx.tick_frame(frame, 1.0 / 60.0);
        assert!((fx.display_level - l).abs() < 1e-6);

        let c = fx.led_color(0.5, 0.5, 0.0, 0.0, 0.0);
        let expected_max = srgb_to_linear(200) * l; // green channel is the color max
        let actual_max = c.r.max(c.g).max(c.b);
        assert!(
            (actual_max - expected_max).abs() < 1e-5,
            "actual={actual_max} expected={expected_max}"
        );
    }

    #[test]
    fn audio_level_black_on_silent_frames() {
        let mut fx = AudioLevel::with_frame(
            RgbColor {
                r: 0,
                g: 200,
                b: 120,
            },
            false,
            0.0,
            1.0,
        );
        fx.tick_frame(SpectrumFrame::default(), 1.0 / 60.0);
        let c = fx.led_color(0.5, 0.5, 0.5, 0.5, 0.0);
        assert_eq!((c.r, c.g, c.b), (0.0, 0.0, 0.0));
    }

    #[test]
    fn audio_level_sensitivity_amplifies_quiet_input_up_to_clamp() {
        let mut fx = AudioLevel::with_frame(
            RgbColor {
                r: 0,
                g: 200,
                b: 120,
            },
            false,
            0.0,
            2.0,
        );
        let frame = SpectrumFrame {
            level: 0.3,
            ..SpectrumFrame::default()
        };
        fx.tick_frame(frame, 1.0 / 60.0);
        assert!((fx.display_level - 0.6).abs() < 1e-6);

        // Gain pushes the target past 1.0; the result must clamp, not overflow.
        let frame = SpectrumFrame {
            level: 0.9,
            ..SpectrumFrame::default()
        };
        fx.tick_frame(frame, 1.0 / 60.0);
        assert!((fx.display_level - 1.0).abs() < 1e-6);
    }

    #[test]
    fn audio_level_still_responds_at_max_smoothing() {
        // smoothing == 1.0 must not freeze the meter at zero (dead effect).
        let mut fx = AudioLevel::with_frame(
            RgbColor {
                r: 0,
                g: 200,
                b: 120,
            },
            false,
            1.0,
            1.0,
        );
        let frame = SpectrumFrame {
            level: 0.8,
            ..SpectrumFrame::default()
        };
        for _ in 0..10 {
            fx.tick_frame(frame, 1.0 / 60.0);
        }
        assert!(fx.display_level > 0.0, "meter froze at max smoothing");
    }

    #[test]
    fn audio_level_smoothing_is_tick_rate_independent() {
        // Converging over the same wall-clock time with different tick sizes
        // should land at (approximately) the same level.
        let make = || {
            AudioLevel::with_frame(
                RgbColor {
                    r: 0,
                    g: 200,
                    b: 120,
                },
                false,
                0.5,
                1.0,
            )
        };
        let frame = SpectrumFrame {
            level: 0.7,
            ..SpectrumFrame::default()
        };
        let (mut coarse, mut fine) = (make(), make());
        for _ in 0..30 {
            coarse.tick_frame(frame, 1.0 / 30.0);
        }
        for _ in 0..60 {
            fine.tick_frame(frame, 1.0 / 60.0);
        }
        assert!(
            (coarse.display_level - fine.display_level).abs() < 1e-2,
            "coarse={} fine={}",
            coarse.display_level,
            fine.display_level
        );
    }

    fn sensor_gradient_params(sensor: &str, mode: &str) -> HashMap<String, EffectParamValue> {
        params(&[
            ("sensor", EffectParamValue::Str(sensor.to_string())),
            ("mode", EffectParamValue::Str(mode.to_string())),
            ("min", EffectParamValue::Float(20.0)),
            ("max", EffectParamValue::Float(90.0)),
            ("smoothing", EffectParamValue::Float(0.0)),
            (
                "color_a",
                EffectParamValue::Color(RgbColor { r: 0, g: 0, b: 0 }),
            ),
            (
                "color_b",
                EffectParamValue::Color(RgbColor {
                    r: 255,
                    g: 255,
                    b: 255,
                }),
            ),
        ])
    }

    #[test]
    fn sensor_gradient_reports_no_sensor_id_when_param_empty() {
        let fx = build_direct(SENSOR_GRADIENT_EFFECT_ID, &HashMap::new()).unwrap();
        assert!(fx.sensor_id().is_none());
    }

    #[test]
    fn sensor_gradient_reports_configured_sensor_id() {
        let fx = build_direct(
            SENSOR_GRADIENT_EFFECT_ID,
            &sensor_gradient_params("temp1", "gradient"),
        )
        .unwrap();
        assert_eq!(fx.sensor_id(), Some("temp1"));
    }

    #[test]
    fn sensor_gradient_reaches_endpoints_at_min_and_max() {
        // smoothing=0 → instant convergence (alpha=1) each tick.
        let mut fx = build_direct(
            SENSOR_GRADIENT_EFFECT_ID,
            &sensor_gradient_params("temp1", "gradient"),
        )
        .unwrap();
        fx.set_sensor_value(Some(20.0));
        fx.tick(0.0, 1.0);
        let c = fx.led_color(0.5, 0.5, 0.5, 0.5, 0.0);
        assert!(
            c.r < 0.01 && c.g < 0.01 && c.b < 0.01,
            "expected black at min, got {c:?}"
        );

        fx.set_sensor_value(Some(90.0));
        fx.tick(0.0, 1.0);
        let c = fx.led_color(0.5, 0.5, 0.5, 0.5, 0.0);
        assert!(
            c.r > 0.9 && c.g > 0.9 && c.b > 0.9,
            "expected white at max, got {c:?}"
        );
    }

    #[test]
    fn sensor_gradient_missing_sensor_fades_to_black() {
        let mut fx = build_direct(
            SENSOR_GRADIENT_EFFECT_ID,
            &sensor_gradient_params("temp1", "gradient"),
        )
        .unwrap();
        fx.set_sensor_value(Some(90.0));
        fx.tick(0.0, 1.0);
        let lit = fx.led_color(0.5, 0.5, 0.5, 0.5, 0.0);
        assert!(lit.r > 0.9, "sanity: lit before going missing");

        fx.set_sensor_value(None);
        fx.tick(0.0, 1.0);
        let c = fx.led_color(0.5, 0.5, 0.5, 0.5, 0.0);
        assert!(
            c.r < 0.01 && c.g < 0.01 && c.b < 0.01,
            "expected black once the sensor disappears, got {c:?}"
        );
    }

    #[test]
    fn sensor_gradient_min_equals_max_does_not_nan() {
        let mut fx = build_direct(
            SENSOR_GRADIENT_EFFECT_ID,
            &params(&[
                ("sensor", EffectParamValue::Str("temp1".to_string())),
                ("min", EffectParamValue::Float(50.0)),
                ("max", EffectParamValue::Float(50.0)),
                ("smoothing", EffectParamValue::Float(0.0)),
            ]),
        )
        .unwrap();
        fx.set_sensor_value(Some(50.0));
        fx.tick(0.0, 1.0);
        let c = fx.led_color(0.5, 0.5, 0.5, 0.5, 0.0);
        for ch in [c.r, c.g, c.b] {
            assert!(ch.is_finite(), "channel went non-finite: {ch}");
        }
    }

    #[test]
    fn sensor_gradient_meter_mode_lights_up_to_level_only() {
        let mut fx = build_direct(
            SENSOR_GRADIENT_EFFECT_ID,
            &sensor_gradient_params("temp1", "meter"),
        )
        .unwrap();
        // min=20 max=90, value=55 → level = (55-20)/70 = 0.5.
        fx.set_sensor_value(Some(55.0));
        fx.tick(0.0, 1.0);

        let below = fx.led_color(0.2, 0.2, 0.5, 0.5, 0.0);
        assert!(
            below.r > 0.0 || below.g > 0.0 || below.b > 0.0,
            "position below the level must be lit"
        );

        let above = fx.led_color(0.8, 0.8, 0.5, 0.5, 0.0);
        assert_eq!(
            (above.r, above.g, above.b),
            (0.0, 0.0, 0.0),
            "position past the level must be dark"
        );
    }

    fn steps(pairs: &[(f64, RgbColor)]) -> Vec<ColorStep> {
        pairs
            .iter()
            .map(|&(value, color)| ColorStep { value, color })
            .collect()
    }

    const RED: RgbColor = RgbColor { r: 255, g: 0, b: 0 };
    const GREEN: RgbColor = RgbColor { r: 0, g: 255, b: 0 };
    const BLUE: RgbColor = RgbColor { r: 0, g: 0, b: 255 };

    fn sensor_steps_params(pairs: &[(f64, RgbColor)]) -> HashMap<String, EffectParamValue> {
        params(&[
            ("sensor", EffectParamValue::Str("temp1".to_string())),
            ("steps", EffectParamValue::Steps(steps(pairs))),
            ("smoothing", EffectParamValue::Float(0.0)),
        ])
    }

    fn led_at(fx: &mut Box<dyn DirectLedEffect>, v: f64) -> LinearColor {
        fx.set_sensor_value(Some(v));
        fx.tick(0.0, 1.0);
        fx.led_color(0.5, 0.5, 0.5, 0.5, 0.0)
    }

    #[test]
    fn sensor_steps_picks_color_of_highest_reached_threshold() {
        let mut fx = build_direct(
            SENSOR_STEPS_EFFECT_ID,
            &sensor_steps_params(&[(40.0, GREEN), (60.0, BLUE), (80.0, RED)]),
        )
        .unwrap();
        let c = led_at(&mut fx, 50.0);
        assert!(c.g > 0.9 && c.r < 0.01 && c.b < 0.01, "50 → green: {c:?}");
        let c = led_at(&mut fx, 60.0);
        assert!(c.b > 0.9 && c.r < 0.01 && c.g < 0.01, "60 → blue: {c:?}");
        let c = led_at(&mut fx, 99.0);
        assert!(c.r > 0.9 && c.g < 0.01 && c.b < 0.01, "99 → red: {c:?}");
    }

    #[test]
    fn sensor_steps_below_first_threshold_uses_first_color() {
        let mut fx = build_direct(
            SENSOR_STEPS_EFFECT_ID,
            &sensor_steps_params(&[(40.0, GREEN), (80.0, RED)]),
        )
        .unwrap();
        let c = led_at(&mut fx, 10.0);
        assert!(
            c.g > 0.9 && c.r < 0.01,
            "below all thresholds → green: {c:?}"
        );
    }

    #[test]
    fn sensor_steps_sorts_unordered_step_lists() {
        let mut fx = build_direct(
            SENSOR_STEPS_EFFECT_ID,
            &sensor_steps_params(&[(80.0, RED), (40.0, GREEN)]),
        )
        .unwrap();
        let c = led_at(&mut fx, 50.0);
        assert!(
            c.g > 0.9 && c.r < 0.01,
            "50 → green despite input order: {c:?}"
        );
    }

    #[test]
    fn sensor_steps_missing_sensor_fades_to_black() {
        let mut fx = build_direct(
            SENSOR_STEPS_EFFECT_ID,
            &sensor_steps_params(&[(40.0, GREEN)]),
        )
        .unwrap();
        let lit = led_at(&mut fx, 50.0);
        assert!(lit.g > 0.9, "sanity: lit before going missing");

        fx.set_sensor_value(None);
        fx.tick(0.0, 1.0);
        let c = fx.led_color(0.5, 0.5, 0.5, 0.5, 0.0);
        assert!(c.r < 0.01 && c.g < 0.01 && c.b < 0.01, "faded out: {c:?}");
    }

    #[test]
    fn sensor_steps_is_position_independent() {
        let mut fx = build_direct(
            SENSOR_STEPS_EFFECT_ID,
            &sensor_steps_params(&[(40.0, GREEN)]),
        )
        .unwrap();
        fx.set_sensor_value(Some(60.0));
        fx.tick(0.0, 1.0);
        let a = fx.led_color(0.1, 0.1, 0.5, 0.5, 0.0);
        let b = fx.led_color(0.9, 0.9, 0.5, 0.5, 0.0);
        assert_eq!((a.r, a.g, a.b), (b.r, b.g, b.b));
    }

    #[test]
    fn sensor_steps_reports_configured_sensor_id() {
        let fx = build_direct(SENSOR_STEPS_EFFECT_ID, &sensor_steps_params(&[])).unwrap();
        assert_eq!(fx.sensor_id(), Some("temp1"));
        let fx = build_direct(SENSOR_STEPS_EFFECT_ID, &HashMap::new()).unwrap();
        assert!(fx.sensor_id().is_none());
    }

    #[test]
    fn sensor_gradient_smoothing_is_tick_rate_independent() {
        let make = || {
            build_direct(
                SENSOR_GRADIENT_EFFECT_ID,
                &params(&[
                    ("sensor", EffectParamValue::Str("temp1".to_string())),
                    ("min", EffectParamValue::Float(0.0)),
                    ("max", EffectParamValue::Float(100.0)),
                    ("smoothing", EffectParamValue::Float(0.5)),
                ]),
            )
            .unwrap()
        };
        let (mut coarse, mut fine) = (make(), make());
        coarse.set_sensor_value(Some(70.0));
        fine.set_sensor_value(Some(70.0));
        for _ in 0..30 {
            coarse.tick(0.0, 1.0 / 30.0);
        }
        for _ in 0..60 {
            fine.tick(0.0, 1.0 / 60.0);
        }
        let a = coarse.led_color(0.5, 0.5, 0.5, 0.5, 0.0);
        let b = fine.led_color(0.5, 0.5, 0.5, 0.5, 0.0);
        assert!(
            (a.r - b.r).abs() < 1e-2,
            "coarse and fine ticking should converge to about the same color: {a:?} vs {b:?}"
        );
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn direct_output_stays_in_unit_gamut(
            id in prop::sample::select(vec![
                "breathing", "audio_beat", "audio_level", SENSOR_GRADIENT_EFFECT_ID,
                SENSOR_STEPS_EFFECT_ID, DESIGNER_EFFECT_ID,
            ]),
            nx in 0.0f32..1.0,
            ny in 0.0f32..1.0,
            t in 0.0f32..1000.0,
        ) {
            let fx = build_direct(id, &std::collections::HashMap::new()).unwrap();
            let c = fx.led_color(nx, nx, nx, ny, t);
            for ch in [c.r, c.g, c.b] {
                prop_assert!((0.0..=1.0).contains(&ch), "channel {ch} out of gamut");
            }
        }

        #[test]
        fn sensor_gradient_stays_in_gamut_after_tick(
            min in -50.0f32..50.0,
            max in -50.0f32..200.0,
            value in -100.0f64..300.0,
            mode in prop::sample::select(vec!["gradient", "meter"]),
            p in 0.0f32..1.0,
            dt in 0.001f32..2.0,
        ) {
            let mut params = HashMap::new();
            params.insert("sensor".to_string(), EffectParamValue::Str("s".to_string()));
            params.insert("mode".to_string(), EffectParamValue::Str(mode.to_string()));
            params.insert("min".to_string(), EffectParamValue::Float(min as f64));
            params.insert("max".to_string(), EffectParamValue::Float(max as f64));
            let mut fx = build_direct(SENSOR_GRADIENT_EFFECT_ID, &params).unwrap();
            fx.set_sensor_value(Some(value));
            fx.tick(0.0, dt);
            let c = fx.led_color(p, p, 0.5, 0.5, 0.0);
            for ch in [c.r, c.g, c.b] {
                prop_assert!(ch.is_finite() && (0.0..=1.0).contains(&ch), "channel {ch} out of gamut");
            }
        }

        #[test]
        fn sensor_steps_stays_in_gamut_for_any_step_list(
            thresholds in prop::collection::vec(-100.0f64..300.0, 0..6),
            value in -100.0f64..300.0,
            p in 0.0f32..1.0,
            dt in 0.001f32..2.0,
        ) {
            let steps: Vec<ColorStep> = thresholds
                .iter()
                .enumerate()
                .map(|(i, &value)| ColorStep {
                    value,
                    color: RgbColor { r: (i * 50) as u8, g: 255 - (i * 40) as u8, b: 128 },
                })
                .collect();
            let mut params = HashMap::new();
            params.insert("sensor".to_string(), EffectParamValue::Str("s".to_string()));
            params.insert("steps".to_string(), EffectParamValue::Steps(steps));
            let mut fx = build_direct(SENSOR_STEPS_EFFECT_ID, &params).unwrap();
            fx.set_sensor_value(Some(value));
            fx.tick(0.0, dt);
            let c = fx.led_color(p, p, 0.5, 0.5, 0.0);
            for ch in [c.r, c.g, c.b] {
                prop_assert!(ch.is_finite() && (0.0..=1.0).contains(&ch), "channel {ch} out of gamut");
            }
        }
    }
}
