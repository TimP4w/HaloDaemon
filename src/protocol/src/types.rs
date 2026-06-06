use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::zone_transform::ZoneContentTransform;

pub const DEFAULT_PROFILE_NAME: &str = "default";

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
    /// device_id → template_id
    pub device_templates: HashMap<String, String>,
}

/// A rendered LCD engine frame broadcast to subscribed clients.
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
    pub zone_id: String,
    pub led_id: u32,
    pub color: RgbColor,
}

/// A rendered canvas frame broadcast to subscribed clients.
///
/// frame_id is monotonically increasing; gaps indicate frames the daemon dropped
/// (broadcast channel Lagged). Frontend derives:
///   FPS  = frames_received / elapsed_seconds (rolling 1s window using timestamp_ms)
///   Lost = Σ (frame_id[n] - frame_id[n-1] - 1) across received frames
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanvasFrame {
    pub frame_id: u64,
    pub timestamp_ms: u64,
    pub canvas_srgb_b64: String,
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub led_colors: Vec<LedFrameEntry>,
}

fn default_zone_size() -> f32 {
    0.15
}
fn default_rotation() -> f32 {
    0.0
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WirePlacedZone {
    pub device_id: String,
    pub zone_id: String,
    pub x: f32,
    pub y: f32,
    #[serde(default = "default_zone_size")]
    pub w: f32,
    #[serde(default = "default_zone_size")]
    pub h: f32,
    #[serde(default = "default_rotation")]
    pub rotation: f32,
}

fn default_sample_radius() -> f32 {
    3.0
}

fn default_close_to_tray() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WireCanvasState {
    pub active_effect_id: Option<String>,
    pub available_effects: Vec<Animation>,
    pub placed_zones: Vec<WirePlacedZone>,
    #[serde(default = "default_sample_radius")]
    pub sample_radius: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub engine_fan_curve_enabled: bool,
    /// Milliseconds between fan-curve ticks.
    pub engine_fan_curve_tick_ms: u64,
    pub engine_canvas_enabled: bool,
    pub engine_canvas_fps: u32,
    pub engine_lcd_enabled: bool,
    pub engine_lcd_fps: u32,
    /// Duty (0-100) applied when a fan's assigned sensor is absent.
    pub fan_failsafe_duty: u8,
    pub log_level: String,
    #[serde(default = "default_close_to_tray")]
    pub close_to_tray: bool,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            engine_fan_curve_enabled: true,
            engine_fan_curve_tick_ms: 2000,
            engine_canvas_enabled: true,
            engine_canvas_fps: 20,
            engine_lcd_enabled: true,
            engine_lcd_fps: 20,
            fan_failsafe_duty: 75,
            log_level: "info".to_string(),
            close_to_tray: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub level: String,
    pub target: String,
    pub message: String,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub severity: NotificationSeverity,
    pub title: String,
    pub message: String,
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

fn bool_true() -> bool { true }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default)]
    pub discovery: DiscoveryStatus,
    #[serde(default)]
    pub devices: Vec<WireDevice>,
    #[serde(default)]
    pub active_profile: String,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub fan_curves: Vec<WireFanCurve>,
    #[serde(default)]
    pub preset_curves: Vec<WirePresetCurve>,
    #[serde(default)]
    pub canvas: WireCanvasState,
    #[serde(default)]
    pub lcd_engine: WireLcdEngineState,
    #[serde(default)]
    pub global_config: GlobalConfig,
    #[serde(default)]
    pub log_entries: Vec<LogEntry>,
    /// Filesystem path to the daemon's config directory. The daemon is the
    /// single source of truth — the UI displays and opens this path without
    /// recomputing it client-side.
    #[serde(default)]
    pub config_dir: String,
    #[serde(default)]
    pub app_rules: Vec<AppRule>,
    /// False when no supported compositor/platform backend was found.
    /// Default false so the UI doesn't flash "supported" before the first broadcast.
    #[serde(default)]
    pub focus_watcher_supported: bool,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoveryStatus {
    #[serde(default)]
    pub phase: DiscoveryPhase,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeviceType {
    #[default]
    Other,
    Fan,
    Hub,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum DeviceCapability {
    Children(Vec<WireDevice>),
    Choice(Vec<Choice>),
    Range(Vec<Range>),
    Boolean(Vec<Boolean>),
    Action(Vec<Action>),
    Battery(Vec<Battery>),
    Equalizer(Equalizer),
    Sensors(Vec<Sensor>),
    Fan(FanStatus),
    Pump(PumpStatus),
    Rgb(RgbStatus),
    Dpi(DpiStatus),
    OnboardProfiles(OnboardProfiles),
    Lcd(LcdStatus),
    KeyRemap(KeyRemapStatus),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub category: String,
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
    pub supported_rotations: Vec<u32>,
    pub supported_image_types: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LcdMode {
    #[default]
    Default,
    Image,
    Gif,
    /// LCD is controlled by the LCD engine (template-driven).
    Engine,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdStatus {
    pub descriptor: LcdDescriptor,
    pub brightness: u8,
    pub rotation: u32,
    pub mode: LcdMode,
    /// Filename only (relative to lcd_images_dir). None when in default/built-in mode.
    pub active_image: Option<String>,
}

/// Which layer the device's DPI is currently managed by.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DpiMode {
    /// DPI steps are stored in the device's active onboard profile (flash).
    Onboard,
    /// DPI is managed by the host (software step list, host mode).
    Host,
}

/// Unified DPI state for a pointing device. In `Onboard` mode the steps come
/// from the device's active onboard profile; in `Host` mode they come from the
/// software-managed step list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DpiStatus {
    /// Ordered DPI steps (onboard profile slots, or the software step list).
    pub steps: Vec<u16>,
    /// Index of the currently active step within `steps`.
    pub current_index: usize,
    /// Currently active DPI value reported by / applied to the device.
    pub current_dpi: u16,
    /// Full list of DPI values the hardware accepts (used to validate edits).
    pub available_dpis: Vec<u16>,
    /// Whether DPI is currently managed onboard or by the host.
    pub mode: DpiMode,
}

/// One onboard-profile slot stored in the device's flash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OnboardProfileSlot {
    /// 1-based slot index (matches the writable RAM sector address).
    pub index: u8,
    /// Whether the slot is enabled in the device's profile directory.
    pub enabled: bool,
    /// Whether this slot is the device's currently active profile.
    pub active: bool,
    /// Whether the slot is backed by a factory ROM profile (restorable).
    pub has_rom_default: bool,
}

/// Onboard (on-device) profile management for HID++ mice.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OnboardProfiles {
    /// 1-based index of the active slot; 0 when no profile is active (host mode).
    pub active_slot: u8,
    pub slots: Vec<OnboardProfileSlot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Tells the frontend how to render the zone widget.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardFormFactor {
    FullSize,
    TKL,
    Compact75,
    Compact65,
    Compact60,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardLayout {
    CH,
    IT,
    US,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ZoneTopology {
    Ring,
    Rings { count: u8 },
    Linear,
    Grid,
    Keyboard {
        form_factor: KeyboardFormFactor,
        layout: KeyboardLayout,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RgbZone {
    pub id: String,
    pub name: String,
    pub topology: ZoneTopology,
    pub leds: Vec<LedPosition>,
}

/// Widget type for one effect parameter
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParamKind {
    Range { min: f64, max: f64, step: f64 },
    Enum { options: Vec<String> },
    Color,
    Boolean,
    /// Free-text entry. Value is an `EffectParamValue::Str`.
    Text,
    /// Sensor picker. Value is an `EffectParamValue::Str` holding the sensor id.
    Sensor,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EffectParamValue {
    Float(f64),
    Str(String),
    Color(RgbColor),
    Bool(bool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RgbState {
    Static {
        color: RgbColor,
    },
    // LED IDs use String keys because JSON object keys are always strings and
    // serde's u32 visitor does not implement visit_str for roundtrip conversion.
    PerLed {
        zones: HashMap<String, HashMap<String, RgbColor>>,
    },
    NativeEffect {
        id: String,
        params: HashMap<String, EffectParamValue>,
    },
    /// Device is under control of the RGB canvas engine. Frontend should disable other controls and not attempt to read or write state while in this mode.
    Engine,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RgbDescriptor {
    pub zones: Vec<RgbZone>,
    pub native_effects: Vec<NativeEffect>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RgbStatus {
    pub descriptor: RgbDescriptor,
    pub state: Option<RgbState>,
    /// Zones absent from the map use the identity transform.
    #[serde(default)]
    pub zone_transforms: HashMap<String, ZoneContentTransform>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chainable_channels: Vec<ChainableChannelInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainableChannelInfo {
    pub channel_id: String,
    pub name: String,
    pub max_leds: u32,
    /// String the UI echoes back as `kind` in `RgbChainAddLink` (e.g.
    /// `"generic_aura_argb"`).
    pub link_kind: String,
    pub links: Vec<ChainLinkInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainLinkInfo {
    pub child_device_id: String,
    pub name: String,
    pub topology: ZoneTopology,
    pub led_count: u32,
    /// Hardware-detected links cannot be removed, reordered, or renamed
    /// through chain IPC commands; only the hardware controls them.
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChoiceDisplay {
    /// Dropdown list (default).
    #[default]
    List,
    /// All options shown as inline toggle buttons.
    Inline,
    /// Two-option choice rendered as an on/off switch (index 0 = off, 1 = on).
    Toggle,
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
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceOption {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Boolean {
    pub key: String,
    pub label: String,
    pub value: bool,
    pub read_only: bool,
    #[serde(default)]
    pub category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqBand {
    pub index: usize,
    pub label: String,
    pub min: f32,
    pub max: f32,
    pub step: f32,
    pub value: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Equalizer {
    pub presets: Vec<ChoiceOption>,
    pub selected_preset: usize,
    pub bands: Vec<EqBand>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SensorUnit {
    #[default]
    Celsius,
    Fahrenheit,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SensorType {
    #[default]
    Temperature,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sensor {
    pub id: String,
    pub name: String,
    pub value: f64,
    pub unit: SensorUnit,
    #[serde(default)]
    pub sensor_type: SensorType,
    #[serde(default)]
    pub visibility: VisibilityState,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FanStatus {
    pub channel: u8,
    pub rpm: u32,
    pub duty: u8,
    /// Whether the fan duty can be set (false when no fan is connected on this channel).
    pub controllable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PumpStatus {
    pub rpm: u32,
    pub duty: u8,
    pub controllable: bool,
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
    /// set_duty syscall failed (e.g. permission denied on sysfs pwm file).
    WriteError(String),
    /// Fan RPM has been 0 while curve duty is >20% for more than 10 s.
    FanStalled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireFanCurve {
    pub fan_id: String,
    pub sensor_id: Option<String>,
    /// Points as [temp_celsius, duty_percent] pairs.
    pub points: Vec<[f32; 2]>,
    pub status: FanCurveStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WirePresetCurve {
    pub id: String,
    pub name: String,
    pub points: Vec<[f32; 2]>,
}

// ── Key Remapper types ────────────────────────────────────────────────────────

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

/// Action to execute when a button event fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ButtonAction {
    /// Leave button as-is (no divert — firmware handles it normally).
    Native,
    /// Swallow the press, do nothing.
    Disable,
    /// Emulate a mouse button click.
    MouseButton { btn: MouseBtn },
    /// Emulate scroll wheel movement.
    Scroll { axis: ScrollAxis, clicks: i32 },
    /// Emulate a keyboard key chord (key + modifiers).
    KeyChord { key: u32, modifiers: Vec<ModKey> },
    /// Send a media/volume OS key.
    MediaKey { key: MediaAction },
    /// Cycle the software DPI step list up or down (host mode only).
    DpiCycle { direction: CycleDir },
    /// Cycle the onboard profile (non-host mode only).
    ProfileCycle { direction: CycleDir },
    /// Temporarily apply a specific DPI while the button is held; restore on release.
    MomentaryDpi { dpi: u16 },
    /// Marks this button as the global Layer Shift modifier key.
    LayerShift,
    /// Execute a sequence of input steps (runs asynchronously).
    Macro { steps: Vec<MacroStep> },
    /// Launch an application by path.
    OpenApp { path: String },
    /// Spawn a shell command.
    Command { cmd: String, args: Vec<String> },
}

impl Default for ButtonAction {
    fn default() -> Self {
        Self::Native
    }
}

/// Description of a physical remappable button, read from the device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonDescriptor {
    /// Control ID as reported by the device (getCidInfo).
    pub cid: u16,
    /// Human-readable label derived from the task ID (e.g. "Left Button").
    pub label: String,
    /// Whether this button can be diverted to host control.
    pub divertable: bool,
    /// Mutual-exclusion group — buttons in the same group share a hardware slot.
    pub group: u8,
}

/// Configured mapping for one physical button.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ButtonMapping {
    pub cid: u16,
    #[serde(default)]
    pub base: ButtonAction,
    #[serde(default)]
    pub shifted: ButtonAction,
}

/// Full key-remap state for one device, sent to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRemapStatus {
    /// All remappable buttons the device reports (order from getCidInfo).
    pub buttons: Vec<ButtonDescriptor>,
    /// Only entries that differ from Native in at least one action.
    pub mappings: Vec<ButtonMapping>,
    /// True for Logitech — the UI shows a "requires host mode" notice when false.
    pub requires_host_mode: bool,
    pub host_mode_active: bool,
}

// ── WireDevice capability accessors ──────────────────────────────────────────

macro_rules! find_cap {
    ($name:ident, $variant:ident, $ty:ty) => {
        pub fn $name(&self) -> Option<&$ty> {
            self.capabilities.iter().find_map(|c|
                if let DeviceCapability::$variant(s) = c { Some(s) } else { None })
        }
    };
}

impl WireDevice {
    find_cap!(battery,          Battery,         Vec<Battery>);
    find_cap!(fan,              Fan,             FanStatus);
    find_cap!(pump,             Pump,            PumpStatus);
    find_cap!(rgb,              Rgb,             RgbStatus);
    find_cap!(sensors,          Sensors,         Vec<Sensor>);
    find_cap!(dpi,              Dpi,             DpiStatus);
    find_cap!(onboard_profiles, OnboardProfiles, OnboardProfiles);
    find_cap!(key_remap,        KeyRemap,        KeyRemapStatus);
    find_cap!(equalizer,        Equalizer,       Equalizer);
    find_cap!(children,         Children,        Vec<WireDevice>);
}

#[cfg(test)]
mod app_rule_tests {
    use super::*;

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
    fn app_state_defaults_empty_app_rules() {
        let state: AppState = serde_json::from_str("{}").unwrap();
        assert!(state.app_rules.is_empty());
    }

    #[test]
    fn app_rule_enabled_defaults_to_true() {
        let back: AppRule = serde_json::from_str(
            r#"{"process_names":["foo"],"profile":"P"}"#
        ).unwrap();
        assert!(back.enabled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_type_roundtrip() {
        let d = WireDevice {
            id: "x".into(),
            name: "x".into(),
            vendor: "x".into(),
            model: "x".into(),
            device_type: DeviceType::Mouse,
            connected: true,
            capabilities: vec![],
            connection_type: Some(ConnectionType::Wired),
            serial_number: None,
            ..Default::default()
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"connection_type\":\"wired\""));
        let back: WireDevice = serde_json::from_str(&json).unwrap();
        assert_eq!(back.connection_type, Some(ConnectionType::Wired));
    }

    #[test]
    fn connection_type_absent_when_none() {
        let d = WireDevice {
            id: "x".into(),
            name: "x".into(),
            vendor: "x".into(),
            model: "x".into(),
            device_type: DeviceType::Mouse,
            connected: true,
            capabilities: vec![],
            connection_type: None,
            serial_number: None,
            ..Default::default()
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("connection_type"));
    }

    #[test]
    fn notification_severity_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&NotificationSeverity::Info).unwrap(),
            "\"info\""
        );
        assert_eq!(
            serde_json::to_string(&NotificationSeverity::Warning).unwrap(),
            "\"warning\""
        );
        assert_eq!(
            serde_json::to_string(&NotificationSeverity::Error).unwrap(),
            "\"error\""
        );
    }

    #[test]
    fn notification_roundtrip() {
        let n = Notification {
            severity: NotificationSeverity::Error,
            title: "Boom".into(),
            message: "thing exploded".into(),
            timestamp_ms: 1234,
        };
        let json = serde_json::to_string(&n).unwrap();
        let back: Notification = serde_json::from_str(&json).unwrap();
        assert_eq!(back.severity, NotificationSeverity::Error);
        assert_eq!(back.title, "Boom");
        assert_eq!(back.message, "thing exploded");
        assert_eq!(back.timestamp_ms, 1234);
    }
}
