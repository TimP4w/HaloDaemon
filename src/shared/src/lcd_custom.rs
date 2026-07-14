//! Wire contract for the data-driven "custom" LCD template: a widget list plus
//! a screen style, edited by the GUI's LCD editor and rendered by the daemon's
//! `CustomTemplate`. Serialization and the shared per-widget param schema —
//! no rendering logic here.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::types::{
    EffectParamDescriptor, EffectParamValue, ParamKind, RgbColor, SensorUnit, MAX_EFFECT_PARAMS,
};

/// Param key the custom template's `widgets_json` is stored under in
/// `EffectDef`/`RgbState`-style parameter maps.
pub const WIDGETS_JSON_PARAM: &str = "widgets_json";

/// Sentinel `filename` for an Image widget that renders the bundled HaloDaemon
/// logo ([`LOGO_SVG`]) rather than a library file. The `@` prefix can't collide
/// with a real upload (`validate_image_filename` rejects it).
pub const LOGO_IMAGE: &str = "@halo-logo";

/// Templates are supplied over IPC and kept in profiles. Keep their work and
/// persisted size bounded independently of the generic effect-param limit.
pub const MAX_LCD_WIDGETS: usize = 64;
pub const MAX_WIDGET_ID_BYTES: usize = 64;
pub const MAX_WIDGET_TEXT_BYTES: usize = 4096;

/// The bundled HaloDaemon logo, rasterized by the daemon and GUI for the
/// [`LOGO_IMAGE`] sentinel.
pub const LOGO_SVG: &[u8] = include_bytes!("../../../assets/icon.svg");

