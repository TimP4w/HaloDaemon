// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedFrameEntry {
    pub device_id: crate::ids::DeviceId,
    pub channel_id: String,
    pub led_id: u32,
    pub color: RgbColor,
}

/// Gaps in the monotonically increasing `frame_id` are frames the daemon dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanvasFrame {
    pub frame_id: u64,
    pub timestamp_ms: u64,
    pub canvas_srgb_b64: String,
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub led_colors: Vec<LedFrameEntry>,
}

/// Fraction of the canvas.
pub const DEFAULT_ZONE_SIZE: f32 = 0.15;

/// Pixels.
pub const DEFAULT_SAMPLE_RADIUS: f32 = 3.0;

fn default_zone_size() -> f32 {
    DEFAULT_ZONE_SIZE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SamplingMode {
    #[default]
    Spatial,
    /// LEDs are laid along the zone rect by chain index.
    Unrolled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PlacedZone {
    pub device_id: crate::ids::DeviceId,
    pub channel_id: String,
    pub x: f32,
    pub y: f32,
    #[serde(default = "default_zone_size")]
    pub w: f32,
    #[serde(default = "default_zone_size")]
    pub h: f32,
    #[serde(default)]
    pub rotation: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
    #[serde(default)]
    pub sampling_mode: SamplingMode,
}

/// The `available_*` catalogs are only filled in when sent to the GUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CanvasState {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub available_effects: Vec<Animation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub available_direct_effects: Vec<Animation>,
    /// Saved Designer effects, shown alongside the built-ins.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub custom_direct_effects: Vec<EffectDef>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub effects: HashMap<String, EffectDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_effect: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub placed_zones: Vec<PlacedZone>,
    pub sample_radius: f32,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            available_effects: Vec::new(),
            available_direct_effects: Vec::new(),
            custom_direct_effects: Vec::new(),
            effects: HashMap::new(),
            default_effect: None,
            placed_zones: Vec::new(),
            sample_radius: DEFAULT_SAMPLE_RADIUS,
        }
    }
}

pub const MAX_PLACED_ZONES: usize = 256;

pub const MAX_CANVAS_EFFECTS: usize = 256;

pub const MAX_EFFECT_PARAMS: usize = 64;

pub const MAX_LIGHTING_TARGET_IDS: usize = 512;

impl PlacedZone {
    /// `serde_json` rejects non-finite floats, and the sampler must never see NaN.
    pub fn sanitize(&mut self) {
        self.x = finite_or(self.x, 0.0);
        self.y = finite_or(self.y, 0.0);
        self.w = finite_or(self.w, DEFAULT_ZONE_SIZE).clamp(f32::EPSILON, 4.0);
        self.h = finite_or(self.h, DEFAULT_ZONE_SIZE).clamp(f32::EPSILON, 4.0);
        self.rotation = finite_or(self.rotation, 0.0);
    }
}

impl CanvasState {
    /// Applied on config load and before persistence.
    pub fn sanitize(&mut self) {
        for z in &mut self.placed_zones {
            z.sanitize();
        }
        self.placed_zones.truncate(MAX_PLACED_ZONES);
        self.sample_radius = finite_or(self.sample_radius, DEFAULT_SAMPLE_RADIUS).clamp(0.5, 64.0);
    }
}

/// Persisted under `config.yaml`'s `rgb` key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RgbConfig {
    pub canvas_enabled: bool,
    pub canvas_fps: u32,
}

