use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

pub use crate::commands::ScreenRotation;
use crate::zone_transform::ZoneContentTransform;

pub const DEFAULT_PROFILE_NAME: &str = "default";

/// Relative to the daemon's config dir.
pub const LCD_IMAGES_SUBDIR: &str = "media/lcd_images";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdEngineTemplateDescriptor {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub params: Vec<EffectParamDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WireLcdEngineState {
    pub available_templates: Vec<LcdEngineTemplateDescriptor>,
    #[serde(default)]
    pub available_widgets: Vec<LcdWidgetDescriptor>,
    #[serde(default)]
    pub available_presets: Vec<LcdPresetDescriptor>,
    /// device_id → template_id
    pub device_templates: HashMap<String, String>,
    /// device_id → (param id → value) for the active template.
    #[serde(default)]
    pub device_template_params: HashMap<String, HashMap<String, EffectParamValue>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LcdWidgetResize {
    #[default]
    Uniform,
    Box,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LcdWidgetUpdates {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u32>,
    #[serde(default)]
    pub data: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_when: Option<LcdParamVisibility>,
    #[serde(default)]
    pub audio: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_when: Option<LcdParamVisibility>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdWidgetDescriptor {
    /// Namespaced `<plugin_id>:<widget_id>` catalog id.
    pub id: String,
    pub plugin_id: String,
    pub name: String,
    /// `name` remains the mandatory fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_key: Option<String>,
    /// Inert plugin-owned catalogs, indexed by locale then translation key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub translations: BTreeMap<String, BTreeMap<String, String>>,
    /// Declared SVG asset, fetched through `GetLcdWidgetIcon`.
    pub icon: String,
    /// Additional SVGs this widget may access through `ctx:draw_asset`.
    #[serde(default)]
    pub assets: Vec<String>,
    #[serde(default)]
    pub params: Vec<EffectParamDescriptor>,
    pub resize: LcdWidgetResize,
    /// Initial horizontal scale and height/width ratio used by the editor.
    #[serde(default = "default_widget_scale")]
    pub default_scale: f32,
    #[serde(default = "default_widget_min_scale")]
    pub min_scale: f32,
    #[serde(default = "default_widget_aspect")]
    pub default_aspect: f32,
    /// Text parameter whose content should grow the widget horizontally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_width_param: Option<String>,
    #[serde(default)]
    pub param_visibility: HashMap<String, LcdParamVisibility>,
    #[serde(default)]
    pub uses_color: bool,
    #[serde(default)]
    pub uses_font: bool,
    #[serde(default = "default_true")]
    pub font_controls: bool,
    /// `None` inherits the screen font.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_font: Option<String>,
    /// Host-enforced weight for fixed-style widgets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_text_weight: Option<String>,
    #[serde(default)]
    pub updates: LcdWidgetUpdates,
}

impl LcdWidgetDescriptor {
    pub fn localized_name(&self, locale: &str) -> &str {
        let Some(key) = self.name_key.as_deref() else {
            return &self.name;
        };
        let locale = locale.to_ascii_lowercase();
        let base = locale.split(['-', '_']).next().unwrap_or(&locale);
        self.translations
            .get(&locale)
            .and_then(|c| c.get(key))
            .or_else(|| self.translations.get(base).and_then(|c| c.get(key)))
            .or_else(|| self.translations.get("en").and_then(|c| c.get(key)))
            .map(String::as_str)
            .unwrap_or(&self.name)
    }
}

fn default_widget_scale() -> f32 {
    1.0
}

fn default_widget_min_scale() -> f32 {
    0.6
}

fn default_widget_aspect() -> f32 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LcdParamVisibility {
    pub param: String,
    pub equals: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LcdPresetDescriptor {
    /// Namespaced `<plugin_id>:<preset_id>` catalog id.
    pub id: String,
    pub plugin_id: String,
    pub name: String,
    pub file: String,
}

/// Pushed only to the uploading client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LcdUploadProgress {
    pub device_id: String,
    pub stage: LcdUploadStage,
    /// 0–100 within the stage; `None` when the stage has no measurable extent.
    #[serde(default)]
    pub percent: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LcdUploadStage {
    /// GIFs report per-frame percent.
    Processing,
    Applying,
    /// Terminal.
    Done,
    /// Terminal.
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdEngineFrame {
    pub device_id: String,
    pub frame_id: u64,
    /// Base64-encoded PNG preview image.
    pub preview_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedFrameEntry {
    pub device_id: String,
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
pub const DEFAULT_LANGUAGE: &str = "en";
/// The daemon accepts only these in `SetLanguage`; each needs a `locales/<code>/` catalog.
pub const SUPPORTED_LANGUAGES: &[&str] = &["en", "it"];

/// Falls back to `DEFAULT_LANGUAGE` when the OS locale has no matching catalog.
pub fn detect_system_language() -> &'static str {
    sys_locale::get_locale().map_or(DEFAULT_LANGUAGE, |locale| narrow_language(&locale))
}

fn narrow_language(locale: &str) -> &'static str {
    let primary = locale
        .split(['-', '_', '.'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    SUPPORTED_LANGUAGES
        .iter()
        .copied()
        .find(|code| *code == primary)
        .unwrap_or(DEFAULT_LANGUAGE)
}
/// Kept well under `LCD_PREVIEW_LEASE_SECS`.
pub const LCD_PREVIEW_KEEPALIVE_SECS: f64 = 1.0;
/// Daemon pauses streaming after this.
pub const LCD_PREVIEW_LEASE_SECS: u64 = 3;

fn default_zone_size() -> f32 {
    DEFAULT_ZONE_SIZE
}

/// Enforced at the transport layer; there is no default ceiling, devices opt in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteRateLimit {
    pub max_bytes_per_sec: u32,
}

/// `limit` is `None` unless the device declares a ceiling; throughput is still measured.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct WriteRateStatus {
    pub limit: Option<WriteRateLimit>,
    pub current_writes_per_sec: f32,
    pub current_bytes_per_sec: f32,
    pub rejected_total: u64,
}

/// The JSON broadcast rejects non-finite floats, so a hand-edited NaN must not reach it.
pub fn finite_or(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

/// Allowlist, not a traversal denylist: a bare `[A-Za-z0-9._-]` name with a known image
/// extension and no leading dot, ruling out separators, `..`, absolute paths and NTFS streams.
pub fn validate_image_filename(name: &str) -> Result<(), &'static str> {
    let ext_ok = matches!(
        std::path::Path::new(name)
            .extension()
            .and_then(|e| e.to_str()),
        Some("png" | "jpg" | "jpeg" | "gif")
    );
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && ext_ok
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err("invalid filename")
    }
}

/// Sized so its base64 fits one IPC frame ([`crate::frames::MAX_PAYLOAD`]).
pub const MAX_LCD_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

pub fn validate_image_upload_size(len: u64) -> Result<(), String> {
    if len > MAX_LCD_IMAGE_BYTES {
        return Err(format!(
            "image is {len} bytes, over the {MAX_LCD_IMAGE_BYTES}-byte limit"
        ));
    }
    Ok(())
}

pub fn sniff_ext(data: &[u8]) -> &'static str {
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        "gif"
    } else if data.starts_with(&[0xFF, 0xD8]) {
        "jpg"
    } else if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else {
        "unknown"
    }
}

/// Bounds a hostile plugin from bloating the socket and the GUI's texture memory.
pub const MAX_PLUGIN_ASSET_BYTES: u64 = 256 * 1024;

/// Painted into a small square tile, so anything larger is wasted.
pub const MAX_PLUGIN_LOGO_DIM: u32 = 512;

/// Long:short side ratio; an extreme banner would letterbox to an unreadable sliver.
pub const MAX_PLUGIN_LOGO_ASPECT: u32 = 2;

pub fn validate_logo_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("logo has a zero dimension".to_owned());
    }
    if width > MAX_PLUGIN_LOGO_DIM || height > MAX_PLUGIN_LOGO_DIM {
        return Err(format!(
            "logo {width}x{height} exceeds the {MAX_PLUGIN_LOGO_DIM}px maximum"
        ));
    }
    let (long, short) = if width >= height {
        (width, height)
    } else {
        (height, width)
    };
    if long > short * MAX_PLUGIN_LOGO_ASPECT {
        return Err(format!(
            "logo aspect {width}x{height} exceeds the {MAX_PLUGIN_LOGO_ASPECT}:1 maximum"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SamplingMode {
    #[default]
    Spatial,
    /// LEDs are laid along the zone rect by chain index.
    Unrolled,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlacedZone {
    pub device_id: String,
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

/// Persisted under `config.yaml`'s `cooling` key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CoolingConfig {
    pub fan_curve_enabled: bool,
    /// Milliseconds between fan-curve ticks.
    pub fan_curve_tick_ms: u64,
    /// Duty (0-100) applied when a fan's assigned sensor is absent.
    pub fan_failsafe_duty: u8,
}

impl Default for CoolingConfig {
    fn default() -> Self {
        Self {
            fan_curve_enabled: true,
            fan_curve_tick_ms: 2000,
            fan_failsafe_duty: 75,
        }
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

/// Persisted under `config.yaml`'s `lcd` key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LcdConfig {
    pub enabled: bool,
    pub fps: u32,
}

impl Default for LcdConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fps: 20,
        }
    }
}

/// `Unset` remains readable for configs written by older versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PluginDownloadConsent {
    Unset,
    #[default]
    Allowed,
    Denied,
}

pub const ONBOARDING_TOUR_KEY: &str = "onboarding";

pub const MAX_HOME_WIDGETS: usize = 12;
pub const MAX_BATTERY_WIDGET_ENTRIES: usize = 3;

pub const BATTERY_REF_SEPARATOR: char = '/';

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HomeWidgetKind {
    Chart {
        sensor_id: String,
    },
    Gauge {
        sensor_id: String,
        min: f64,
        max: f64,
    },
    /// Battery references, each `"{device_id}/{battery_key}"`.
    Battery {
        batteries: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HomeWidget {
    pub id: String,
    pub kind: HomeWidgetKind,
    /// Index into the GUI's widget accent palette.
    pub color: u8,
    /// Overrides the source's own name when non-empty.
    #[serde(default)]
    pub label: String,
}

pub fn battery_ref(device_id: &str, key: &str) -> String {
    format!("{device_id}{BATTERY_REF_SEPARATOR}{key}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeWidgetsError {
    TooMany,
    DuplicateId,
    EmptyId,
    TooManyBatteries,
    EmptyRange,
}

impl std::fmt::Display for HomeWidgetsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::TooMany => "too many home widgets",
            Self::DuplicateId => "duplicate home widget id",
            Self::EmptyId => "home widget id must not be empty",
            Self::TooManyBatteries => "too many batteries in one widget",
            Self::EmptyRange => "gauge range must have min below max",
        };
        f.write_str(text)
    }
}

impl std::error::Error for HomeWidgetsError {}

pub fn validate_home_widgets(widgets: &[HomeWidget]) -> Result<(), HomeWidgetsError> {
    if widgets.len() > MAX_HOME_WIDGETS {
        return Err(HomeWidgetsError::TooMany);
    }
    let mut seen = std::collections::HashSet::new();
    for w in widgets {
        if w.id.trim().is_empty() {
            return Err(HomeWidgetsError::EmptyId);
        }
        if !seen.insert(&w.id) {
            return Err(HomeWidgetsError::DuplicateId);
        }
        match &w.kind {
            HomeWidgetKind::Gauge { min, max, .. } if min >= max => {
                return Err(HomeWidgetsError::EmptyRange)
            }
            HomeWidgetKind::Battery { batteries }
                if batteries.len() > MAX_BATTERY_WIDGET_ENTRIES =>
            {
                return Err(HomeWidgetsError::TooManyBatteries)
            }
            _ => {}
        }
    }
    Ok(())
}

/// Splits from the right so a device id containing the separator survives.
pub fn split_battery_ref(reference: &str) -> Option<(&str, &str)> {
    reference.rsplit_once(BATTERY_REF_SEPARATOR)
}

/// Persisted under `config.yaml`'s `gui` key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiConfig {
    pub language: String,
    pub close_to_tray: bool,
    pub suppress_dependency_warning: bool,
    /// For tiling window managers that drive those actions themselves.
    pub hide_window_controls: bool,
    /// Notify once below 20%, then again below 10% and 5% while discharging.
    pub low_battery_notifications: bool,
    /// Persistence keys (e.g. "page:home") of completed or skipped tutorials.
    pub seen_tours: BTreeSet<String>,
    pub log_level: String,
    pub plugin_downloads: PluginDownloadConsent,
    /// In display order.
    pub home_widgets: Vec<HomeWidget>,
    /// In display order; separate from `home_widgets` so each page keeps its own sensors.
    pub cooling_widgets: Vec<HomeWidget>,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            language: DEFAULT_LANGUAGE.to_string(),
            close_to_tray: true,
            suppress_dependency_warning: false,
            hide_window_controls: false,
            low_battery_notifications: true,
            seen_tours: BTreeSet::new(),
            log_level: "info".to_string(),
            plugin_downloads: PluginDownloadConsent::Allowed,
            home_widgets: Vec::new(),
            cooling_widgets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSeverity {
    Info,
    Warning,
    Error,
}

/// The GUI translates this code instead of displaying backend text.
pub const ERROR_REPOSITORY_SIGNATURE_VERIFICATION_FAILED: &str =
    "repository_signature_verification_failed";

/// The GUI owns all human-readable copy; `severity()` is derived from the variant, not sent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum NotificationCode {
    EngineStopped {
        detail: String,
    },
    KeyRemapUnavailable {
        detail: String,
    },
    DeviceReconnectFailed {
        device: String,
    },
    ProfileSwitched {
        profile: String,
    },
    LowBattery {
        device: String,
        battery: String,
        level: u8,
        threshold: u8,
    },
    ChainLinkRestoreFailed {
        name: String,
        detail: String,
    },
    DeviceInitFailed {
        device: String,
        detail: String,
    },
    /// `detail` carries the complete error chain for the Details modal.
    DeviceWriteFailed {
        device: String,
        detail: String,
    },
    FanStalled {
        fan: String,
    },
    /// Dirty-update metadata, not a consent decision.
    PluginContentChanged {
        plugin: String,
    },
    /// Background-path Lua failure, deduplicated daemon-side so it alerts once, not every frame.
    PluginRuntimeError {
        plugin: String,
        detail: String,
    },
    /// Deduplicated daemon-side per plugin, alerting once per failure episode.
    PluginConnectFailed {
        plugin: String,
        detail: String,
    },
    /// `plugin` is the display name and never includes device paths or serials.
    PluginRecommended {
        plugin: String,
    },
    /// Sticky; signed repositories may expose a restore action through `restore_slug`.
    RepositoryIntegrityError {
        repository: Option<String>,
        package: String,
        expected: String,
        actual: String,
        restore_slug: Option<String>,
    },
    /// The GUI translates only the title and shows `message` verbatim.
    Generic {
        message: String,
    },
}

impl NotificationCode {
    pub fn severity(&self) -> NotificationSeverity {
        use NotificationCode::*;
        match self {
            EngineStopped { .. }
            | ChainLinkRestoreFailed { .. }
            | DeviceInitFailed { .. }
            | DeviceWriteFailed { .. }
            | RepositoryIntegrityError { .. }
            | Generic { .. } => NotificationSeverity::Error,
            KeyRemapUnavailable { .. }
            | DeviceReconnectFailed { .. }
            | FanStalled { .. }
            | PluginContentChanged { .. }
            | PluginRuntimeError { .. }
            | PluginConnectFailed { .. } => NotificationSeverity::Warning,
            ProfileSwitched { .. } | LowBattery { .. } | PluginRecommended { .. } => {
                NotificationSeverity::Info
            }
        }
    }

    pub fn detail(&self) -> Option<&str> {
        use NotificationCode::*;
        match self {
            DeviceWriteFailed { detail, .. }
            | PluginRuntimeError { detail, .. }
            | PluginConnectFailed { detail, .. } => Some(detail),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    #[serde(flatten)]
    pub code: NotificationCode,
    #[serde(rename = "showNative", default)]
    pub show_native: bool,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningApp {
    pub process_name: String,
    pub display_name: String,
    pub icon_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppRule {
    pub process_names: Vec<String>,
    pub profile: String,
    #[serde(default = "bool_true")]
    pub enabled: bool,
}

fn bool_true() -> bool {
    true
}

/// Empty `device_ids` = nothing selected; a device absent from `channels` targets all of them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LightingTargets {
    #[serde(default)]
    pub device_ids: Vec<String>,
    #[serde(default)]
    pub channels: HashMap<String, Vec<String>>,
}

/// `Null` when the default profile has no value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OverrideDiff {
    pub value: serde_json::Value,
    #[serde(default)]
    pub default: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileOverrides {
    /// `BTreeMap` for deterministic serialization order, which UI change-detection hashes.
    #[serde(default)]
    pub device_capabilities:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, OverrideDiff>>,
    #[serde(default)]
    pub canvas: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileState {
    #[serde(default)]
    pub active: String,
    #[serde(default)]
    pub available: Vec<String>,
    #[serde(default)]
    pub app_rules: Vec<AppRule>,
    #[serde(default)]
    pub overrides: std::collections::BTreeMap<String, ProfileOverrides>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthCheckState {
    /// Default false so the UI doesn't flash "supported" before the first broadcast.
    #[serde(default)]
    pub focus_watcher_supported: bool,
    /// Gates LCD video mode.
    #[serde(default)]
    pub ffmpeg_available: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginsState {
    #[serde(default)]
    pub plugins: Vec<PluginInfo>,
    #[serde(default)]
    pub repos: Vec<PluginRepoInfo>,
    /// Manifests too malformed to load, shown as a "skipped" notice.
    #[serde(default)]
    pub skipped: Vec<SkippedPlugin>,
    /// Computed once at startup.
    #[serde(default)]
    pub recommendations: Vec<PluginRecommendation>,
    /// Retained on this topic so reconnecting clients need no ad-hoc broadcast.
    #[serde(default)]
    pub updates: Vec<PluginUpdateStatus>,
    #[serde(default)]
    pub repo_updates: Vec<RepoUpdateStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedPlugin {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRepoInfo {
    pub url: String,
    pub slug: String,
    #[serde(default)]
    pub repository_id: Option<String>,
    #[serde(default)]
    pub release_tag: Option<String>,
    #[serde(default)]
    pub release_policy: PluginReleasePolicy,
    #[serde(default)]
    pub active_revision: Option<String>,
    #[serde(default)]
    pub previous_release_tag: Option<String>,
    /// RFC 3339.
    #[serde(default)]
    pub last_sync: Option<String>,
    /// The GUI hides the remove control for the seeded official repo.
    #[serde(default)]
    pub official: bool,
    #[serde(default)]
    pub location: PluginRepoLocation,
    #[serde(default)]
    pub signature: RepoSignatureStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "tag")]
pub enum PluginReleasePolicy {
    #[default]
    Latest,
    Pinned(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginReleaseInfo {
    pub tag: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRepoLocation {
    #[default]
    RemoteRelease,
    LocalArchive,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RepoSignatureStatus {
    Verified,
    #[default]
    Unsigned,
    Invalid {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoUpdateStatus {
    pub slug: String,
    pub installed_tag: String,
    #[serde(default)]
    pub available_tag: Option<String>,
    #[serde(default)]
    pub pinned: bool,
}

/// A repository can be behind while a given package's content is unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginUpdateStatus {
    pub plugin_id: String,
    pub slug: String,
    pub update_available: bool,
    /// A local edit (or tampering), not an upstream change.
    #[serde(default)]
    pub on_disk_changed: bool,
    pub current_version: String,
    pub available_version: String,
}

/// The boundary between "trusted to talk to its matched device" and "trusted to reach outside it".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Hid,
    /// Only allowlisted bulk/interrupt endpoints or bounded control-transfer policy.
    Usb,
    Hwmon,
    Lpcio,
    /// Read-only.
    AmdSmn,
    Command,
    Serial,
    Network,
    /// Currently clock reads via `os.time`/`os.clock`.
    Os,
    /// Non-secure config values need no permission.
    SecureStorage,
    /// A raw bus grants access to every device on it, hence a separate grant.
    Smbus,
    AudioRouting,
}

/// Separate from [`PluginSource`]: what Halo could verify, not where the bytes live.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginProvenance {
    VerifiedOfficial,
    UnsignedRepository,
    /// `--dev-plugin-repo`.
    LocalDevelopment,
    #[default]
    LocalUnsigned,
    /// Visible for repair but never executable.
    InvalidSignature,
}

/// Transport scopes are sorted before persistence so equality and subset checks are stable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginAuthority {
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub transport_scopes: Vec<String>,
    /// Exact keys, except for the host-managed `host.sensors.*` scope.
    #[serde(default)]
    pub data_reads: Vec<String>,
}

impl PluginAuthority {
    pub fn normalized(mut self) -> Self {
        self.permissions.sort_unstable();
        self.permissions.dedup();
        self.transport_scopes.sort();
        self.transport_scopes.dedup();
        self.data_reads.sort();
        self.data_reads.dedup();
        self
    }

    pub fn is_subset_of(&self, accepted: &Self) -> bool {
        let requested = self.clone().normalized();
        let accepted = accepted.clone().normalized();
        requested
            .permissions
            .iter()
            .all(|permission| accepted.permissions.binary_search(permission).is_ok())
            && requested
                .transport_scopes
                .iter()
                .all(|scope| accepted.transport_scopes.binary_search(scope).is_ok())
            && requested
                .data_reads
                .iter()
                .all(|scope| accepted.data_reads.binary_search(scope).is_ok())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDataRecordStatus {
    pub key: String,
    pub status: String,
    #[serde(default)]
    pub updated_at: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    #[default]
    Device,
    /// Never opens a transport.
    Effect,
    /// Instantiated from its own config values rather than a discovery handle.
    Integration,
    Lcd,
}

/// Display-only; the GUI fetches the bytes via `GetPluginAsset`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEffectAsset {
    pub id: String,
    pub thumbnail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginSource {
    #[default]
    Local,
    Repo {
        slug: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginIssueKind {
    ConnectFailed,
    InitFailed,
    /// Background path only.
    RuntimeError,
    /// e.g. bad logo, id collision.
    LoadWarning,
    LoadFailed,
}

/// Unstructured failures continue to use `PluginIssue::detail`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginIssueContext {
    RepositoryHashMismatch {
        package: String,
        expected: String,
        actual: String,
    },
    RepositoryManifestMismatch {
        package: String,
        field: String,
        expected: String,
        actual: String,
    },
}

/// Persisted so it survives a transient toast the user may have missed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginIssue {
    pub kind: PluginIssueKind,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<PluginIssueContext>,
    pub timestamp_ms: u64,
}

/// Identical failures update an existing episode without notifying again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    #[default]
    Healthy,
    Degraded,
    Failed,
}

/// A success resets it to `Healthy` and clears `notification_sent`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthState {
    #[serde(default)]
    pub status: HealthStatus,
    #[serde(default)]
    pub issue: Option<PluginIssue>,
    #[serde(default)]
    pub notification_sent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementImpact {
    #[default]
    Block,
    Degrade,
}

/// The UI owns the localized remediation text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementFailureReason {
    NotInstalled,
    NotLoaded,
    NotFound,
    PermissionDenied,
    Unavailable,
    Incompatible,
    Busy,
}

/// Not a permission: never enters the consented authority snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginRequirement {
    /// Must resolve on the daemon's `PATH`.
    Command { executable: String },
    /// Explicitly required by an integration.
    KernelModule { name: String },
    /// Inferred for Windows transports backed by the bundled modules.
    PawnIo,
    /// Inferred from an SMBus device match.
    LinuxI2c,
    /// `access` is `read` for sensor discovery or `pwm` for writable fan control.
    LinuxHwmon { access: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRequirementStatus {
    pub requirement: PluginRequirement,
    #[serde(default)]
    pub impact: RequirementImpact,
    pub satisfied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RequirementFailureReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
}

/// Derived from enable intent, setup completeness, and the live root device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationLifecycleState {
    #[default]
    Disabled,
    Unconfigured,
    Configured,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationSetupMode {
    Automatic,
    Manual,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationSetupPhase {
    #[default]
    Init,
    Discovering,
    Pairing,
    Done,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationAuthKind {
    #[default]
    None,
    Button,
    Oauth2Pkce,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationSetupCandidate {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub values: HashMap<String, String>,
}

/// Lua supplies only candidates and hook results; it never controls screens or transitions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntegrationSetupStatus {
    #[serde(default)]
    pub phase: IntegrationSetupPhase,
    #[serde(default)]
    pub modes: Vec<IntegrationSetupMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_mode: Option<IntegrationSetupMode>,
    #[serde(default)]
    pub auth: IntegrationAuthKind,
    #[serde(default)]
    pub candidates: Vec<IntegrationSetupCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_candidate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub instructions: Vec<String>,
    /// GUI i18n key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_url: Option<String>,
}

/// Distinct from consent and from runtime health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginActivationBlocker {
    MissingRequirements {
        requirements: Vec<PluginRequirementStatus>,
    },
}

/// Never includes device paths or serial numbers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum PluginRecommendationMatch {
    /// Currently reserved for the stock Halo Effects library.
    Always,
    Command {
        executable: String,
    },
    Hid {
        vid: u16,
        pid: u16,
    },
    Usb {
        vid: u16,
        pid: u16,
    },
    /// Chipset SMBus controllers alone would be false positives on nearly every desktop.
    SmbusGpu {
        pci_vendor: u16,
        pci_device: u16,
        pci_sub_vendor: u16,
        pci_sub_device: u16,
    },
}

/// Enabling still requires explicit authority consent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRecommendation {
    pub plugin_id: String,
    pub plugin_name: String,
    pub hardware: PluginRecommendationMatch,
    /// Always true until the access probe lands.
    #[serde(default = "default_true")]
    pub accessible: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdevRulesStatus {
    /// False on hosts where udev is not used.
    pub supported: bool,
    /// Highest-precedence installed `60-halod.rules` is byte-equal to the daemon's output.
    pub current: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_path: Option<String>,
    pub generated_rule_count: usize,
    /// Empty when the rules are current or only the daemon baseline drifted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins_requiring_update: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub missing_rules_by_plugin: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributing_plugin_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub translations: BTreeMap<String, BTreeMap<String, String>>,
    pub path: String,
    #[serde(default)]
    pub plugin_type: PluginKind,
    #[serde(default)]
    pub experimental: bool,
    /// e.g. "RGB", "Fan".
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Empty means all platforms.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// False when the catalog is visible on this host but must stay inert.
    #[serde(default = "default_true")]
    pub platform_supported: bool,
    #[serde(default)]
    pub effect_names: Vec<String>,
    pub enabled: bool,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub targets: Vec<String>,
    /// SPDX identifier or free-text name.
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub devices: Vec<PluginDeviceInfo>,
    #[serde(default)]
    pub logo: Option<String>,
    #[serde(default)]
    pub effect_thumbnails: Vec<PluginEffectAsset>,
    #[serde(default)]
    pub source: PluginSource,
    #[serde(default)]
    pub provenance: PluginProvenance,
    #[serde(default)]
    pub declared_permissions: Vec<Permission>,
    #[serde(default)]
    pub authority: PluginAuthority,
    #[serde(default)]
    pub provides: Vec<String>,
    #[serde(default)]
    pub consumes: Vec<String>,
    #[serde(default)]
    pub data_records: Vec<PluginDataRecordStatus>,
    #[serde(default)]
    pub accepted_authority: Option<PluginAuthority>,
    #[serde(default)]
    pub config_fields: Vec<PluginConfigField>,
    /// Secure fields never appear here; see `secret_set`.
    #[serde(default)]
    pub config_values: HashMap<String, String>,
    /// The secret's plaintext never crosses the IPC boundary.
    #[serde(default)]
    pub secret_set: HashMap<String, bool>,
    /// Independent of `enabled`, which only governs whether its Lua may run; `true` for non-integrations.
    #[serde(default = "default_true")]
    pub integration_enabled: bool,
    #[serde(default)]
    pub integration_configured: bool,
    #[serde(default)]
    pub integration_requires_setup: bool,
    #[serde(default)]
    pub integration_state: IntegrationLifecycleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_setup: Option<IntegrationSetupStatus>,
    /// Content hashes are update/dirty metadata, never a consent gate.
    #[serde(default = "default_true")]
    pub consented: bool,
    /// An enabled plugin is inactive while a blocking host requirement is missing.
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub requirements: Vec<PluginRequirementStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_blocker: Option<PluginActivationBlocker>,
    /// Device-specific records remain scoped in the daemon registry.
    #[serde(default)]
    pub health: HealthState,
}

impl PluginInfo {
    fn translated_text<'a>(&'a self, locale: &str, key: &str, fallback: &'a str) -> &'a str {
        let locale = locale.to_ascii_lowercase();
        let base = locale.split(['-', '_']).next().unwrap_or(&locale);
        self.translations
            .get(&locale)
            .and_then(|catalog| catalog.get(key))
            .or_else(|| {
                self.translations
                    .get(base)
                    .and_then(|catalog| catalog.get(key))
            })
            .or_else(|| {
                self.translations
                    .get("en")
                    .and_then(|catalog| catalog.get(key))
            })
            .map(String::as_str)
            .unwrap_or(fallback)
    }

    pub fn localized_name(&self, locale: &str) -> &str {
        self.translated_text(locale, "plugin.name", &self.name)
    }

    pub fn localized_description(&self, locale: &str) -> &str {
        self.translated_text(locale, "plugin.description", &self.description)
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDeviceInfo {
    pub vendor: String,
    pub model: String,
    /// The device's `name` override, or `model`.
    pub name: String,
    #[serde(default)]
    pub device_type: Option<DeviceType>,
}

/// `value` is a replug-stable `/dev/serial/by-id` path when available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialPortInfo {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginConfigFieldKind {
    #[default]
    Text,
    Number,
    Boolean,
    Enum,
    Host,
    Port,
    /// Offered as a dropdown of host ports, with a free-text fallback.
    SerialPort,
    Url,
    DurationMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginConfigVisibility {
    pub field: String,
    pub equals: String,
}

/// No `default`: the GUI only sees resolved values via `PluginInfo::config_values`/`secret_set`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginConfigField {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub kind: PluginConfigFieldKind,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<PluginConfigVisibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoolingState {
    #[serde(default)]
    pub fan_curves: Vec<WireFanCurve>,
    #[serde(default)]
    pub preset_curves: Vec<WirePresetCurve>,
    #[serde(default)]
    pub config: CoolingConfig,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LcdState {
    #[serde(default)]
    pub engine: WireLcdEngineState,
    /// `lcd/<name>.yaml`, sorted.
    #[serde(default)]
    pub templates: Vec<String>,
    #[serde(default)]
    pub config: LcdConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryPhase {
    #[default]
    Discovering,
    Idle,
    Complete,
    Error,
}

/// Structured so the UI can translate the step.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiscoveryDetail {
    #[default]
    None,
    Usb,
    PluginHostTransports,
    Hid,
    PluginIntegrations,
    Hwmon,
    Computer,
    Smbus,
    SmbusAdapter {
        name: String,
    },
    SmbusBus {
        number: u32,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoveryStatus {
    #[serde(default)]
    pub phase: DiscoveryPhase,
    #[serde(default)]
    pub detail: DiscoveryDetail,
    /// Independent of `phase`: may still be true after discovery completes.
    #[serde(default)]
    pub checking_updates: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeviceType {
    #[default]
    Other,
    Fan,
    Hub,
    Dongle,
    Keyboard,
    Mouse,
    Headset,
    Monitor,
    Gpu,
    LedStrip,
    Motherboard,
    Ram,
    Sensor,
    AIO,
    Speaker,
    Computer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionType {
    Wired,
    Wireless,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VisibilityState {
    #[default]
    Visible,
    Hidden,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WireDevice {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub model: String,
    pub device_type: DeviceType,
    pub connected: bool,
    pub capabilities: Vec<DeviceCapability>,
    #[serde(default)]
    pub active_state: VisibilityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_type: Option<ConnectionType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    /// `None` when unknown/internal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// `None` without live stats; the GUI hides the row rather than show a misleading zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_rate: Option<WriteRateStatus>,
    /// Empty ⇒ one full-width row per category, alphabetically.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub control_layout: Vec<CategoryLayout>,
    /// Set when this device is an integration's root, not real hardware; shown only on the Integrations page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<String>,
    /// Computed per snapshot, never persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<DeviceConflictSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictConfidence {
    Confirmed,
    Possible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceConflictSummary {
    pub peer_ids: Vec<String>,
    pub recommended_id: String,
    pub confidence: ConflictConfidence,
    /// Lets clients explain an integration/plugin choice without guessing from the device id.
    #[serde(default)]
    pub participants: Vec<ConflictParticipant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictParticipant {
    pub id: String,
    pub source: ConflictDeviceSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConflictDeviceSource {
    #[default]
    Builtin,
    Plugin(String),
    Integration(String),
}

fn span_default() -> u8 {
    1
}

/// `column`/`span` are 0-based start and width in grid columns; `order` breaks row ties.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CategoryLayout {
    /// Matches `Choice`/`Range`/`Boolean`/`Action` `category` (`""` ⇒ `"Settings"`).
    pub category: String,
    #[serde(default)]
    pub order: i32,
    #[serde(default)]
    pub column: u8,
    #[serde(default = "span_default")]
    pub span: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    #[default]
    Idle,
    Listening,
    /// During the most recent listening window.
    Paired,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PairingSlot {
    pub slot: u8,
    pub device_id: String,
    pub name: String,
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PairingStatus {
    pub state: PairingState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub max_slots: u8,
    pub slots: Vec<PairingSlot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum DeviceCapability {
    Children(Vec<WireDevice>),
    Pairing(PairingStatus),
    Choice(Vec<Choice>),
    Range(Vec<Range>),
    Boolean(Vec<Boolean>),
    Action(Vec<Action>),
    Battery(Vec<Battery>),
    Connection(ConnectionStatus),
    Equalizer(Equalizer),
    Sensors(Vec<Sensor>),
    Cooling(CoolingStatus),
    Lighting(LightingStatus),
    Dpi(DpiStatus),
    OnboardProfiles(OnboardProfiles),
    Lcd(LcdStatus),
    KeyRemap(KeyRemapStatus),
    KeyboardLayout(crate::keyboard::KeyboardLayoutStatus),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub category: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<VisibleWhen>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ScreenShape {
    Circle,
    Square,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdDescriptor {
    pub shape: ScreenShape,
    pub width: u32,
    pub height: u32,
    pub supported_rotations: Vec<crate::commands::ScreenRotation>,
    pub supported_image_types: Vec<String>,
    #[serde(default)]
    pub latches_last_frame: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LcdMode {
    #[default]
    Default,
    Image,
    Gif,
    Engine,
    Video,
    EditorPreview,
}

/// Orthogonal to `LcdMode`: whether the active content is actually rendering.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LcdHealth {
    #[default]
    Stable,
    Starting,
    Stopping,
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdStatus {
    pub descriptor: LcdDescriptor,
    pub brightness: u8,
    pub rotation: ScreenRotation,
    pub mode: LcdMode,
    /// Relative to lcd_images_dir; `None` in built-in mode.
    pub active_image: Option<String>,
    #[serde(default)]
    pub video_path: Option<String>,
    #[serde(default)]
    pub raw_streaming: bool,
    #[serde(default)]
    pub health: LcdHealth,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DpiMode {
    /// Stored in the active onboard profile (flash).
    Onboard,
    /// Software step list.
    Host,
}

/// `current_index < steps.len()` is not enforced; use `steps.get(current_index)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DpiStatus {
    pub steps: Vec<u16>,
    pub current_index: usize,
    pub current_dpi: u16,
    /// Used to validate edits.
    pub available_dpis: Vec<u16>,
    pub mode: DpiMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OnboardProfileSlot {
    /// 1-based; matches the writable RAM sector address.
    pub index: u16,
    #[serde(default)]
    pub read_only: bool,
    pub enabled: bool,
    pub active: bool,
    pub has_rom_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OnboardProfiles {
    /// 1-based; 0 when no profile is active (host mode).
    pub active_slot: u16,
    pub slots: Vec<OnboardProfileSlot>,
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
    pub device_id: String,
    pub channel_id: String,
    pub name: String,
    pub topology: ZoneTopology,
    pub led_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_order: Option<ColorOrder>,
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChoiceDisplay {
    List,
    #[default]
    Inline,
    /// Index 0 = off, 1 = on.
    Toggle,
}

/// Sibling value = a Choice's `selected`, a Range's `value`, or a Boolean's `0`/`1`;
/// a missing sibling shows the control (fail-open).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VisibleWhen {
    pub key: String,
    pub equals: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub key: String,
    pub label: String,
    pub options: Vec<ChoiceOption>,
    pub selected: usize,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub display: ChoiceDisplay,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<VisibleWhen>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RangeDisplay {
    #[default]
    Slider,
    Stepper,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Range {
    pub key: String,
    pub label: String,
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub value: i32,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub start_label: Option<String>,
    #[serde(default)]
    pub end_label: Option<String>,
    #[serde(default)]
    pub display: RangeDisplay,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<VisibleWhen>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceOption {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Boolean {
    pub key: String,
    pub value: bool,
    // Defaulted so `get_booleans` can return just `{ key, value }`; backfilled from `BooleanDef`.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub category: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<VisibleWhen>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BatteryStatus {
    Charging,
    #[default]
    Discharging,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Battery {
    pub key: String,
    pub label: String,
    pub level: u8,
    pub status: BatteryStatus,
}

/// Present only on wireless-capable devices.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectionStatus {
    pub connection_type: ConnectionType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqBand {
    pub index: usize,
    pub label: String,
    pub min: f32,
    pub max: f32,
    pub step: f32,
    pub value: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EqPreset {
    pub id: String,
    pub label: String,
    /// Rendered first and separated from the rest.
    #[serde(default)]
    pub is_custom: bool,
    /// Firmware presets write a device byte (their `bands` are informational);
    /// software presets push `bands` via the custom path.
    #[serde(default)]
    pub is_firmware: bool,
    /// `None` for no curve.
    #[serde(default)]
    pub bands: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Equalizer {
    pub presets: Vec<EqPreset>,
    pub selected_preset: usize,
    pub bands: Vec<EqBand>,
    /// The UI shows band sliders only when set.
    #[serde(default)]
    pub editable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SensorUnit {
    #[default]
    Celsius,
    Fahrenheit,
    Percent,
    Megahertz,
    Hours,
    Rpm,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SensorType {
    #[default]
    Temperature,
    Load,
    Memory,
    Frequency,
    Uptime,
    FanSpeed,
    FanDuty,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sensor {
    pub id: String,
    pub name: String,
    pub value: f64,
    pub unit: SensorUnit,
    #[serde(default)]
    pub sensor_type: SensorType,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CoolingChannelKind {
    #[default]
    Fan,
    Pump,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CoolingChannel {
    /// Stable and device-local; keys persisted curves.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: CoolingChannelKind,
    #[serde(default)]
    pub controllable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty: Option<u8>,
    /// `Hidden` drops the channel from the UI; `Disabled` also stops the fan curve driving it.
    #[serde(default)]
    pub visibility: VisibilityState,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CoolingStatus {
    #[serde(default)]
    pub channels: Vec<CoolingChannel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FanCurveStatus {
    #[default]
    Ok,
    /// sensor_id is None, or the sensor device could not be found.
    NoSensor,
    /// Sensor value has not changed for longer than the stale threshold.
    SensorMalfunction,
    /// e.g. permission denied on the sysfs pwm file.
    WriteError(String),
    /// Fan RPM has been 0 while curve duty is >20% for more than 10 s.
    FanStalled,
    /// Saved curve but device absent; no write was attempted.
    NoDevice,
    /// Probing at full duty produced no RPM; inert until any RPM > 0.
    Disconnected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireFanCurve {
    #[serde(default)]
    pub device_id: String,
    #[serde(default = "default_cooling_channel_id")]
    pub channel_id: String,
    pub sensor_id: Option<String>,
    /// [temp_celsius, duty_percent].
    pub points: Vec<[f32; 2]>,
    pub status: FanCurveStatus,
}

fn default_cooling_channel_id() -> String {
    "default".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WirePresetCurve {
    pub id: String,
    pub name: String,
    pub points: Vec<[f32; 2]>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseBtn {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModKey {
    Ctrl,
    Shift,
    Alt,
    Super,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaAction {
    VolumeUp,
    VolumeDown,
    Mute,
    Play,
    Next,
    Prev,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CycleDir {
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MacroAtom {
    KeyDown { key: u32 },
    KeyUp { key: u32 },
    MouseDown { btn: MouseBtn },
    MouseUp { btn: MouseBtn },
    Delay,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MacroStep {
    pub kind: MacroAtom,
    pub delay_after_ms: u32,
}

pub const MACRO_MAX_STEPS: usize = 512;
pub const MACRO_MAX_DELAY_MS: u32 = 60_000;
/// So a macro within the per-step and step-count limits still can't schedule input for hours.
pub const MACRO_MAX_TOTAL_MS: u64 = 600_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ButtonAction {
    /// No divert; firmware handles it.
    #[default]
    Native,
    /// Swallow the press.
    Disable,
    /// Emulate a mouse button click.
    MouseButton { btn: MouseBtn },
    /// Emulate scroll wheel movement.
    Scroll { axis: ScrollAxis, clicks: i32 },
    /// Emulate a key chord.
    KeyChord { key: u32, modifiers: Vec<ModKey> },
    /// Media/volume OS key.
    MediaKey { key: MediaAction },
    /// Host mode only.
    DpiCycle { direction: CycleDir },
    /// Non-host mode only.
    ProfileCycle { direction: CycleDir },
    /// Restored on release.
    MomentaryDpi { dpi: u16 },
    /// Global Layer Shift modifier.
    LayerShift,
    /// Runs asynchronously.
    Macro { steps: Vec<MacroStep> },
    /// Launch an application by path.
    OpenApp { path: String },
    /// Spawn a shell command.
    Command { cmd: String, args: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonDescriptor {
    /// As reported by getCidInfo.
    pub cid: u16,
    /// Derived from the task ID.
    pub label: String,
    pub divertable: bool,
    /// Buttons in the same group share a hardware slot.
    pub group: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonMapping {
    pub cid: u16,
    #[serde(default)]
    pub base: ButtonAction,
    #[serde(default)]
    pub shifted: ButtonAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRemapStatus {
    /// Order from getCidInfo.
    pub buttons: Vec<ButtonDescriptor>,
    /// Configured mappings, including explicit Native overrides of plugin defaults.
    pub mappings: Vec<ButtonMapping>,
    pub requires_host_mode: bool,
    pub host_mode_active: bool,
}

macro_rules! find_cap {
    ($name:ident, $variant:ident, $ty:ty) => {
        pub fn $name(&self) -> Option<&$ty> {
            self.capabilities.iter().find_map(|c| {
                if let DeviceCapability::$variant(s) = c {
                    Some(s)
                } else {
                    None
                }
            })
        }
    };
}

impl WireDevice {
    find_cap!(battery, Battery, Vec<Battery>);
    find_cap!(cooling, Cooling, CoolingStatus);
    find_cap!(lighting, Lighting, LightingStatus);
    find_cap!(sensors, Sensors, Vec<Sensor>);
    find_cap!(dpi, Dpi, DpiStatus);
    find_cap!(onboard_profiles, OnboardProfiles, OnboardProfiles);
    find_cap!(key_remap, KeyRemap, KeyRemapStatus);
    find_cap!(
        keyboard_layout,
        KeyboardLayout,
        crate::keyboard::KeyboardLayoutStatus
    );
    find_cap!(equalizer, Equalizer, Equalizer);
    find_cap!(children, Children, Vec<WireDevice>);
    find_cap!(pairing, Pairing, PairingStatus);
}

#[cfg(test)]
mod app_rule_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn app_rule_serde_round_trip() {
        let rule = AppRule {
            process_names: vec!["firefox".into(), "chrome".into()],
            profile: "Web".into(),
            enabled: true,
        };
        let json = serde_json::to_string(&rule).unwrap();
        let back: AppRule = serde_json::from_str(&json).unwrap();
        assert_eq!(back.process_names, rule.process_names);
        assert_eq!(back.profile, rule.profile);
        assert!(back.enabled);
    }

    #[test]
    fn app_rule_enabled_defaults_to_true() {
        let back: AppRule =
            serde_json::from_str(r#"{"process_names":["foo"],"profile":"P"}"#).unwrap();
        assert!(back.enabled);
    }

    #[test]
    fn gui_config_seen_tours_round_trip() {
        let mut cfg = GuiConfig::default();
        cfg.seen_tours.insert("page:home".into());
        cfg.seen_tours.insert("tab:cooling".into());
        let json = serde_json::to_string(&cfg).unwrap();
        let back: GuiConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seen_tours, cfg.seen_tours);
    }

    #[test]
    fn gui_config_seen_tours_defaults_empty_for_old_configs() {
        let cfg: GuiConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.seen_tours.is_empty());
    }

    fn widget(id: &str, kind: HomeWidgetKind) -> HomeWidget {
        HomeWidget {
            id: id.into(),
            kind,
            color: 0,
            label: String::new(),
        }
    }

    #[test]
    fn gui_config_home_widgets_round_trip() {
        let cfg = GuiConfig {
            home_widgets: vec![
                widget(
                    "a",
                    HomeWidgetKind::Chart {
                        sensor_id: "cpu".into(),
                    },
                ),
                widget(
                    "b",
                    HomeWidgetKind::Gauge {
                        sensor_id: "gpu".into(),
                        min: 0.0,
                        max: 100.0,
                    },
                ),
                widget(
                    "c",
                    HomeWidgetKind::Battery {
                        batteries: vec![battery_ref("mouse", "main")],
                    },
                ),
            ],
            ..GuiConfig::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: GuiConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.home_widgets, cfg.home_widgets);
    }

    #[test]
    fn gui_config_home_widgets_default_empty_for_old_configs() {
        let cfg: GuiConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.home_widgets.is_empty());
    }

    #[test]
    fn home_widget_kind_wire_format() {
        assert_eq!(
            serde_json::to_value(HomeWidgetKind::Gauge {
                sensor_id: "cpu".into(),
                min: 20.0,
                max: 90.0,
            })
            .unwrap(),
            json!({"type": "gauge", "sensor_id": "cpu", "min": 20.0, "max": 90.0})
        );
    }

    #[test]
    fn battery_ref_round_trips_device_ids_containing_the_separator() {
        let reference = battery_ref("hid/1234:5678", "left");
        assert_eq!(
            split_battery_ref(&reference),
            Some(("hid/1234:5678", "left"))
        );
    }

    #[test]
    fn validate_home_widgets_rejects_malformed_rows() {
        let chart = |id: &str| {
            widget(
                id,
                HomeWidgetKind::Chart {
                    sensor_id: "cpu".into(),
                },
            )
        };
        let cases = [
            (
                (0..=MAX_HOME_WIDGETS)
                    .map(|i| chart(&i.to_string()))
                    .collect::<Vec<_>>(),
                HomeWidgetsError::TooMany,
            ),
            (vec![chart("a"), chart("a")], HomeWidgetsError::DuplicateId),
            (vec![chart(" ")], HomeWidgetsError::EmptyId),
            (
                vec![widget(
                    "a",
                    HomeWidgetKind::Gauge {
                        sensor_id: "cpu".into(),
                        min: 50.0,
                        max: 50.0,
                    },
                )],
                HomeWidgetsError::EmptyRange,
            ),
            (
                vec![widget(
                    "a",
                    HomeWidgetKind::Battery {
                        batteries: (0..=MAX_BATTERY_WIDGET_ENTRIES)
                            .map(|i| battery_ref("d", &i.to_string()))
                            .collect(),
                    },
                )],
                HomeWidgetsError::TooManyBatteries,
            ),
        ];
        for (widgets, expected) in cases {
            assert_eq!(validate_home_widgets(&widgets), Err(expected));
        }
    }

    #[test]
    fn validate_home_widgets_accepts_a_full_row() {
        let widgets: Vec<_> = (0..MAX_HOME_WIDGETS)
            .map(|i| {
                widget(
                    &i.to_string(),
                    HomeWidgetKind::Chart {
                        sensor_id: "cpu".into(),
                    },
                )
            })
            .collect();
        assert_eq!(validate_home_widgets(&widgets), Ok(()));
    }

    #[test]
    fn plugin_config_field_new_metadata_has_backward_compatible_defaults() {
        let field: PluginConfigField =
            serde_json::from_str(r#"{"key":"host","label":"Host","kind":"host"}"#).unwrap();
        assert!(field.options.is_empty());
        assert!(field.visible_when.is_none());
        assert!(field.help.is_none());
        assert!(field.placeholder.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_language_matches_supported_regardless_of_region_or_charset() {
        assert_eq!(narrow_language("it-IT"), "it");
        assert_eq!(narrow_language("en_US.UTF-8"), "en");
        assert_eq!(narrow_language("IT"), "it");
    }

    #[test]
    fn narrow_language_falls_back_to_default_for_unsupported_or_empty() {
        assert_eq!(narrow_language("fr-FR"), DEFAULT_LANGUAGE);
        assert_eq!(narrow_language(""), DEFAULT_LANGUAGE);
        assert!(SUPPORTED_LANGUAGES.contains(&detect_system_language()));
    }

    #[test]
    fn notification_detail_and_severity_for_detail_codes() {
        let connect = NotificationCode::PluginConnectFailed {
            plugin: "wled".into(),
            detail: "127.0.0.1 blocked".into(),
        };
        let runtime = NotificationCode::PluginRuntimeError {
            plugin: "wled".into(),
            detail: "lua boom".into(),
        };
        let device = NotificationCode::DeviceWriteFailed {
            device: "keyboard".into(),
            detail: "HID++ rejected write".into(),
        };
        assert_eq!(connect.detail(), Some("127.0.0.1 blocked"));
        assert_eq!(runtime.detail(), Some("lua boom"));
        assert_eq!(device.detail(), Some("HID++ rejected write"));
        assert_eq!(device.severity(), NotificationSeverity::Error);
        assert_eq!(connect.severity(), NotificationSeverity::Warning);
        assert_eq!(
            NotificationCode::FanStalled { fan: "cpu".into() }.detail(),
            None
        );
    }

    #[test]
    fn plugin_issue_round_trips_and_info_defaults_to_healthy() {
        let issue = PluginIssue {
            kind: PluginIssueKind::ConnectFailed,
            detail: "boom".into(),
            context: None,
            timestamp_ms: 42,
        };
        let back: PluginIssue =
            serde_json::from_str(&serde_json::to_string(&issue).unwrap()).unwrap();
        assert_eq!(back, issue);
        assert_eq!(
            serde_json::to_value(&issue).unwrap()["kind"],
            "connect_failed"
        );
        let info: PluginInfo = serde_json::from_value(serde_json::json!({
            "id": "p", "name": "Weather", "description": "Weather provider",
            "path": "", "enabled": true,
            "translations": {"it": {
                "plugin.name": "Meteo",
                "plugin.description": "Servizio meteo"
            }}
        }))
        .unwrap();
        assert_eq!(info.health, HealthState::default());
        assert!(!info.experimental);
        assert_eq!(info.localized_name("it-CH"), "Meteo");
        assert_eq!(info.localized_description("it"), "Servizio meteo");
        assert_eq!(info.localized_name("de"), "Weather");

        let init_failed = PluginIssue {
            kind: PluginIssueKind::InitFailed,
            detail: "HID++ timeout".into(),
            context: None,
            timestamp_ms: 43,
        };
        assert_eq!(
            serde_json::to_value(&init_failed).unwrap()["kind"],
            "init_failed"
        );
    }

    #[test]
    fn plugin_info_defaults_active_requirements_and_blocker() {
        // Older daemon payloads must stay inert, never accidentally active.
        let info: PluginInfo = serde_json::from_value(serde_json::json!({
            "id": "p", "name": "P", "path": "", "enabled": true
        }))
        .unwrap();
        assert!(!info.active);
        assert!(info.requirements.is_empty());
        assert!(info.activation_blocker.is_none());
    }

    #[test]
    fn requirement_status_and_blocker_round_trip_snake_case() {
        let status = PluginRequirementStatus {
            requirement: PluginRequirement::KernelModule {
                name: "nct6775".into(),
            },
            impact: RequirementImpact::Block,
            satisfied: false,
            reason: Some(RequirementFailureReason::NotLoaded),
            feature: None,
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["requirement"]["kind"], "kernel_module");
        assert_eq!(value["impact"], "block");
        assert_eq!(value["reason"], "not_loaded");

        let blocker = PluginActivationBlocker::MissingRequirements {
            requirements: vec![status.clone()],
        };
        let back: PluginActivationBlocker =
            serde_json::from_str(&serde_json::to_string(&blocker).unwrap()).unwrap();
        assert_eq!(back, blocker);
        assert_eq!(
            serde_json::to_value(&blocker).unwrap()["type"],
            "missing_requirements"
        );

        let degrade = PluginRequirementStatus {
            requirement: PluginRequirement::Command {
                executable: "pactl".into(),
            },
            impact: RequirementImpact::Degrade,
            satisfied: false,
            reason: Some(RequirementFailureReason::NotFound),
            feature: Some("audio_routing".into()),
        };
        let back: PluginRequirementStatus =
            serde_json::from_str(&serde_json::to_string(&degrade).unwrap()).unwrap();
        assert_eq!(back, degrade);

        assert_eq!(
            serde_json::to_value(PluginRequirement::PawnIo).unwrap()["kind"],
            "pawn_io"
        );
        assert_eq!(
            serde_json::to_value(PluginRequirement::LinuxI2c).unwrap()["kind"],
            "linux_i2c"
        );
    }

    #[test]
    fn plugin_recommendation_round_trips_and_defaults_accessible() {
        let rec = PluginRecommendation {
            plugin_id: "nzxt_kraken".into(),
            plugin_name: "NZXT Kraken".into(),
            hardware: PluginRecommendationMatch::Hid {
                vid: 0x1e71,
                pid: 0x2007,
            },
            accessible: false,
        };
        let back: PluginRecommendation =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(back, rec);
        let old: PluginRecommendation = serde_json::from_value(serde_json::json!({
            "plugin_id": "p", "plugin_name": "P",
            "hardware": { "transport": "usb", "vid": 1, "pid": 2 }
        }))
        .unwrap();
        assert!(old.accessible);

        for hardware in [
            PluginRecommendationMatch::Always,
            PluginRecommendationMatch::Command {
                executable: "nvidia-smi".into(),
            },
        ] {
            let value = serde_json::to_value(&hardware).unwrap();
            let back: PluginRecommendationMatch = serde_json::from_value(value).unwrap();
            assert_eq!(back, hardware);
        }
    }

    #[test]
    fn load_failed_and_skipped_round_trip() {
        let failed = PluginIssue {
            kind: PluginIssueKind::LoadFailed,
            detail: "needs network".into(),
            context: None,
            timestamp_ms: 1,
        };
        assert_eq!(
            serde_json::to_value(&failed).unwrap()["kind"],
            "load_failed"
        );
        let skipped = SkippedPlugin {
            path: "/a/b".into(),
            reason: "bad yaml".into(),
        };
        let back: SkippedPlugin =
            serde_json::from_str(&serde_json::to_string(&skipped).unwrap()).unwrap();
        assert_eq!(back, skipped);
    }

    #[test]
    fn effect_param_value_steps_round_trips_untagged() {
        let v = EffectParamValue::Steps(vec![
            ColorStep {
                value: 40.0,
                color: RgbColor { r: 0, g: 255, b: 0 },
            },
            ColorStep {
                value: 80.0,
                color: RgbColor { r: 255, g: 0, b: 0 },
            },
        ]);
        let json = serde_json::to_string(&v).unwrap();
        let back: EffectParamValue = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);

        // The array encoding must not shadow the object-encoded Color variant.
        let c = EffectParamValue::Color(RgbColor { r: 1, g: 2, b: 3 });
        let json = serde_json::to_string(&c).unwrap();
        let back: EffectParamValue = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn connection_capability_roundtrips() {
        let cap = DeviceCapability::Connection(ConnectionStatus {
            connection_type: ConnectionType::Wireless,
        });
        let json = serde_json::to_string(&cap).unwrap();
        assert!(json.contains("\"kind\":\"connection\""));
        let back: DeviceCapability = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            DeviceCapability::Connection(ConnectionStatus {
                connection_type: ConnectionType::Wireless
            })
        ));
    }

    #[test]
    fn notification_roundtrip() {
        let n = Notification {
            code: NotificationCode::DeviceInitFailed {
                device: "Kraken".into(),
                detail: "thing exploded".into(),
            },
            show_native: true,
            timestamp_ms: 1234,
        };
        let json = serde_json::to_value(&n).unwrap();
        assert_eq!(json["code"], "device_init_failed");
        assert_eq!(json["device"], "Kraken");
        assert_eq!(json["detail"], "thing exploded");
        assert_eq!(json["timestamp_ms"], 1234);
        assert_eq!(json["showNative"], true);
        let back: Notification = serde_json::from_value(json).unwrap();
        assert_eq!(back.code, n.code);
        assert!(back.show_native);
        assert_eq!(back.code.severity(), NotificationSeverity::Error);
        assert_eq!(back.timestamp_ms, 1234);
    }

    #[test]
    fn capability_accessors_find_only_their_own_kind() {
        let caps = vec![
            DeviceCapability::Battery(vec![]),
            DeviceCapability::Lighting(LightingStatus {
                descriptor: LightingDescriptor {
                    channels: vec![],
                    native_effects: vec![],
                },
                state: None,
                channel_transforms: HashMap::new(),
            }),
            DeviceCapability::Sensors(vec![]),
            DeviceCapability::Children(vec![]),
            DeviceCapability::Pairing(PairingStatus::default()),
        ];
        let full = WireDevice {
            capabilities: caps,
            ..Default::default()
        };
        assert!(full.battery().is_some());
        assert!(full.lighting().is_some());
        assert!(full.sensors().is_some());
        assert!(full.children().is_some());
        assert!(full.pairing().is_some());
        let other = WireDevice {
            capabilities: vec![DeviceCapability::Cooling(CoolingStatus {
                channels: vec![],
            })],
            ..Default::default()
        };
        assert!(other.battery().is_none());
        assert!(other.lighting().is_none());
        assert!(other.pairing().is_none());
    }

    #[test]
    fn button_actions_round_trip_and_default_to_native() {
        assert_eq!(
            serde_json::to_string(&ButtonAction::default()).unwrap(),
            r#"{"type":"native"}"#
        );
        for action in [
            ButtonAction::Native,
            ButtonAction::KeyChord {
                key: 42,
                modifiers: vec![ModKey::Ctrl, ModKey::Shift],
            },
            ButtonAction::Macro {
                steps: vec![MacroStep {
                    kind: MacroAtom::KeyDown { key: 7 },
                    delay_after_ms: 50,
                }],
            },
            ButtonAction::DpiCycle {
                direction: CycleDir::Up,
            },
        ] {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(serde_json::from_str::<ButtonAction>(&json).unwrap(), action);
        }
    }

    #[test]
    fn eq_preset_defaults_when_flags_absent() {
        let p: EqPreset =
            serde_json::from_value(serde_json::json!({"id": "custom", "label": "Custom"})).unwrap();
        assert!(!p.is_custom);
        assert!(!p.is_firmware);
        assert_eq!(p.bands, None);
    }

    proptest::proptest! {
        #[test]
        fn eq_preset_json_round_trips(
            id in ".{0,32}",
            label in ".{0,32}",
            is_custom: bool,
            is_firmware: bool,
            bands in proptest::option::of(
                proptest::collection::vec(-100.0f32..100.0, 0..12)
            ),
        ) {
            let p = EqPreset { id, label, is_custom, is_firmware, bands };
            let json = serde_json::to_string(&p).unwrap();
            let back: EqPreset = serde_json::from_str(&json).unwrap();
            proptest::prop_assert_eq!(back, p);
        }
    }
}

#[cfg(test)]
mod default_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn placed_zone_defaults_when_size_and_rotation_omitted() {
        let z: PlacedZone =
            serde_json::from_str(r#"{"device_id":"d","channel_id":"z","x":1.0,"y":2.0}"#).unwrap();
        assert_eq!(z.w, 0.15, "default_zone_size");
        assert_eq!(z.h, 0.15, "default_zone_size");
        assert_eq!(z.rotation, 0.0, "default_rotation");
    }

    #[test]
    fn canvas_state_defaults_sample_radius_when_omitted() {
        let s: CanvasState =
            serde_json::from_str(r#"{"available_effects":[],"placed_zones":[]}"#).unwrap();
        assert_eq!(s.sample_radius, 3.0, "default_sample_radius");
    }

    #[test]
    fn gui_config_defaults_close_to_tray_when_omitted() {
        let c: GuiConfig = serde_json::from_str(r#"{"language":"en","log_level":"info"}"#).unwrap();
        assert!(c.close_to_tray, "default_close_to_tray");
        assert!(c.low_battery_notifications);
        assert!(
            !c.suppress_dependency_warning,
            "default_suppress_dependency_warning"
        );
    }

    #[test]
    fn gui_config_defaults_plugin_downloads_to_allowed_when_omitted() {
        let c: GuiConfig = serde_json::from_str(r#"{"language":"en","log_level":"info"}"#).unwrap();
        assert_eq!(c.plugin_downloads, PluginDownloadConsent::Allowed);
    }

    #[test]
    fn plugin_download_consent_round_trips() {
        for v in [
            PluginDownloadConsent::Unset,
            PluginDownloadConsent::Allowed,
            PluginDownloadConsent::Denied,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: PluginDownloadConsent = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
        assert_eq!(
            serde_json::to_string(&PluginDownloadConsent::Allowed).unwrap(),
            "\"allowed\""
        );
    }

    #[test]
    fn validate_image_filename_accepts_uuid_image_names() {
        assert!(validate_image_filename("0e7d4c2a-1234-5678-9abc-def012345678.png").is_ok());
        assert!(validate_image_filename("pic_1.jpg").is_ok());
        assert!(validate_image_filename("a.gif").is_ok());
    }

    #[test]
    fn validate_image_filename_rejects_traversal_and_paths() {
        for bad in [
            "../secret.png",
            "a/b.png",
            "a\\b.png",
            "C:\\x.png",
            ".hidden.png",
            "noext",
            "x.txt",
            "name.png:stream",
            "",
        ] {
            assert!(
                validate_image_filename(bad).is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn validate_logo_dimensions_accepts_square_and_mild_rectangles() {
        assert!(validate_logo_dimensions(64, 64).is_ok());
        assert!(validate_logo_dimensions(MAX_PLUGIN_LOGO_DIM, MAX_PLUGIN_LOGO_DIM).is_ok());
        assert!(validate_logo_dimensions(200, 100).is_ok());
        assert!(validate_logo_dimensions(100, 200).is_ok());
        assert!(validate_logo_dimensions(201, 100).is_err());
    }

    #[test]
    fn validate_logo_dimensions_rejects_zero_oversize_and_banners() {
        assert!(validate_logo_dimensions(0, 64).is_err());
        assert!(validate_logo_dimensions(64, 0).is_err());
        assert!(validate_logo_dimensions(MAX_PLUGIN_LOGO_DIM + 1, 64).is_err());
        assert!(validate_logo_dimensions(64, MAX_PLUGIN_LOGO_DIM + 1).is_err());
        assert!(validate_logo_dimensions(500, 50).is_err());
    }

    #[test]
    fn validate_image_upload_size_gates_at_the_ceiling() {
        assert!(validate_image_upload_size(0).is_ok());
        assert!(validate_image_upload_size(MAX_LCD_IMAGE_BYTES).is_ok());
        assert!(validate_image_upload_size(MAX_LCD_IMAGE_BYTES + 1).is_err());
        assert!(validate_image_upload_size(u64::MAX).is_err());
    }

    #[test]
    fn canvas_state_sanitize_cleans_nonfinite_and_caps_zones() {
        let mut cs = CanvasState {
            placed_zones: (0..MAX_PLACED_ZONES + 10)
                .map(|_| PlacedZone::default())
                .collect(),
            ..CanvasState::default()
        };
        cs.placed_zones[0].x = f32::NAN;
        cs.placed_zones[0].rotation = f32::INFINITY;
        cs.placed_zones[0].w = -1.0;
        cs.placed_zones[0].h = 100.0;
        cs.sample_radius = -3.0;
        cs.sanitize();
        assert_eq!(cs.placed_zones.len(), MAX_PLACED_ZONES);
        assert!(cs.placed_zones[0].x.is_finite());
        assert!(cs.placed_zones[0].rotation.is_finite());
        assert!(cs.placed_zones[0].w > 0.0);
        assert_eq!(cs.placed_zones[0].h, 4.0);
        assert_eq!(cs.sample_radius, 0.5);
    }

    #[test]
    fn max_lcd_image_base64_fits_in_one_ipc_frame() {
        // An accepted image's base64 (≈4/3) plus envelope must fit the frame cap,
        // or the upload the size check "passes" would be rejected at the wire.
        let base64_len = MAX_LCD_IMAGE_BYTES.div_ceil(3) * 4;
        assert!(base64_len < crate::frames::MAX_PAYLOAD as u64);
    }

    #[test]
    fn sniff_ext_detects_formats() {
        assert_eq!(sniff_ext(b"GIF89aXXX"), "gif");
        assert_eq!(sniff_ext(b"GIF87aXXX"), "gif");
        assert_eq!(sniff_ext(&[0xFF, 0xD8, 0x00]), "jpg");
        assert_eq!(sniff_ext(&[0x89, 0x50, 0x4E, 0x47]), "png");
        assert_eq!(sniff_ext(b"anything"), "unknown");
    }

    #[test]
    fn battery_status_wire_form_is_snake_case() {
        // Lowercase wire form crosses the IPC boundary.
        assert_eq!(
            serde_json::to_value(BatteryStatus::Charging).unwrap(),
            json!("charging")
        );
        assert_eq!(
            serde_json::to_value(BatteryStatus::Discharging).unwrap(),
            json!("discharging")
        );
        assert_eq!(
            serde_json::to_value(BatteryStatus::Unknown).unwrap(),
            json!("unknown")
        );
        let s: BatteryStatus = serde_json::from_value(json!("charging")).unwrap();
        assert_eq!(s, BatteryStatus::Charging);
    }

    #[test]
    fn write_rate_status_defaults_to_no_limit() {
        assert_eq!(WriteRateStatus::default().limit, None);
    }

    #[test]
    fn wire_device_optional_fields_default_when_omitted() {
        let d: WireDevice = serde_json::from_str(
            r#"{"id":"d","name":"d","vendor":"v","model":"m","device_type":"other","connected":true,"capabilities":[]}"#,
        )
        .unwrap();
        assert_eq!(d.write_rate, None);
        assert!(d.control_layout.is_empty());
    }

    #[test]
    fn choice_display_defaults_to_inline() {
        // Must match the inline pills the Controls tab hardcoded before reading this field.
        assert_eq!(ChoiceDisplay::default(), ChoiceDisplay::Inline);
        let c: Choice =
            serde_json::from_str(r#"{"key":"k","label":"K","options":[],"selected":0}"#).unwrap();
        assert_eq!(c.display, ChoiceDisplay::Inline);
    }

    #[test]
    fn category_layout_defaults_span_to_one_when_omitted() {
        let l: CategoryLayout = serde_json::from_str(r#"{"category":"Audio"}"#).unwrap();
        assert_eq!(l.order, 0);
        assert_eq!(l.column, 0);
        assert_eq!(l.span, 1);
    }

    #[test]
    fn visible_when_absent_when_none() {
        let c = Choice {
            key: "k".into(),
            label: "K".into(),
            options: vec![],
            selected: 0,
            category: String::new(),
            display: ChoiceDisplay::default(),
            visible_when: None,
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("visible_when"));
    }

    #[test]
    fn visible_when_round_trips_when_present() {
        let r = Range {
            key: "nc_level".into(),
            label: "Transparency Level".into(),
            min: 1,
            max: 10,
            step: 1,
            value: 5,
            read_only: false,
            category: "Noise Cancelling".into(),
            start_label: None,
            end_label: None,
            display: RangeDisplay::default(),
            visible_when: Some(VisibleWhen {
                key: "nc_mode".into(),
                equals: vec![1],
            }),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: Range = serde_json::from_str(&json).unwrap();
        assert_eq!(back.visible_when, r.visible_when);
    }
}

#[cfg(test)]
mod rgb_state_tests {
    use super::*;

    #[test]
    fn per_led_uses_string_keys_for_led_ids() {
        let state = LightingState::PerLed {
            channels: {
                let mut channels = std::collections::HashMap::new();
                let mut led_map = std::collections::HashMap::new();
                led_map.insert("12".to_string(), RgbColor { r: 255, g: 0, b: 0 });
                channels.insert("z0".to_string(), led_map);
                channels
            },
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["channels"]["z0"]["12"]["r"], 255);
        let back: LightingState = serde_json::from_value(json).unwrap();
        assert!(matches!(back, LightingState::PerLed { .. }));
    }

    #[test]
    fn lcd_preview_lease_is_at_least_double_the_keepalive() {
        // One lost/late keepalive must never flap the preview stream.
        assert!(LCD_PREVIEW_LEASE_SECS as f64 >= 2.0 * LCD_PREVIEW_KEEPALIVE_SECS);
    }
}

#[cfg(test)]
mod color_order_tests {
    use super::*;

    #[test]
    fn color_orders_cover_every_rgb_permutation() {
        let color = RgbColor { r: 1, g: 2, b: 3 };
        assert_eq!(ColorOrder::Rgb.encode(color), [1, 2, 3]);
        assert_eq!(ColorOrder::Rbg.encode(color), [1, 3, 2]);
        assert_eq!(ColorOrder::Grb.encode(color), [2, 1, 3]);
        assert_eq!(ColorOrder::Gbr.encode(color), [2, 3, 1]);
        assert_eq!(ColorOrder::Brg.encode(color), [3, 1, 2]);
        assert_eq!(ColorOrder::Bgr.encode(color), [3, 2, 1]);
    }
}
