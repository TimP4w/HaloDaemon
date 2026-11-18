// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthCheckState {
    /// Default false so the UI doesn't flash "supported" before the first broadcast.
    #[serde(default)]
    pub focus_watcher_supported: bool,
    /// Gates LCD video mode.
    #[serde(default)]
    pub ffmpeg_available: bool,
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

/// The order full discovery runs its scanners in, for progress displays.
pub const DISCOVERY_STEPS: &[DiscoveryDetail] = &[
    DiscoveryDetail::Computer,
    DiscoveryDetail::Hid,
    DiscoveryDetail::Usb,
    DiscoveryDetail::Smbus,
    DiscoveryDetail::PluginHostTransports,
    DiscoveryDetail::PluginIntegrations,
];

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
    pub id: crate::ids::DeviceId,
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
    pub integration_id: Option<crate::ids::PluginId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<crate::ids::DeviceId>,
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
    pub peer_ids: Vec<crate::ids::DeviceId>,
    pub recommended_id: crate::ids::DeviceId,
    pub confidence: ConflictConfidence,
    /// Lets clients explain an integration/plugin choice without guessing from the device id.
    #[serde(default)]
    pub participants: Vec<ConflictParticipant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictParticipant {
    pub id: crate::ids::DeviceId,
    pub source: ConflictDeviceSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConflictDeviceSource {
    #[default]
    Builtin,
    Plugin(crate::ids::PluginId),
    Integration(crate::ids::PluginId),
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
    pub device_id: crate::ids::DeviceId,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sensor {
    pub id: String,
    pub name: String,
    pub value: f64,
    pub unit: SensorUnit,
    #[serde(default)]
    pub sensor_type: SensorType,
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