impl Default for RgbConfig {
    fn default() -> Self {
        Self {
            canvas_enabled: true,
            canvas_fps: 20,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LightingOverviewState {
    #[serde(default)]
    pub canvas: CanvasState,
    #[serde(default)]
    pub targets: LightingTargets,
    #[serde(default)]
    pub config: RgbConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub type LedId = u32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedPosition {
    pub id: LedId,
    pub x: f32,
    pub y: f32,
}

/// UI rendering hint, unrelated to [`StandardLayout`](crate::keyboard::StandardLayout); a new form factor must update both.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardFormFactor {
    FullSize,
    TKL,
    Compact75,
    Compact65,
    Compact60,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardLayout {
    CH,
    IT,
    US,
    DE,
    FR,
    UK,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ZoneTopology {
    Ring,
    Rings {
        count: u8,
    },
    Linear,
    Grid,
    Keyboard {
        form_factor: KeyboardFormFactor,
        layout: KeyboardLayout,
    },
}

/// Channel ids are only unique within one capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Lighting,
    Cooling,
}

impl ChannelKind {
    /// Persisted; must stay stable.
    pub fn key(&self, channel_id: &str) -> String {
        match self {
            Self::Lighting => format!("lighting:{channel_id}"),
            Self::Cooling => format!("cooling:{channel_id}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightingChannel {
    pub id: String,
    pub name: String,
    pub topology: ZoneTopology,
    pub leds: Vec<LedPosition>,
    #[serde(default)]
    pub color_order: ColorOrder,
    #[serde(default)]
    pub division: LightingDivision,
    /// `Hidden` drops the channel from the UI; `Disabled` also stops the engines driving it.
    #[serde(default)]
    pub visibility: VisibilityState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParamKind {
    Range {
        min: f64,
        max: f64,
        step: f64,
    },
    /// Input field rather than a slider; `min`/`max` only clamp.
    Number {
        min: f64,
        max: f64,
    },
    Enum {
        options: Vec<String>,
    },
    Color,
    Boolean,
    /// Free-text entry. Value is an `EffectParamValue::Str`.
    Text,
    /// Sensor picker. Value is an `EffectParamValue::Str` holding the sensor id.
    Sensor,
    /// Value is an `EffectParamValue::Steps`.
    Steps,
    Gradient,
    /// Value is an `EffectParamValue::Str` filename relative to the LCD images dir.
    Image,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectParamDescriptor {
    pub id: String,
    pub label: String,
    pub kind: ParamKind,
    pub default: EffectParamValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeEffect {
    pub id: String,
    pub name: String,
    pub params: Vec<EffectParamDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Animation {
    pub id: String,
    pub name: String,
    pub params: Vec<EffectParamDescriptor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ColorStep {
    pub value: f64,
    pub color: RgbColor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EffectParamValue {
    Float(f64),
    Str(String),
    Color(RgbColor),
    Bool(bool),
    /// Serializes as a JSON array, so it stays unambiguous under `untagged`.
    Steps(Vec<ColorStep>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum LightingState {
    Static {
        color: RgbColor,
    },
    // LED IDs use String keys: JSON object keys are always strings.
    PerLed {
        channels: HashMap<String, HashMap<String, RgbColor>>,
    },
    NativeEffect {
        id: String,
        params: HashMap<String, EffectParamValue>,
    },
    DirectEffect {
        id: String,
        params: HashMap<String, EffectParamValue>,
    },
    /// The frontend disables other controls and doesn't touch state in this mode.
    Engine,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectDef {
    pub effect_id: String,
    /// `None` falls back to the instance id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub params: HashMap<String, EffectParamValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightingDescriptor {
    pub channels: Vec<LightingChannel>,
    pub native_effects: Vec<NativeEffect>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightingStatus {
    pub descriptor: LightingDescriptor,
    pub state: Option<LightingState>,
    /// Zones absent from the map use the identity transform.
    #[serde(default)]
    pub channel_transforms: HashMap<String, ZoneContentTransform>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ColorOrder {
    #[default]
    Rgb,
    Rbg,
    Grb,
    Gbr,
    Brg,
    Bgr,
}

impl ColorOrder {
    pub fn encode(self, color: RgbColor) -> [u8; 3] {
        match self {
            Self::Rgb => [color.r, color.g, color.b],
            Self::Rbg => [color.r, color.b, color.g],
            Self::Grb => [color.g, color.r, color.b],
            Self::Gbr => [color.g, color.b, color.r],
            Self::Brg => [color.b, color.r, color.g],
            Self::Bgr => [color.b, color.g, color.r],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LightingDivision {
    #[default]
    Indivisible,
    Divisible {
        max_leds: u32,
        segments: Vec<LightingSegmentInfo>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightingSegmentInfo {
    pub device_id: crate::ids::DeviceId,
    pub channel_id: String,
    pub name: String,
    pub topology: ZoneTopology,
    pub led_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_order: Option<ColorOrder>,
    pub locked: bool,
}