/// A default custom template: a single centered Logo widget.
pub fn default_with_logo() -> CustomTemplateDef {
    let mut params = HashMap::new();
    params.insert("show_img".to_string(), EffectParamValue::Bool(true));
    params.insert("show_text".to_string(), EffectParamValue::Bool(true));
    CustomTemplateDef {
        widgets: vec![WidgetDef {
            id: "w1".to_string(),
            widget_type: WidgetType::Logo,
            x: 0.5,
            y: 0.5,
            scale: 2.0,
            rotation: 0.0,
            color: None,
            font: None,
            params,
        }],
        style: ScreenStyle::default(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomTemplateDef {
    pub widgets: Vec<WidgetDef>,
    pub style: ScreenStyle,
}

/// One widget's CONTENT rendered unrotated at its current pixel size, with a
/// transparent background and its per-widget opacity baked into the alpha
/// (straight-alpha RGBA, base64). The GUI editor uploads this as a texture and
/// applies the widget's position, scale, and rotation itself — the daemon is the
/// single source of truth for the pixels, so the editor preview matches the device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WidgetSprite {
    pub id: String,
    /// The owning widget's content signature; the GUI caches textures by
    /// `(id, signature)` and skips re-uploading unchanged widgets.
    pub signature: u64,
    pub rgba_b64: String,
    pub w: u32,
    pub h: u32,
}

/// The widget sprites for a `CustomTemplateDef`, rendered by the daemon
/// against a device's canvas size for the LCD editor's static preview.
/// `sprites` carries only the widgets whose content changed since the
/// requester's `known` signatures (all of them when `known` was empty);
/// `signatures` carries the current (id, signature) pair for every widget so
/// the requester can tell which cached sprites are still valid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LcdEditorRender {
    pub device_id: String,
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub sprites: Vec<WidgetSprite>,
    pub signatures: Vec<(String, u64)>,
}

/// A blank custom screen shows the bundled logo, so an unconfigured device
/// isn't just black. The user can move, resize, or delete it.
impl Default for CustomTemplateDef {
    fn default() -> Self {
        default_with_logo()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenStyle {
    pub accent: RgbColor,
    pub background: BgKind,
    pub font: FontKind,
}

impl Default for ScreenStyle {
    fn default() -> Self {
        Self {
            accent: RgbColor {
                r: 0,
                g: 200,
                b: 220,
            },
            background: BgKind::Flow,
            font: FontKind::Sans,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BgKind {
    Flow,
    Solid,
    Grid,
    Glow,
    Image { filename: String, dim: f64 },
}

/// A bundled typeface the LCD renderer can draw text with. The wire keys stay
/// stable (`sans`/`mono` predate the others) so saved templates keep working;
/// each maps to a concrete font file in `assets/fonts/` on both the daemon
/// (ab_glyph) and the GUI (egui family).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FontKind {
    /// Noto Sans (default proportional face).
    Sans,
    /// JetBrains Mono.
    Mono,
    /// Inter Tight.
    Inter,
}

impl FontKind {
    /// Every selectable font, in picker order.
    pub const ALL: [FontKind; 3] = [FontKind::Sans, FontKind::Mono, FontKind::Inter];

    /// The typeface's real name, shown in the editor's font picker (a proper
    /// noun — not localized).
    pub fn label(self) -> &'static str {
        match self {
            FontKind::Sans => "Noto Sans",
            FontKind::Mono => "JetBrains Mono",
            FontKind::Inter => "Inter Tight",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetType {
    Clock,
    Date,
    Sensor,
    Text,
    Image,
    /// Overlays the engine's live frame counter — a diagnostic aid.
    Debug,
    AudioSpectrum,
    AudioLevel,
    NowPlaying,
    /// Bundled logo image + "halodaemon" text, each toggleable.
    Logo,
    /// A geometric primitive (circle/square/rectangle/triangle/line), filled or
    /// outline-only, driven by the `shape` and `filled` params.
    Shape,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WidgetDef {
    pub id: String,
    pub widget_type: WidgetType,
    /// Normalized center position, [0, 1].
    pub x: f32,
    pub y: f32,
    /// Size multiplier, 0.6..=3.0.
    pub scale: f32,
    #[serde(default)]
    pub rotation: f32,
    /// `None` falls back to `ScreenStyle::accent`.
    pub color: Option<RgbColor>,
    /// `None` falls back to `ScreenStyle::font`.
    pub font: Option<FontKind>,
    /// Widget-specific params: variant, sensor, label, min, max, text, filename.
    pub params: HashMap<String, EffectParamValue>,
}

// Shared param accessors
//
// Canonical param fallbacks shared by daemon and GUI.

pub fn param_str(w: &WidgetDef, key: &str) -> String {
    match w.params.get(key) {
        Some(EffectParamValue::Str(s)) => s.clone(),
        _ => String::new(),
    }
}

/// The `variant` param, or `default` when absent or empty.
pub fn param_variant(w: &WidgetDef, default: &str) -> String {
    match w.params.get("variant") {
        Some(EffectParamValue::Str(s)) if !s.is_empty() => s.clone(),
        _ => default.to_string(),
    }
}

pub fn param_color(w: &WidgetDef, key: &str, default: RgbColor) -> RgbColor {
    match w.params.get(key) {
        Some(EffectParamValue::Color(c)) => *c,
        _ => default,
    }
}

pub fn param_bool(w: &WidgetDef, key: &str, default: bool) -> bool {
    match w.params.get(key) {
        Some(EffectParamValue::Bool(b)) => *b,
        _ => default,
    }
}

pub fn param_f64(w: &WidgetDef, key: &str, default: f64) -> f64 {
    match w.params.get(key) {
        Some(EffectParamValue::Float(f)) => *f,
        _ => default,
    }
}

/// Independent vertical scale, stored in the `scale_y` param. Falls back to the
/// (uniform) `scale` field for widgets saved before 2D resize — and for widget
/// types that stay proportional. `scale` remains the horizontal scale.
pub fn scale_y(w: &WidgetDef) -> f32 {
    match w.params.get("scale_y") {
        Some(EffectParamValue::Float(f)) => *f as f32,
        _ => w.scale,
    }
}

/// Widget types whose box stretches independently on each axis (2D resize);
/// everything else scales uniformly.
pub fn is_box_widget(t: WidgetType) -> bool {
    matches!(
        t,
        WidgetType::Image | WidgetType::Shape | WidgetType::AudioSpectrum
    )
}

/// Format a sensor reading, suffixed with the sensor's own unit (e.g. `42°`,
/// `73%`, `4500 MHz`) rather than assuming °C.
pub fn format_value(value: f64, unit: &SensorUnit) -> String {
    let suffix = match unit {
        SensorUnit::Celsius => "°",
        SensorUnit::Fahrenheit => "°F",
        SensorUnit::Percent => "%",
        SensorUnit::Megahertz => " MHz",
        SensorUnit::Hours => " h",
        SensorUnit::Rpm => " RPM",
    };
    format!("{value:.0}{suffix}")
}

/// Unfilled part of a progress bar when no `track` color is set — a dark
/// neutral that reads on any background.
pub const DEFAULT_TRACK: RgbColor = RgbColor {
    r: 0x2e,
    g: 0x33,
    b: 0x40,
};

/// Default sensor value-text color (bright).
pub const DEFAULT_VALUE_COLOR: RgbColor = RgbColor {
    r: 0xe6,
    g: 0xee,
    b: 0xf5,
};
/// Default sensor label-text color (muted).
pub const DEFAULT_LABEL_COLOR: RgbColor = RgbColor {
    r: 0x8a,
    g: 0x93,
    b: 0xa5,
};
/// Default high-value color for a value-based gradient fill (red).
pub const DEFAULT_GRADIENT_HIGH: RgbColor = RgbColor {
    r: 0xe6,
    g: 0x46,
    b: 0x40,
};
/// Spectrum-specific gradient high (brand purple), more visible on small LCDs
/// than the sensor default red.
pub const SPECTRUM_GRADIENT_HIGH: RgbColor = RgbColor {
    r: 0x9b,
    g: 0x7f,
    b: 0xe0,
};

/// Linear interpolation between two colors, `t` clamped to `[0, 1]`.
pub fn lerp_rgb(a: RgbColor, b: RgbColor, t: f32) -> RgbColor {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    RgbColor {
        r: mix(a.r, b.r),
        g: mix(a.g, b.g),
        b: mix(a.b, b.b),
    }
}

/// Fill color for a sensor gauge/meter: the base `fill`, or a value-based blend
/// from `fill` (low) to `high` (e.g. green→red) when `gradient` is on. Shared so
/// the daemon and editor agree.
pub fn sensor_fill_color(fill: RgbColor, high: RgbColor, gradient: bool, frac: f32) -> RgbColor {
    if gradient {
        lerp_rgb(fill, high, frac)
    } else {
        fill
    }
}

/// Validate every widget's geometry and referenced assets: finite coords, an
/// on-canvas center, a positive/bounded scale, a bounded param map, and valid
/// image filenames. Pure — the GUI uses it for feedback, the daemon enforces it
/// on save, load, and preview.
pub fn validate_widgets(def: &CustomTemplateDef) -> Result<(), String> {
    let scale_ok = |s: f32| s.is_finite() && s > 0.0 && s <= 100.0;
    if def.widgets.len() > MAX_LCD_WIDGETS {
        return Err(format!("template has more than {MAX_LCD_WIDGETS} widgets"));
    }
    let mut ids = std::collections::HashSet::with_capacity(def.widgets.len());
    for w in &def.widgets {
        if w.id.is_empty()
            || w.id.len() > MAX_WIDGET_ID_BYTES
            || w.id.contains('\0')
            || !ids.insert(&w.id)
        {
            return Err(format!("widget '{}' has an invalid or duplicate id", w.id));
        }
        if w.widget_type == WidgetType::Unknown {
            return Err(format!("widget '{}' has an unknown widget type", w.id));
        }
        if !w.x.is_finite()
            || !(0.0..=1.0).contains(&w.x)
            || !w.y.is_finite()
            || !(0.0..=1.0).contains(&w.y)
        {
            return Err(format!("widget '{}' position out of bounds", w.id));
        }
        if !scale_ok(w.scale) {
            return Err(format!("widget '{}' scale out of range", w.id));
        }
        if !w.rotation.is_finite() {
            return Err(format!("widget '{}' rotation is not finite", w.id));
        }
        if w.params.len() > MAX_EFFECT_PARAMS {
            return Err(format!("widget '{}' has too many params", w.id));
        }
        validate_widget_params(w, scale_ok)?;
    }
    if let BgKind::Image { filename, dim } = &def.style.background {
        if !filename.is_empty() && crate::types::validate_image_filename(filename).is_err() {
            return Err("background image filename is invalid".to_string());
        }
        if !dim.is_finite() || !(0.0..=100.0).contains(dim) {
            return Err("background image dim must be between 0 and 100".to_string());
        }
    }
    Ok(())
}

fn validate_widget_params(w: &WidgetDef, scale_ok: impl Fn(f32) -> bool) -> Result<(), String> {
    let schema = widget_schema(w.widget_type);
    for (key, value) in &w.params {
        if key.is_empty() || key.len() > MAX_WIDGET_ID_BYTES || key.contains('\0') {
            return Err(format!("widget '{}' has an invalid parameter key", w.id));
        }
        if key == "scale_y" {
            if !is_box_widget(w.widget_type) {
                return Err(format!("widget '{}' cannot use scale_y", w.id));
            }
            match value {
                EffectParamValue::Float(scale_y) if scale_ok(*scale_y as f32) => continue,
                _ => return Err(format!("widget '{}' scale_y out of range", w.id)),
            }
        }
        let Some(desc) = schema.iter().find(|desc| desc.id == *key) else {
            return Err(format!("widget '{}' has unknown parameter '{key}'", w.id));
        };
        validate_widget_param_value(w, desc, value)?;
    }
    if w.widget_type == WidgetType::Sensor {
        let min = w
            .params
            .get("min")
            .and_then(|v| match v {
                EffectParamValue::Float(v) => Some(*v),
                _ => None,
            })
            .unwrap_or(0.0);
        let max = w
            .params
            .get("max")
            .and_then(|v| match v {
                EffectParamValue::Float(v) => Some(*v),
                _ => None,
            })
            .unwrap_or(100.0);
        if min >= max {
            return Err(format!(
                "widget '{}' sensor min must be less than max",
                w.id
            ));
        }
    }
    Ok(())
}

fn validate_widget_param_value(
    widget: &WidgetDef,
    desc: &EffectParamDescriptor,
    value: &EffectParamValue,
) -> Result<(), String> {
    let err = |detail: &str| {
        Err(format!(
            "widget '{}' parameter '{}' {detail}",
            widget.id, desc.id
        ))
    };
    match (&desc.kind, value) {
        (ParamKind::Range { min, max, step }, EffectParamValue::Float(value)) => {
            if !value.is_finite() || value < min || value > max {
                return err("is out of range");
            }
            if *step > 0.0 && (((value - min) / step).round() * step - (value - min)).abs() > 1e-8 {
                return err("does not align to its step");
            }
        }
        (ParamKind::Number { min, max }, EffectParamValue::Float(value))
            if value.is_finite() && value >= min && value <= max => {}
        (ParamKind::Enum { options }, EffectParamValue::Str(value))
            if options.iter().any(|option| option == value) => {}
        (ParamKind::Color, EffectParamValue::Color(_))
        | (ParamKind::Boolean, EffectParamValue::Bool(_)) => {}
        (ParamKind::Text | ParamKind::Sensor, EffectParamValue::Str(value)) => {
            if value.len() > MAX_WIDGET_TEXT_BYTES || value.contains('\0') {
                return err("contains invalid text");
            }
        }
        (ParamKind::Image, EffectParamValue::Str(value)) => {
            if value != LOGO_IMAGE
                && !value.is_empty()
                && crate::types::validate_image_filename(value).is_err()
            {
                return err("has an invalid image filename");
            }
        }
        (ParamKind::Steps, EffectParamValue::Steps(steps))
            if steps.len() <= MAX_EFFECT_PARAMS
                && steps.iter().all(|step| step.value.is_finite()) => {}
        _ => return err("has the wrong type or value"),
    }
    Ok(())
}

/// Full param schema for a widget type: the type-specific rows plus the
/// universal `opacity` control appended to every real widget. This is the
/// shared schema the GUI editor renders its inspector from and seeds new
/// widgets with, and the daemon's renderer honours (via its `param_*`
/// fallbacks, which mirror the defaults here).
pub fn widget_schema(widget_type: WidgetType) -> Vec<EffectParamDescriptor> {
    let mut rows = widget_schema_inner(widget_type);
    if widget_type != WidgetType::Unknown {
        rows.push(EffectParamDescriptor {
            id: "opacity".to_string(),
            label: "Opacity".to_string(),
            kind: ParamKind::Range {
                min: 0.0,
                max: 100.0,
                step: 5.0,
            },
            default: EffectParamValue::Float(100.0),
        });
    }
    rows
}

/// Shared meter-gauge params: appearance/curve + fill/track/gradient colors,
/// identical for `Sensor` and `AudioLevel` (both render via the same ring/bar
/// gauge primitive).
fn meter_gauge_params() -> Vec<EffectParamDescriptor> {
    vec![
        EffectParamDescriptor {
            id: "rounded".to_string(),
            label: "Rounded ends".to_string(),
            kind: ParamKind::Boolean,
            default: EffectParamValue::Bool(false),
        },
        EffectParamDescriptor {
            id: "inverted".to_string(),
            label: "Inverted".to_string(),
            kind: ParamKind::Boolean,
            default: EffectParamValue::Bool(false),
        },
        EffectParamDescriptor {
            id: "curve".to_string(),
            label: "Meter curve".to_string(),
            kind: ParamKind::Range {
                min: 0.0,
                max: 180.0,
                step: 5.0,
            },
            default: EffectParamValue::Float(0.0),
        },
        EffectParamDescriptor {
            id: "fill".to_string(),
            label: "Fill color".to_string(),
            kind: ParamKind::Color,
            default: EffectParamValue::Color(ScreenStyle::default().accent),
        },
        EffectParamDescriptor {
            id: "track".to_string(),
            label: "Track color".to_string(),
            kind: ParamKind::Color,
            default: EffectParamValue::Color(DEFAULT_TRACK),
        },
        EffectParamDescriptor {
            id: "gradient".to_string(),
            label: "Value gradient".to_string(),
            kind: ParamKind::Boolean,
            default: EffectParamValue::Bool(false),
        },
        EffectParamDescriptor {
            id: "gradient_high".to_string(),
            label: "Gradient high color".to_string(),
            kind: ParamKind::Color,
            default: EffectParamValue::Color(DEFAULT_GRADIENT_HIGH),
        },
    ]
}

fn widget_schema_inner(widget_type: WidgetType) -> Vec<EffectParamDescriptor> {
    let enum_kind = |options: &[&str]| ParamKind::Enum {
        options: options.iter().map(|s| s.to_string()).collect(),
    };
    match widget_type {
        WidgetType::Clock => vec![EffectParamDescriptor {
            id: "variant".to_string(),
            label: "Style".to_string(),
            kind: enum_kind(&["24h", "24h_seconds", "12h"]),
            default: EffectParamValue::Str("24h".to_string()),
        }],
        WidgetType::Date => vec![EffectParamDescriptor {
            id: "variant".to_string(),
            label: "Style".to_string(),
            kind: enum_kind(&["short", "numeric"]),
            default: EffectParamValue::Str("short".to_string()),
        }],
        WidgetType::Sensor => {
            let mut rows = vec![
                EffectParamDescriptor {
                    id: "variant".to_string(),
                    label: "Style".to_string(),
                    kind: enum_kind(&["stat", "ring", "bar"]),
                    default: EffectParamValue::Str("stat".to_string()),
                },
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
                    id: "show_value".to_string(),
                    label: "Show value".to_string(),
                    kind: ParamKind::Boolean,
                    default: EffectParamValue::Bool(true),
                },
                EffectParamDescriptor {
                    id: "value_color".to_string(),
                    label: "Number color".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(DEFAULT_VALUE_COLOR),
                },
                EffectParamDescriptor {
                    id: "label_color".to_string(),
                    label: "Label color".to_string(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(DEFAULT_LABEL_COLOR),
                },
            ];
            rows.extend(meter_gauge_params());
            rows.extend([
                EffectParamDescriptor {
                    id: "min".to_string(),
                    label: "Min value".to_string(),
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1000.0,
                        step: 1.0,
                    },
                    default: EffectParamValue::Float(0.0),
                },
                EffectParamDescriptor {
                    id: "max".to_string(),
                    label: "Max value".to_string(),
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1000.0,
                        step: 1.0,
                    },
                    default: EffectParamValue::Float(100.0),
                },
            ]);
            rows
        }
        WidgetType::Text => vec![EffectParamDescriptor {
            id: "text".to_string(),
            label: "Text".to_string(),
            kind: ParamKind::Text,
            default: EffectParamValue::Str("TEXT".to_string()),
        }],
        WidgetType::Image => vec![
            EffectParamDescriptor {
                id: "filename".to_string(),
                label: "Image".to_string(),
                kind: ParamKind::Image,
                default: EffectParamValue::Str(String::new()),
            },
            EffectParamDescriptor {
                id: "fit".to_string(),
                label: "Scaling".to_string(),
                kind: enum_kind(&["fit", "cover", "contain"]),
                default: EffectParamValue::Str("fit".to_string()),
            },
            EffectParamDescriptor {
                id: "shape".to_string(),
                label: "Shape".to_string(),
                kind: enum_kind(&["rect", "rounded", "circle"]),
                default: EffectParamValue::Str("rect".to_string()),
            },
        ],
        WidgetType::AudioSpectrum => vec![
            EffectParamDescriptor {
                id: "bands".to_string(),
                label: "Bands".to_string(),
                kind: ParamKind::Range {
                    min: 8.0,
                    max: 64.0,
                    step: 8.0,
                },
                default: EffectParamValue::Float(32.0),
            },
            EffectParamDescriptor {
                id: "fill".to_string(),
                label: "Bar color".to_string(),
                kind: ParamKind::Color,
                default: EffectParamValue::Color(ScreenStyle::default().accent),
            },
            EffectParamDescriptor {
                id: "flip_h".to_string(),
                label: "Flip horizontally".to_string(),
                kind: ParamKind::Boolean,
                default: EffectParamValue::Bool(false),
            },
            EffectParamDescriptor {
                id: "flip_v".to_string(),
                label: "Flip vertically".to_string(),
                kind: ParamKind::Boolean,
                default: EffectParamValue::Bool(false),
            },
            EffectParamDescriptor {
                id: "mirror".to_string(),
                label: "Mirror from center".to_string(),
                kind: ParamKind::Boolean,
                default: EffectParamValue::Bool(false),
            },
            EffectParamDescriptor {
                id: "gradient".to_string(),
                label: "Height gradient".to_string(),
                kind: ParamKind::Boolean,
                default: EffectParamValue::Bool(false),
            },
            EffectParamDescriptor {
                id: "gradient_high".to_string(),
                label: "Gradient high color".to_string(),
                kind: ParamKind::Color,
                default: EffectParamValue::Color(SPECTRUM_GRADIENT_HIGH),
            },
        ],
        WidgetType::AudioLevel => {
            let mut rows = vec![EffectParamDescriptor {
                id: "variant".to_string(),
                label: "Style".to_string(),
                kind: enum_kind(&["ring", "bar"]),
                default: EffectParamValue::Str("ring".to_string()),
            }];
            rows.extend(meter_gauge_params());
            rows
        }
        WidgetType::NowPlaying => vec![
            EffectParamDescriptor {
                id: "show_art".to_string(),
                label: "Show album art".to_string(),
                kind: ParamKind::Boolean,
                default: EffectParamValue::Bool(true),
            },
            EffectParamDescriptor {
                id: "show_title".to_string(),
                label: "Show title".to_string(),
                kind: ParamKind::Boolean,
                default: EffectParamValue::Bool(true),
            },
            EffectParamDescriptor {
                id: "show_artist".to_string(),
                label: "Show artist".to_string(),
                kind: ParamKind::Boolean,
                default: EffectParamValue::Bool(true),
            },
            EffectParamDescriptor {
                id: "title_color".to_string(),
                label: "Title color".to_string(),
                kind: ParamKind::Color,
                default: EffectParamValue::Color(DEFAULT_VALUE_COLOR),
            },
            EffectParamDescriptor {
                id: "artist_color".to_string(),
                label: "Artist color".to_string(),
                kind: ParamKind::Color,
                default: EffectParamValue::Color(DEFAULT_LABEL_COLOR),
            },
        ],
        WidgetType::Logo => vec![
            EffectParamDescriptor {
                id: "show_img".to_string(),
                label: "Show logo".to_string(),
                kind: ParamKind::Boolean,
                default: EffectParamValue::Bool(true),
            },
            EffectParamDescriptor {
                id: "show_text".to_string(),
                label: "Show text".to_string(),
                kind: ParamKind::Boolean,
                default: EffectParamValue::Bool(true),
            },
        ],
        WidgetType::Shape => vec![
            EffectParamDescriptor {
                id: "shape".to_string(),
                label: "Shape".to_string(),
                kind: enum_kind(&["circle", "square", "rectangle", "triangle", "line"]),
                default: EffectParamValue::Str("circle".to_string()),
            },
            EffectParamDescriptor {
                id: "filled".to_string(),
                label: "Filled".to_string(),
                kind: ParamKind::Boolean,
                default: EffectParamValue::Bool(true),
            },
            EffectParamDescriptor {
                id: "fill".to_string(),
                label: "Color".to_string(),
                kind: ParamKind::Color,
                default: EffectParamValue::Color(ScreenStyle::default().accent),
            },
        ],
        WidgetType::Debug | WidgetType::Unknown => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn effect_param_value_strategy() -> impl Strategy<Value = EffectParamValue> {
        prop_oneof![
            any::<f64>()
                .prop_filter("finite", |f| f.is_finite())
                .prop_map(EffectParamValue::Float),
            ".*".prop_map(EffectParamValue::Str),
            (any::<u8>(), any::<u8>(), any::<u8>())
                .prop_map(|(r, g, b)| EffectParamValue::Color(RgbColor { r, g, b })),
            any::<bool>().prop_map(EffectParamValue::Bool),
        ]
    }

    fn widget_type_strategy() -> impl Strategy<Value = WidgetType> {
        prop_oneof![
            Just(WidgetType::Clock),
            Just(WidgetType::Date),
            Just(WidgetType::Sensor),
            Just(WidgetType::Text),
            Just(WidgetType::Image),
            Just(WidgetType::Debug),
            Just(WidgetType::AudioSpectrum),
            Just(WidgetType::AudioLevel),
            Just(WidgetType::NowPlaying),
            Just(WidgetType::Logo),
            Just(WidgetType::Shape),
            Just(WidgetType::Unknown),
        ]
    }

    fn bg_kind_strategy() -> impl Strategy<Value = BgKind> {
        prop_oneof![
            Just(BgKind::Flow),
            Just(BgKind::Solid),
            Just(BgKind::Grid),
            Just(BgKind::Glow),
            (".*", any::<f64>().prop_filter("finite", |f| f.is_finite()))
                .prop_map(|(filename, dim)| BgKind::Image { filename, dim }),
        ]
    }

    fn font_kind_strategy() -> impl Strategy<Value = FontKind> {
        prop_oneof![
            Just(FontKind::Mono),
            Just(FontKind::Sans),
            Just(FontKind::Inter),
        ]
    }

    #[test]
    fn font_kind_wire_keys_stay_stable() {
        // Saved templates persist these keys — they must not drift.
        assert_eq!(serde_json::to_string(&FontKind::Sans).unwrap(), "\"sans\"");
        assert_eq!(serde_json::to_string(&FontKind::Mono).unwrap(), "\"mono\"");
        assert_eq!(
            serde_json::to_string(&FontKind::Inter).unwrap(),
            "\"inter\""
        );
        for f in FontKind::ALL {
            assert!(!f.label().is_empty());
        }
    }

    fn widget_def_strategy() -> impl Strategy<Value = WidgetDef> {
        (
            ".*",
            widget_type_strategy(),
            any::<f32>().prop_filter("finite", |f| f.is_finite()),
            any::<f32>().prop_filter("finite", |f| f.is_finite()),
            any::<f32>().prop_filter("finite", |f| f.is_finite()),
            any::<f32>().prop_filter("finite", |f| f.is_finite()),
            proptest::option::of((any::<u8>(), any::<u8>(), any::<u8>())),
            proptest::option::of(font_kind_strategy()),
            prop::collection::hash_map(".*", effect_param_value_strategy(), 0..4),
        )
            .prop_map(
                |(id, widget_type, x, y, scale, rotation, color, font, params)| WidgetDef {
                    id,
                    widget_type,
                    x,
                    y,
                    scale,
                    rotation,
                    color: color.map(|(r, g, b)| RgbColor { r, g, b }),
                    font,
                    params,
                },
            )
    }

    fn screen_style_strategy() -> impl Strategy<Value = ScreenStyle> {
        (
            (any::<u8>(), any::<u8>(), any::<u8>()),
            bg_kind_strategy(),
            font_kind_strategy(),
        )
            .prop_map(|((r, g, b), background, font)| ScreenStyle {
                accent: RgbColor { r, g, b },
                background,
                font,
            })
    }

    fn custom_template_def_strategy() -> impl Strategy<Value = CustomTemplateDef> {
        (
            prop::collection::vec(widget_def_strategy(), 0..6),
            screen_style_strategy(),
        )
            .prop_map(|(widgets, style)| CustomTemplateDef { widgets, style })
    }

    proptest! {
        #[test]
        fn custom_template_def_json_round_trips(def in custom_template_def_strategy()) {
            let json = serde_json::to_string(&def).unwrap();
            let back: CustomTemplateDef = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, def);
        }

        #[test]
        fn validate_widgets_never_panics(def in custom_template_def_strategy()) {
            let _ = validate_widgets(&def);
        }
    }

    fn def_with(widgets: Vec<WidgetDef>) -> CustomTemplateDef {
        CustomTemplateDef {
            widgets,
            style: ScreenStyle::default(),
        }
    }

    #[test]
    fn validate_widgets_accepts_a_default_widget() {
        assert!(validate_widgets(&def_with(vec![widget_with(&[])])).is_ok());
    }

    #[test]
    fn validate_widgets_rejects_offcanvas_and_nonfinite() {
        let mut off = widget_with(&[]);
        off.x = 1.5;
        assert!(validate_widgets(&def_with(vec![off])).is_err());

        let mut nan_rot = widget_with(&[]);
        nan_rot.rotation = f32::NAN;
        assert!(validate_widgets(&def_with(vec![nan_rot])).is_err());

        let mut bad_scale = widget_with(&[]);
        bad_scale.scale = 0.0;
        assert!(validate_widgets(&def_with(vec![bad_scale])).is_err());
    }

    #[test]
    fn validate_widgets_rejects_traversal_image_filename() {
        let w = widget_with(&[(
            "filename",
            EffectParamValue::Str("../../etc/shadow".to_string()),
        )]);
        assert!(validate_widgets(&def_with(vec![w])).is_err());
    }

    #[test]
    fn validate_widgets_rejects_too_many_params() {
        let params: Vec<(String, EffectParamValue)> = (0..MAX_EFFECT_PARAMS + 1)
            .map(|i| (format!("k{i}"), EffectParamValue::Float(1.0)))
            .collect();
        let mut w = widget_with(&[]);
        for (k, v) in params {
            w.params.insert(k, v);
        }
        assert!(validate_widgets(&def_with(vec![w])).is_err());
    }

    #[test]
    fn validate_widgets_enforces_schema_and_widget_specific_invariants() {
        let mut unknown = widget_with(&[("not_a_param", EffectParamValue::Bool(true))]);
        unknown.widget_type = WidgetType::Logo;
        assert!(validate_widgets(&def_with(vec![unknown])).is_err());

        let mut wrong_type = widget_with(&[("opacity", EffectParamValue::Str("100".to_string()))]);
        wrong_type.widget_type = WidgetType::Logo;
        assert!(validate_widgets(&def_with(vec![wrong_type])).is_err());

        let mut sensor = widget_with(&[
            ("min", EffectParamValue::Float(100.0)),
            ("max", EffectParamValue::Float(10.0)),
        ]);
        sensor.widget_type = WidgetType::Sensor;
        assert!(validate_widgets(&def_with(vec![sensor])).is_err());
    }

    #[test]
    fn validate_widgets_rejects_duplicate_ids_and_non_box_scale_y() {
        let a = widget_with(&[]);
        let b = widget_with(&[]);
        assert!(validate_widgets(&def_with(vec![a, b])).is_err());

        let mut clock = widget_with(&[("scale_y", EffectParamValue::Float(2.0))]);
        clock.widget_type = WidgetType::Clock;
        assert!(validate_widgets(&def_with(vec![clock])).is_err());
    }

    #[test]
    fn validate_widgets_requires_a_bounded_background_dim() {
        let mut def = def_with(vec![widget_with(&[])]);
        def.style.background = BgKind::Image {
            filename: "wallpaper.png".to_string(),
            dim: 101.0,
        };
        assert!(validate_widgets(&def).is_err());
    }

    #[test]
    fn unknown_widget_type_deserializes_to_unknown() {
        let wt: WidgetType = serde_json::from_str("\"some_future_widget\"").unwrap();
        assert_eq!(wt, WidgetType::Unknown);
    }

    #[test]
    fn bg_kind_image_round_trips() {
        let bg = BgKind::Image {
            filename: "wallpaper.gif".to_string(),
            dim: 42.0,
        };
        let json = serde_json::to_string(&bg).unwrap();
        let back: BgKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, bg);
    }

    #[test]
    fn widget_schema_covers_every_non_unknown_type() {
        for wt in [
            WidgetType::Clock,
            WidgetType::Date,
            WidgetType::Sensor,
            WidgetType::Text,
            WidgetType::Image,
            WidgetType::AudioSpectrum,
            WidgetType::AudioLevel,
            WidgetType::NowPlaying,
            WidgetType::Logo,
            WidgetType::Shape,
        ] {
            assert!(
                !widget_schema(wt).is_empty(),
                "widget_schema must cover {wt:?}"
            );
        }
        assert!(widget_schema(WidgetType::Unknown).is_empty());
    }

    #[test]
    fn audio_spectrum_wire_name_deserializes() {
        let wt: WidgetType = serde_json::from_str("\"audio_spectrum\"").unwrap();
        assert_eq!(wt, WidgetType::AudioSpectrum);
    }

    #[test]
    fn audio_level_wire_name_deserializes() {
        let wt: WidgetType = serde_json::from_str("\"audio_level\"").unwrap();
        assert_eq!(wt, WidgetType::AudioLevel);
    }

    #[test]
    fn lerp_rgb_endpoints_and_midpoint() {
        let a = RgbColor { r: 0, g: 0, b: 0 };
        let b = RgbColor {
            r: 200,
            g: 100,
            b: 50,
        };
        assert_eq!(lerp_rgb(a, b, 0.0), a);
        assert_eq!(lerp_rgb(a, b, 1.0), b);
        // Clamps out-of-range t.
        assert_eq!(lerp_rgb(a, b, 2.0), b);
        assert_eq!(lerp_rgb(a, b, -1.0), a);
        let mid = lerp_rgb(a, b, 0.5);
        assert_eq!((mid.r, mid.g, mid.b), (100, 50, 25));
    }

    #[test]
    fn sensor_fill_color_gradient_toggle() {
        let fill = RgbColor { r: 0, g: 200, b: 0 };
        let high = RgbColor { r: 200, g: 0, b: 0 };
        // Gradient off → always the base fill.
        assert_eq!(sensor_fill_color(fill, high, false, 1.0), fill);
        // Gradient on → blends toward high with the value fraction.
        assert_eq!(sensor_fill_color(fill, high, true, 0.0), fill);
        assert_eq!(sensor_fill_color(fill, high, true, 1.0), high);
    }

    #[test]
    fn widget_def_without_rotation_defaults_to_zero() {
        // Templates saved before rotation existed omit the field.
        let json = r#"{"id":"w1","widget_type":"clock","x":0.5,"y":0.5,"scale":1.0,"params":{}}"#;
        let w: WidgetDef = serde_json::from_str(json).unwrap();
        assert_eq!(w.rotation, 0.0);
    }

    #[test]
    fn now_playing_wire_name_deserializes() {
        let wt: WidgetType = serde_json::from_str("\"now_playing\"").unwrap();
        assert_eq!(wt, WidgetType::NowPlaying);
    }

    fn widget_with(params: &[(&str, EffectParamValue)]) -> WidgetDef {
        let mut w = WidgetDef {
            id: "w1".to_string(),
            widget_type: WidgetType::Text,
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            rotation: 0.0,
            color: None,
            font: None,
            params: HashMap::new(),
        };
        for (k, v) in params {
            w.params.insert(k.to_string(), v.clone());
        }
        w
    }

    #[test]
    fn param_accessors_fall_back_on_missing_or_wrong_type() {
        let w = widget_with(&[
            ("text", EffectParamValue::Str("hi".to_string())),
            ("variant", EffectParamValue::Str("ring".to_string())),
            ("min", EffectParamValue::Float(5.0)),
            ("on", EffectParamValue::Bool(true)),
            ("track", EffectParamValue::Color(DEFAULT_TRACK)),
        ]);
        assert_eq!(param_str(&w, "text"), "hi");
        assert_eq!(param_str(&w, "absent"), "");
        assert_eq!(param_variant(&w, "stat"), "ring");
        assert_eq!(param_variant(&widget_with(&[]), "stat"), "stat");
        assert_eq!(param_f64(&w, "min", 0.0), 5.0);
        assert_eq!(param_f64(&w, "max", 100.0), 100.0);
        assert!(param_bool(&w, "on", false));
        assert!(!param_bool(&w, "off", false));
        assert_eq!(
            param_color(&w, "track", RgbColor { r: 1, g: 2, b: 3 }),
            DEFAULT_TRACK
        );
        assert_eq!(
            param_color(&w, "absent", RgbColor { r: 1, g: 2, b: 3 }),
            RgbColor { r: 1, g: 2, b: 3 }
        );
        // Empty `variant` is treated as absent, so the default wins.
        assert_eq!(
            param_variant(
                &widget_with(&[("variant", EffectParamValue::Str(String::new()))]),
                "stat"
            ),
            "stat"
        );
    }

    #[test]
    fn format_value_suffixes_the_sensor_unit() {
        assert_eq!(format_value(42.0, &SensorUnit::Celsius), "42°");
        assert_eq!(format_value(73.0, &SensorUnit::Percent), "73%");
        assert_eq!(format_value(4500.0, &SensorUnit::Megahertz), "4500 MHz");
        assert_eq!(format_value(80.0, &SensorUnit::Fahrenheit), "80°F");
        assert_eq!(format_value(3.0, &SensorUnit::Hours), "3 h");
    }

    #[test]
    fn default_custom_template_def_serializes() {
        let def = CustomTemplateDef::default();
        assert_eq!(def.widgets.len(), 1);
        assert_eq!(def.widgets[0].widget_type, WidgetType::Logo);
        assert!(param_bool(&def.widgets[0], "show_img", true));
        assert!(param_bool(&def.widgets[0], "show_text", true));
        assert_eq!(def.style.font, FontKind::Sans);
        serde_json::to_string(&def).unwrap();
    }

    #[test]
    fn lcd_editor_render_round_trips_with_signatures() {
        let render = LcdEditorRender {
            device_id: "dev1".to_string(),
            canvas_w: 240,
            canvas_h: 240,
            sprites: vec![],
            signatures: vec![("a".to_string(), 1), ("b".to_string(), 2)],
        };
        let json = serde_json::to_string(&render).unwrap();
        let back: LcdEditorRender = serde_json::from_str(&json).unwrap();
        assert_eq!(back, render);
    }
}
