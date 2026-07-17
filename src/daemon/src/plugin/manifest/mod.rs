// SPDX-License-Identifier: GPL-3.0-or-later
//! Parsing a plugin's manifest into a [`PluginManifest`].

pub mod contract;
pub(crate) mod probes;
pub(crate) mod requirements;
pub(crate) mod udev;

use anyhow::{anyhow, bail, Context, Result};
use halod_shared::types::{
    Animation, CategoryLayout, ChoiceDisplay, ChoiceOption, DeviceType, EffectParamDescriptor,
    EffectParamValue, LcdPresetDescriptor, LcdWidgetDescriptor, LcdWidgetResize, LcdWidgetUpdates,
    ParamKind, Permission, PluginConfigFieldKind, PluginConfigVisibility, PluginKind, RangeDisplay,
    RgbDescriptor, RgbZone, ZoneTopology,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::runtime::transport::{descriptor_for, known_kinds};
use crate::drivers::transports::smbus::{PciMatch, SmbusBusKind};
use crate::drivers::vendors::generic::devices::common::ring_led_positions;
use crate::registry::discovery::DiscoveryHandle;

fn default_report_size() -> usize {
    64
}

fn default_timeout_ms() -> i32 {
    1000
}

/// HID transport parameters a plugin declares. The device path comes from the
/// matched discovery handle, not the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HidConfig {
    #[serde(default = "default_report_size")]
    pub report_size: usize,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: i32,
    #[serde(default)]
    pub feature_report: bool,
    /// Optional second HID collection belonging to the same physical device.
    /// Reports whose IDs are listed here are written through that collection;
    /// reads from both collections are exposed as one logical HID stream.
    #[serde(default)]
    pub companion: Option<HidCompanionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HidCompanionConfig {
    pub usage_page: u16,
    pub usage: u16,
}

impl Default for HidConfig {
    fn default() -> Self {
        Self {
            report_size: default_report_size(),
            timeout_ms: default_timeout_ms(),
            feature_report: false,
            companion: None,
        }
    }
}

fn default_host_key() -> String {
    "host".to_owned()
}

fn default_port_key() -> String {
    "port".to_owned()
}

fn default_tcp_timeout_ms() -> u64 {
    5000
}

/// Names the `config` fields holding host/port rather than literals, so one
/// manifest section is both the GUI-editable settings and the connection source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TcpConfig {
    #[serde(default = "default_host_key")]
    pub host_key: String,
    #[serde(default = "default_port_key")]
    pub port_key: String,
    #[serde(default = "default_tcp_timeout_ms")]
    pub timeout_ms: u64,
    /// Opt-in: allow connecting to loopback/private/link-local addresses. Off by
    /// default so a config-instantiated integration can't be steered into an
    /// SSRF against localhost services or the cloud metadata endpoint. A plugin
    /// that legitimately talks to a LAN device (e.g. WLED) sets this true.
    #[serde(default)]
    pub allow_private: bool,
}

impl Default for TcpConfig {
    fn default() -> Self {
        Self {
            host_key: default_host_key(),
            port_key: default_port_key(),
            timeout_ms: default_tcp_timeout_ms(),
            allow_private: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsbTransferType {
    Bulk,
    Interrupt,
}

fn default_usb_transfer_size() -> usize {
    1024 * 1024
}
fn default_usb_timeout_ms() -> u64 {
    10_000
}

/// One endpoint a plugin may use. The address includes the USB direction bit;
/// undeclared endpoint addresses are never exposed to Lua.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsbEndpointConfig {
    pub address: u8,
    #[serde(rename = "type")]
    pub transfer_type: UsbTransferType,
    #[serde(default = "default_usb_transfer_size")]
    pub max_transfer_size: usize,
    #[serde(default = "default_usb_timeout_ms")]
    pub max_timeout_ms: u64,
}

/// Opt-in policy for endpoint-zero transfers on one named USB device.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsbControlConfig {
    #[serde(default = "default_usb_transfer_size")]
    pub max_transfer_size: usize,
    #[serde(default = "default_usb_timeout_ms")]
    pub max_timeout_ms: u64,
}

/// A named physical USB device. `primary` inherits VID/PID and physical
/// identity from discovery; companion devices declare their own VID/PID.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsbDeviceConfig {
    pub id: String,
    #[serde(default)]
    pub vid: Option<u16>,
    #[serde(default)]
    pub pid: Option<u16>,
    #[serde(default)]
    pub interface: Option<u8>,
    #[serde(default)]
    pub alternate_setting: Option<u8>,
    #[serde(default)]
    pub endpoints: Vec<UsbEndpointConfig>,
    #[serde(default)]
    pub control: Option<UsbControlConfig>,
}

/// General endpoint-oriented USB authority attached to either a USB-primary
/// worker or a composite device whose primary stream remains HID.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsbConfig {
    #[serde(default)]
    pub devices: Vec<UsbDeviceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsbMatch {
    pub vid: u16,
    pub pid: u16,
    #[serde(default)]
    pub interface: u8,
}

/// Executables a command plugin may launch. Names are deliberately bare
/// program names: a path, shell fragment, or argument belongs to runtime data
/// and is rejected before a process is spawned.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandConfig {
    #[serde(default)]
    pub commands: Vec<String>,
}

/// A command or Linux kernel module that cannot be inferred from a transport.
/// Transport requirements are derived by the host.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementDef {
    pub kind: RequirementDefKind,
    pub name: String,
    /// Platforms the requirement applies to; empty means all. A requirement not
    /// applicable to the running host is omitted, never reported as failing.
    #[serde(default)]
    pub platforms: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementDefKind {
    Command,
    KernelModule,
}

/// AMD SMN is deliberately read-only.  Keeping an explicit transport section
/// makes the authority visible in the manifest just like the other privileged
/// transports, while leaving room for future, narrowly-scoped options.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AmdSmnConfig {}

/// LPCIO exposes typed PawnIO operations only; plugins never receive a raw
/// driver handle.  The empty configuration is still significant: it records
/// that the package asks to use this privileged transport.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LpcioConfig {}

/// Linux hwmon is a host-enumerated integration transport. Its empty config is
/// still part of the package's declared authority.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HwmonConfig {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransportsConfig {
    #[serde(default)]
    pub hid: Option<HidConfig>,
    #[serde(default)]
    pub tcp: Option<TcpConfig>,
    #[serde(default)]
    pub usb: Option<UsbConfig>,
    #[serde(default)]
    pub command: Option<CommandConfig>,
    #[serde(default)]
    pub hwmon: Option<HwmonConfig>,
    #[serde(default)]
    pub amd_smn: Option<AmdSmnConfig>,
    #[serde(default)]
    pub lpcio: Option<LpcioConfig>,
}

impl TransportsConfig {
    fn is_empty(&self) -> bool {
        self.hid.is_none()
            && self.tcp.is_none()
            && self.usb.is_none()
            && self.command.is_none()
            && self.hwmon.is_none()
            && self.amd_smn.is_none()
            && self.lpcio.is_none()
    }

    pub fn integration_transport_kind(&self) -> Option<&'static str> {
        match (
            self.tcp.is_some(),
            self.hwmon.is_some(),
            self.command.is_some(),
        ) {
            (true, false, false) => Some("tcp"),
            (false, true, false) => Some("hwmon"),
            (false, false, true) => Some("command"),
            _ => None,
        }
    }
}

/// One choice control the device exposes (e.g. a polling-rate selector).
#[derive(Debug, Clone, Deserialize)]
pub struct ChoiceDef {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub display: ChoiceDisplay,
    pub options: Vec<ChoiceOption>,
    /// Index selected before the user picks one.
    #[serde(default)]
    pub default: usize,
}

/// One user-editable setting a plugin declares (e.g. a server IP/port). Shown by
/// the GUI, persisted per-plugin-id, and readable from Lua via `halod.config`
/// (`sandbox.rs`). Not a capability: it never appears in `capability_labels` and
/// never flips `needs_worker`.
#[derive(Debug, Clone, Deserialize)]
pub struct ConfigFieldDef {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub kind: PluginConfigFieldKind,
    #[serde(default, deserialize_with = "deserialize_config_default")]
    pub default: String,
    #[serde(default)]
    pub category: String,
    /// When true, the value is a secret: encrypted at rest, masked in the GUI,
    /// never sent to the GUI in plaintext, and readable from Lua only when the
    /// plugin was granted `Permission::SecureStorage`.
    #[serde(default)]
    pub secure: bool,
    /// Allowed values for an `Enum` field.
    #[serde(default)]
    pub options: Vec<String>,
    /// Inclusive bounds enforced on a `Number` value at ingress.
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub visible_when: Option<PluginConfigVisibility>,
    #[serde(default)]
    pub help: Option<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
}

fn deserialize_config_default<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    match serde_yaml::Value::deserialize(deserializer)? {
        serde_yaml::Value::Null => Ok(String::new()),
        serde_yaml::Value::Bool(value) => Ok(value.to_string()),
        serde_yaml::Value::Number(value) => Ok(value.to_string()),
        serde_yaml::Value::String(value) => Ok(value),
        _ => Err(D::Error::custom("config defaults must be scalar values")),
    }
}

/// A plugin's declared user-editable settings.
#[derive(Debug, Clone, Deserialize)]
pub struct ConfigManifest {
    pub fields: Vec<ConfigFieldDef>,
}

/// One integer range control (e.g. polling rate in Hz).
#[derive(Debug, Clone, Deserialize)]
pub struct RangeDef {
    pub key: String,
    pub label: String,
    pub min: i32,
    pub max: i32,
    #[serde(default = "default_range_step")]
    pub step: i32,
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
    /// Value shown before the host learns the device's actual value.
    pub default: i32,
}

fn default_range_step() -> i32 {
    1
}

/// One boolean toggle control.
#[derive(Debug, Clone, Deserialize)]
pub struct BooleanDef {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub category: String,
}

/// One fire-and-forget action (button).
#[derive(Debug, Clone, Deserialize)]
pub struct ActionDef {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub category: String,
}

/// Which RGB engine pass a declared effect plugs into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    /// Fills a shared pixmap once per frame; zones sample it (`canvas`).
    Pixmap,
    /// Computes one color per LED directly (`direct`).
    Direct,
}

/// One RGB effect a plugin contributes to the engine's catalog. Registered
/// under a namespaced id (`<plugin_id>:<id>`) so it can never collide with a
/// native effect or another plugin's. The render callback is a sibling
/// function named `render_effect_<id>` (pixmap) or `led_effect_<id>` (direct).
#[derive(Debug, Clone, Deserialize)]
pub struct EffectManifest {
    pub kind: EffectKind,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub params: Vec<EffectParamDescriptor>,
}

impl EffectManifest {
    /// The catalog id an effect is registered/built under.
    pub fn catalog_id(&self, plugin_id: &str) -> String {
        format!("{plugin_id}:{}", self.id)
    }

    pub fn descriptor(&self, plugin_id: &str) -> Animation {
        Animation {
            id: self.catalog_id(plugin_id),
            name: self.name.clone(),
            params: self.params.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WidgetManifest {
    pub id: String,
    pub name: String,
    /// Mandatory SVG used for the GUI catalog tile.
    pub icon: String,
    #[serde(default)]
    pub assets: Vec<String>,
    #[serde(default)]
    pub params: Vec<EffectParamDescriptor>,
    #[serde(default)]
    pub resize: LcdWidgetResize,
    #[serde(default = "default_widget_scale")]
    pub default_scale: f32,
    #[serde(default = "default_widget_min_scale")]
    pub min_scale: f32,
    #[serde(default = "default_widget_aspect")]
    pub default_aspect: f32,
    #[serde(default)]
    pub auto_width_param: Option<String>,
    #[serde(default)]
    pub param_visibility: HashMap<String, halod_shared::types::LcdParamVisibility>,
    #[serde(default)]
    pub uses_color: bool,
    #[serde(default)]
    pub uses_font: bool,
    #[serde(default = "default_true")]
    pub font_controls: bool,
    #[serde(default)]
    pub default_font: Option<String>,
    #[serde(default)]
    pub fixed_text_weight: Option<String>,
    #[serde(default)]
    pub updates: LcdWidgetUpdates,
}

impl WidgetManifest {
    pub fn catalog_id(&self, plugin_id: &str) -> String {
        format!("{plugin_id}:{}", self.id)
    }

    pub fn descriptor(&self, plugin_id: &str) -> LcdWidgetDescriptor {
        LcdWidgetDescriptor {
            id: self.catalog_id(plugin_id),
            plugin_id: plugin_id.to_owned(),
            name: self.name.clone(),
            icon: self.icon.clone(),
            assets: self.assets.clone(),
            params: self.params.clone(),
            resize: self.resize,
            default_scale: self.default_scale,
            min_scale: self.min_scale,
            default_aspect: self.default_aspect,
            auto_width_param: self.auto_width_param.clone(),
            param_visibility: self.param_visibility.clone(),
            uses_color: self.uses_color,
            uses_font: self.uses_font,
            font_controls: self.font_controls,
            default_font: self.default_font.clone(),
            fixed_text_weight: self.fixed_text_weight.clone(),
            updates: self.updates.clone(),
        }
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

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetManifest {
    pub id: String,
    pub name: String,
    pub file: String,
}

impl PresetManifest {
    pub fn descriptor(&self, plugin_id: &str) -> LcdPresetDescriptor {
        LcdPresetDescriptor {
            id: format!("{plugin_id}:{}", self.id),
            plugin_id: plugin_id.to_owned(),
            name: self.name.clone(),
            file: self.file.clone(),
        }
    }
}

impl From<&ConfigFieldDef> for halod_shared::types::PluginConfigField {
    fn from(f: &ConfigFieldDef) -> Self {
        halod_shared::types::PluginConfigField {
            key: f.key.clone(),
            label: f.label.clone(),
            kind: f.kind,
            category: f.category.clone(),
            secure: f.secure,
            options: f.options.clone(),
            min: f.min,
            max: f.max,
            visible_when: f.visible_when.clone(),
            help: f.help.clone(),
            placeholder: f.placeholder.clone(),
        }
    }
}

fn default_topology() -> String {
    "ring".to_owned()
}

/// A recognizable accessory that can attach to a channel. `detect_accessories`
/// returns ids; the host looks them up here to build the child device.
#[derive(Debug, Clone, Deserialize)]
pub struct AccessoryManifest {
    pub id: u8,
    pub name: String,
    pub led_count: u32,
    #[serde(default = "default_topology")]
    pub topology: String,
    /// Ring count for `topology = "rings"`.
    #[serde(default)]
    pub rings: u8,
    /// True when this accessory also exposes a controllable fan.
    #[serde(default)]
    pub fan: bool,
}

/// Map a topology name (+ ring count for "rings") to a [`ZoneTopology`]. Shared
/// by static accessory zones and dynamic `initialize`-reported zones.
pub fn topology_from(topology: &str, rings: u8) -> ZoneTopology {
    match topology {
        "linear" => ZoneTopology::Linear,
        "grid" => ZoneTopology::Grid,
        "rings" => ZoneTopology::Rings {
            count: rings.max(1),
        },
        _ => ZoneTopology::Ring,
    }
}

impl AccessoryManifest {
    pub fn zone_topology(&self) -> ZoneTopology {
        topology_from(&self.topology, self.rings)
    }

    pub fn rgb_descriptor(&self) -> RgbDescriptor {
        let topology = self.zone_topology();
        let leds = ring_led_positions(&topology, self.led_count);
        RgbDescriptor {
            zones: vec![RgbZone {
                id: "ring".to_owned(),
                name: "Ring".to_owned(),
                topology,
                leds,
            }],
            native_effects: vec![],
        }
    }
}

/// How the SMBus scanner probes a declared address before emitting a handle.
/// Openness knob: some controllers NAK a quick-write but answer a read, and a
/// few must not be probed at all (detection is left entirely to `initialize`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeMode {
    /// `write_quick` ACK (the default; what `i2cdetect` uses by default).
    #[default]
    Quick,
    /// `read_byte` succeeds — for devices that misbehave on quick-write.
    ReadByte,
    /// Emit a handle for every declared address unprobed; `initialize` decides.
    None,
}

/// Declarative device match + per-device identity (`None` match fields mean "don't care").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceSpec {
    /// Required (validated non-empty in `validate_manifest`).
    #[serde(default)]
    pub vendor: String,
    /// Required (validated non-empty in `validate_manifest`).
    #[serde(default)]
    pub model: String,
    /// Display-name override; defaults to `model`.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "type")]
    pub device_type: Option<DeviceType>,
    /// Grid placement for the Controls tab's category cards. Empty ⇒ one
    /// full-width row per category, alphabetically.
    #[serde(default)]
    pub control_layout: Vec<CategoryLayout>,

    #[serde(skip)]
    pub transport: String,
    /// Nested transport matcher. It is normalized into the runtime matcher
    /// fields while worker interfaces are being replaced.
    #[serde(default, rename = "match")]
    pub r#match: DeviceMatch,

    // ── HID ──────────────────────────────────────────────────────────────
    #[serde(skip)]
    pub vid: Option<u16>,
    #[serde(skip)]
    pub pid: Option<u16>,
    /// Match any of several product ids (for device families). Takes precedence
    /// over `pid` when non-empty.
    #[serde(skip)]
    pub pids: Vec<u16>,
    #[serde(skip)]
    pub usage_page: Option<u16>,
    #[serde(skip)]
    pub usage: Option<u16>,
    #[serde(skip)]
    pub interface: Option<i32>,
    #[serde(skip)]
    pub generic_hid: bool,

    // ── SMBus (match + scan declaration in one) ──────────────────────────
    /// Bus family to scan/match: "chipset" or "gpu".
    #[serde(skip)]
    pub bus: Option<String>,
    /// Addresses the host may probe on the bus (the security boundary).
    #[serde(skip)]
    pub addresses: Option<Vec<u8>>,
    /// Extra addresses `pre_scan` may write beyond `addresses` (e.g. an ENE
    /// DRAM broadcast address). Never probed or matched — only in `pre_scan`.
    #[serde(skip)]
    pub extra_addresses: Option<Vec<u8>>,
    /// Bus write-rate ceiling applied before any scan traffic.
    #[serde(skip)]
    pub max_bytes_per_sec: Option<u32>,
    /// Run the plugin's `pre_scan` callback on each matching bus before probing.
    #[serde(skip)]
    pub pre_scan: bool,
    #[serde(skip)]
    pub probe: ProbeMode,
    /// PCI-identity gate for GPU buses. Each entry is a `{ vendor, device,
    /// sub_vendor, sub_device, confirmed }` tuple (unset fields are wildcards).
    /// A `bus = "gpu"` spec MUST declare at least one; chipset specs leave it
    /// empty. See [`PciMatch`] and the smbus backend's `validate`.
    #[serde(skip)]
    pub pci_match: Vec<PciMatch>,
}

/// Device matcher. A device declares exactly one transport key; unknown
/// transport matchers are rejected during normalization instead of being
/// ignored by serde and accidentally becoming broad matches.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceMatch {
    #[serde(default)]
    pub hid: Option<HidMatch>,
    #[serde(default)]
    pub usb: Option<UsbMatch>,
    #[serde(default)]
    pub smbus: Option<SmbusMatch>,
    #[serde(default)]
    pub command: Option<CommandMatch>,
    #[serde(default)]
    pub amd_smn: Option<AmdSmnMatch>,
    #[serde(default)]
    pub lpcio: Option<LpcioMatch>,
}

/// A command-backed device is identified by the exact executable that reports
/// it. The executable must also appear in `transports.command.commands`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CommandMatch {
    Name(String),
    Detail { command: String },
}

impl CommandMatch {
    pub(crate) fn command(&self) -> &str {
        match self {
            Self::Name(command) | Self::Detail { command } => command,
        }
    }
}

/// AMD SMN is intentionally a generic family probe. Concrete CPU families are
/// validated from the runtime descriptors returned after a scoped read.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AmdSmnMatch {
    #[serde(default)]
    pub any: bool,
}

/// LPCIO matching requires concrete chip identifiers unless a package
/// deliberately declares a generic catch-all.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LpcioMatch {
    #[serde(default)]
    pub any: bool,
    #[serde(default)]
    pub chip_ids: Vec<u16>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmbusMatch {
    pub bus: String,
    pub addresses: Vec<u8>,
    #[serde(default)]
    pub extra_addresses: Vec<u8>,
    #[serde(default)]
    pub max_bytes_per_sec: Option<u32>,
    #[serde(default)]
    pub pre_scan: bool,
    #[serde(default)]
    pub probe: ProbeMode,
    #[serde(default)]
    pub pci_match: Vec<PciMatch>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HidMatch {
    #[serde(default)]
    pub any: bool,
    #[serde(default)]
    pub vid: Option<u16>,
    #[serde(default)]
    pub pid: Option<u16>,
    #[serde(default)]
    pub pids: Vec<u16>,
    #[serde(default)]
    pub usage_page: Option<u16>,
    #[serde(default)]
    pub usage: Option<u16>,
    #[serde(default)]
    pub interface: Option<i32>,
    /// Per-device HID write ceiling.  This belongs beside the HID identity so
    /// packages can preserve protocol-specific pacing (for example G560's
    /// 1500 B/s vendor-report limit) without granting a global transport rate.
    #[serde(default)]
    pub max_bytes_per_sec: Option<u32>,
}

impl DeviceMatch {
    fn count(&self) -> usize {
        usize::from(self.hid.is_some())
            + usize::from(self.usb.is_some())
            + usize::from(self.smbus.is_some())
            + usize::from(self.command.is_some())
            + usize::from(self.amd_smn.is_some())
            + usize::from(self.lpcio.is_some())
    }
}

impl DeviceSpec {
    /// Delegates to `self.transport`'s registered backend (unknown kind → never).
    pub fn matches(&self, handle: &DiscoveryHandle<'_>) -> bool {
        descriptor_for(&self.transport)
            .and_then(|d| d.matches)
            .is_some_and(|m| m(self, handle))
    }

    /// The SMBus bus family this spec targets, if it is an SMBus spec.
    pub fn bus_kind(&self) -> Option<SmbusBusKind> {
        match self.bus.as_deref() {
            Some("chipset") => Some(SmbusBusKind::Chipset),
            Some("gpu") => Some(SmbusBusKind::Gpu),
            _ => None,
        }
    }

    /// Human-readable device name (`name` override, defaulting to `model`).
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.model)
    }
}

/// One effect's thumbnail, a display-only asset under `assets/` in the plugin directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectAssetRef {
    pub id: String,
    /// Bare filename under `<plugin_dir>/assets/` (no path separators).
    pub thumbnail: String,
}

/// Plugin-level metadata only — vendor/model live on each [`DeviceSpec`] instead.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Identity {
    #[serde(default)]
    pub name: Option<String>,
    /// Optional stable id prefix; defaults to the plugin id.
    #[serde(default)]
    pub id: Option<String>,
    /// Who wrote the plugin (surfaced in the Plugins screen).
    #[serde(default)]
    pub author: Option<String>,
    /// Plugin version string, e.g. "1.2.0".
    #[serde(default)]
    pub version: Option<String>,
    /// SPDX license identifier or free-text license name.
    #[serde(default)]
    pub license: Option<String>,
    /// Free-text description of what the plugin does.
    #[serde(default)]
    pub description: Option<String>,
}

/// A parsed, validated plugin package built from its canonical directory
/// catalog. The inline-Lua helper exists only for isolated runtime tests.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    /// Unique package id, equal to its directory name for external packages.
    pub plugin_id: String,
    pub source_path: PathBuf,
    /// Full entry-script text, re-executed by the worker to build its own VM.
    pub script_source: String,
    /// Package-local Lua modules indexed from `lib/**/*.lua`, keyed by dotted
    /// module name (for example `lib.hidpp.v1`). Sources are read before the VM
    /// starts, so module loading never performs runtime filesystem access.
    pub module_sources: std::collections::BTreeMap<String, String>,
    /// Directory a plugin package was loaded from; empty only for an internal
    /// test fixture.
    pub plugin_dir: PathBuf,
    pub devices: Vec<DeviceSpec>,
    pub identity: Identity,
    /// Display-only logo asset; directory plugins only, empty for a built-in.
    pub logo: Option<String>,
    /// Per-effect thumbnails; directory plugins only, empty for a built-in.
    pub effect_thumbnails: Vec<EffectAssetRef>,
    pub plugin_type: PluginKind,
    /// Opt in to runtime child devices returned by `enumerate_controllers`.
    pub dynamic_children: bool,
    pub effects: Vec<EffectManifest>,
    pub widgets: Vec<WidgetManifest>,
    pub presets: Vec<PresetManifest>,
    pub transports: TransportsConfig,
    /// Explicitly declared host requirements (see [`RequirementDef`]). Auto-
    /// derived requirements are computed on demand, not stored here.
    pub requirements: Vec<RequirementDef>,
    /// Privileged capabilities this plugin needs, gated by user consent.
    pub permissions: Vec<Permission>,
    pub provides: Vec<DataProvideDef>,
    pub consumes: Vec<String>,
    /// Platforms on which this package may execute. An omitted list means all
    /// platforms, allowing catalog visibility without making platform support
    /// an implicit runtime failure.
    pub platforms: Vec<String>,
    /// The complete capability vocabulary this package may return at runtime.
    /// Runtime descriptors remain device-specific; this is the inert catalog
    /// and authority boundary used before Lua is started.
    pub capabilities: Vec<String>,
    pub config: Option<ConfigManifest>,
}

/// Package-only fields from `plugin.yaml`. All declarative device, capability,
/// transport, permission, effect, control, and config fields are deserialized
/// directly into [`PluginManifest`] from the same YAML document. The entry Lua
/// is never evaluated while loading a manifest; it is only read as inert source
/// for hashing and for a later, consent-gated runtime worker.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginMeta {
    /// Required; must equal the plugin's directory name.
    pub id: String,
    #[serde(rename = "type", default)]
    pub plugin_type: PluginKind,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub identity: Identity,
    #[serde(default = "default_entry")]
    pub entry: String,
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub provides: Vec<DataProvideDef>,
    #[serde(default)]
    pub consumes: Vec<String>,
    #[serde(default)]
    pub devices: Vec<DeviceSpec>,
    #[serde(default)]
    pub transports: TransportsConfig,
    #[serde(default)]
    pub requirements: Vec<RequirementDef>,
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub dynamic_children: bool,
    #[serde(default)]
    pub effects: Vec<EffectManifest>,
    #[serde(default)]
    pub widgets: Vec<WidgetManifest>,
    #[serde(default)]
    pub presets: Vec<PresetManifest>,
    #[serde(default)]
    pub config: Option<ConfigManifest>,
    /// Display-only logo, a bare filename under the plugin's `assets/` directory.
    #[serde(default)]
    pub logo: Option<String>,
    /// Per-effect thumbnails, keyed by the YAML-declared effect ids. Kept under
    /// a distinct key because `effects` is the actual effect declaration list.
    #[serde(default)]
    pub effect_assets: Vec<EffectAssetRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DataProvideDef {
    pub key: String,
    pub stale_after_ms: u64,
    pub min_notify_interval_ms: u64,
}

impl<'de> Deserialize<'de> for DataProvideDef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Key(String),
            Policy(Policy),
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Policy {
            key: String,
            #[serde(default = "default_data_stale_after_ms")]
            stale_after_ms: u64,
            #[serde(default = "default_data_notify_interval_ms")]
            min_notify_interval_ms: u64,
        }

        Ok(match Wire::deserialize(deserializer)? {
            Wire::Key(key) => Self {
                key,
                stale_after_ms: default_data_stale_after_ms(),
                min_notify_interval_ms: default_data_notify_interval_ms(),
            },
            Wire::Policy(policy) => Self {
                key: policy.key,
                stale_after_ms: policy.stale_after_ms,
                min_notify_interval_ms: policy.min_notify_interval_ms,
            },
        })
    }
}

const fn default_data_stale_after_ms() -> u64 {
    60_000
}

const fn default_data_notify_interval_ms() -> u64 {
    250
}

fn default_entry() -> String {
    "main.lua".to_owned()
}

/// Conventional logo filename adopted when `plugin.yaml` declares no `logo`.
const DEFAULT_LOGO_NAME: &str = "logo.png";

impl PluginManifest {
    /// The first declared device spec that accepts `handle`, if any.
    pub fn device_for(&self, handle: &DiscoveryHandle<'_>) -> Option<&DeviceSpec> {
        self.devices.iter().find(|s| s.matches(handle))
    }

    /// Device specs that request an SMBus scan.
    pub fn smbus_devices(&self) -> impl Iterator<Item = &DeviceSpec> {
        self.devices.iter().filter(|s| s.bus_kind().is_some())
    }

    /// Stable id prefix a matched device's id is built from.
    pub fn id_prefix(&self) -> &str {
        self.identity.id.as_deref().unwrap_or(&self.plugin_id)
    }

    /// Human-readable device name for a matched spec.
    pub fn display_name_for(&self, spec: &DeviceSpec) -> String {
        spec.display_name().to_owned()
    }

    /// Plugin display name (`identity.name`, falling back to the plugin id).
    pub fn display_name(&self) -> String {
        self.identity
            .name
            .clone()
            .unwrap_or_else(|| self.plugin_id.clone())
    }

    /// Declared plugin author (empty when unset).
    pub fn author(&self) -> &str {
        self.identity.author.as_deref().unwrap_or("")
    }

    /// Declared plugin version (empty when unset).
    pub fn version(&self) -> &str {
        self.identity.version.as_deref().unwrap_or("")
    }

    /// Declared plugin license (empty when unset).
    pub fn license(&self) -> &str {
        self.identity.license.as_deref().unwrap_or("")
    }

    /// Declared plugin description (empty when unset).
    pub fn description(&self) -> &str {
        self.identity.description.as_deref().unwrap_or("")
    }

    /// Display name of every declared device, de-duplicated in declaration order.
    pub fn target_labels(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.devices
            .iter()
            .map(|spec| self.display_name_for(spec))
            .filter(|label| seen.insert(label.clone()))
            .collect()
    }

    /// True when the plugin declares any capability that needs a live transport
    /// + worker. Device-only plugins skip the worker.
    pub fn needs_worker(&self) -> bool {
        // An integration root is not itself capability-bearing, but its
        // manifest still advertises the maximum union its children may expose.
        // It always needs a worker for enumeration and routed callbacks.
        self.plugin_type == PluginKind::Integration || !self.capabilities.is_empty()
    }

    /// Whether this package may execute on the current host. Unsupported
    /// packages remain catalog-visible but discovery must leave them inert.
    pub fn supports_current_platform(&self) -> bool {
        self.platforms.is_empty()
            || self
                .platforms
                .iter()
                .any(|platform| platform == std::env::consts::OS)
    }

    /// Human-readable capability labels for the management UI.
    pub fn capability_labels(&self) -> Vec<String> {
        self.capabilities.clone()
    }

    /// Every user-editable config field this plugin declares (empty if none).
    pub fn config_fields(&self) -> &[ConfigFieldDef] {
        self.config
            .as_ref()
            .map(|c| c.fields.as_slice())
            .unwrap_or(&[])
    }

    /// Keys of declared config fields marked `secure = true`.
    pub fn secure_config_keys(&self) -> Vec<&str> {
        self.config_fields()
            .iter()
            .filter(|f| f.secure)
            .map(|f| f.key.as_str())
            .collect()
    }
}

pub const MAX_PLUGIN_LEDS: u32 = 4_096;
pub const MAX_PLUGIN_LCD_DIM: u32 = 8_192;
pub const MAX_PLUGIN_ZONES: usize = 256;
pub const MAX_PLUGIN_CONTROLLERS: usize = 256;

const MAX_PLUGIN_DEVICES: usize = 256;
const MAX_PLUGIN_EFFECTS: usize = 256;
const MAX_PLUGIN_WIDGETS: usize = 256;
const MAX_WIDGET_ASSETS: usize = 16;
const MAX_PLUGIN_WIDGET_ASSETS: usize = 256;
const MAX_WIDGET_ASSET_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PLUGIN_WIDGET_ASSET_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PLUGIN_PRESETS: usize = 128;
const MAX_EFFECT_PARAMS: usize = 64;
const MAX_CONFIG_FIELDS: usize = 128;
const MAX_USB_ENDPOINTS: usize = 32;
const MAX_CHAIN_ACCESSORIES: usize = 256;
const MAX_CONTROL_DEFS: usize = 256;
const MAX_CONTROL_LAYOUT_ENTRIES: usize = 64;
const MAX_TEXT_BYTES: usize = 256;
const MAX_LONG_TEXT_BYTES: usize = 4_096;

/// Reject a plugin-declared LED count above [`MAX_PLUGIN_LEDS`]. `what` names the
/// offending zone/accessory for the error.
pub fn check_led_count(what: &str, led_count: u32) -> Result<()> {
    if led_count > MAX_PLUGIN_LEDS {
        bail!("'{what}' declares {led_count} LEDs, exceeding the {MAX_PLUGIN_LEDS} limit");
    }
    Ok(())
}

/// Reject a zone count above [`MAX_PLUGIN_ZONES`] — bounds the native LED-position
/// tables the daemon builds per zone outside the plugin VM's memory cap.
pub fn check_zone_count(n: usize) -> Result<()> {
    if n > MAX_PLUGIN_ZONES {
        bail!("declares {n} RGB zones, exceeding the {MAX_PLUGIN_ZONES} limit");
    }
    Ok(())
}

/// Reject plugin-declared LCD dimensions above [`MAX_PLUGIN_LCD_DIM`].
pub fn check_lcd_dims(width: u32, height: u32) -> Result<()> {
    if width > MAX_PLUGIN_LCD_DIM || height > MAX_PLUGIN_LCD_DIM {
        bail!("LCD panel {width}x{height} exceeds the {MAX_PLUGIN_LCD_DIM} per-side limit");
    }
    Ok(())
}

/// Upper bound on the entry Lua source read from disk. Without a cap a
/// symlinked/pathological `entry` (e.g.
/// `/dev/zero`) would drive an unbounded `read_to_string`.
const MAX_ENTRY_BYTES: u64 = 1024 * 1024;
const MAX_MODULE_BYTES: u64 = 1024 * 1024;
const MAX_MODULES: usize = 256;

/// A plugin-package file (`plugin.yaml` / the entry Lua) must be a regular file no
/// larger than `max`. Rejects a symlink (a manually-placed local plugin could point
/// it at `/dev/zero` to hang the read or at a secret to leak its content via parse
/// errors / the content hash — git/imported plugins are already symlink-rejected on
/// copy) and caps the size before the read, all from one no-follow `stat`.
fn check_package_file(path: &Path, max: u64) -> Result<()> {
    let stat =
        std::fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if stat.file_type().is_symlink() {
        bail!("{} must be a regular file, not a symlink", path.display());
    }
    if stat.len() > max {
        bail!(
            "{} is {} bytes, over the {max} byte limit",
            path.display(),
            stat.len()
        );
    }
    Ok(())
}

fn read_package_modules(dir: &Path) -> Result<std::collections::BTreeMap<String, String>> {
    fn visit(
        root: &Path,
        dir: &Path,
        out: &mut std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        let stat =
            std::fs::symlink_metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
        if stat.file_type().is_symlink() || !stat.is_dir() {
            bail!("{} must be a directory, not a symlink", dir.display());
        }
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            let stat = std::fs::symlink_metadata(&path)?;
            if stat.file_type().is_symlink() {
                bail!("{} must not be a symlink", path.display());
            }
            if stat.is_dir() {
                visit(root, &path, out)?;
                continue;
            }
            if path.extension().and_then(|v| v.to_str()) != Some("lua") {
                continue;
            }
            check_package_file(&path, MAX_MODULE_BYTES)?;
            let relative = path
                .strip_prefix(root)
                .expect("visited module stays under root");
            let module_path = relative.with_extension("");
            let mut components = Vec::new();
            for component in module_path.components() {
                let value = component
                    .as_os_str()
                    .to_str()
                    .ok_or_else(|| anyhow!("module path is not valid UTF-8: {}", path.display()))?;
                if value.is_empty()
                    || !value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                {
                    bail!(
                        "invalid Lua module path component '{value}' in {}",
                        path.display()
                    );
                }
                components.push(value);
            }
            let name = components.join(".");
            let source = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            if out.insert(name.clone(), source).is_some() {
                bail!("duplicate Lua module '{name}'");
            }
            if out.len() > MAX_MODULES {
                bail!("plugin contains more than {MAX_MODULES} Lua modules");
            }
        }
        Ok(())
    }

    let root = dir.join("lib");
    let mut modules = std::collections::BTreeMap::new();
    visit(dir, &root, &mut modules)?;
    Ok(modules)
}

/// The `entry` is joined onto the plugin dir and read, so it must be a relative
/// path that stays inside it — reject an absolute path or any `..`/root component
/// that `dir.join` would let escape to arbitrary files (`/etc/shadow`, `../../x`).
fn validate_entry_path(entry: &str) -> Result<()> {
    use std::path::Component;
    let mut saw_normal = false;
    for c in Path::new(entry).components() {
        match c {
            Component::Normal(_) => saw_normal = true,
            Component::CurDir => {}
            _ => {
                bail!("plugin entry '{entry}' must be a relative path inside the plugin directory")
            }
        }
    }
    if !saw_normal {
        bail!("plugin entry '{entry}' is empty");
    }
    Ok(())
}

/// Whether `platforms` names `os` and nothing else — the manifest is pinned to
/// a single host OS. An empty list (meaning "all platforms") does not qualify.
fn declares_only_platform(platforms: &[String], os: &str) -> bool {
    !platforms.is_empty() && platforms.iter().all(|platform| platform == os)
}

/// Cross-field validation, gated by `plugin_type`.
pub(super) fn validate_manifest(manifest: &PluginManifest) -> Result<()> {
    check_count("devices", manifest.devices.len(), MAX_PLUGIN_DEVICES)?;
    check_count("effects", manifest.effects.len(), MAX_PLUGIN_EFFECTS)?;
    check_count("widgets", manifest.widgets.len(), MAX_PLUGIN_WIDGETS)?;
    check_count("presets", manifest.presets.len(), MAX_PLUGIN_PRESETS)?;
    check_count(
        "effect thumbnails",
        manifest.effect_thumbnails.len(),
        MAX_PLUGIN_EFFECTS,
    )?;
    validate_identity(&manifest.identity)?;
    validate_catalog(manifest)?;
    validate_device_identifiers(manifest)?;
    validate_effects(&manifest.effects, "effect")?;
    validate_lcd_content(manifest)?;
    validate_effect_assets(manifest)?;
    validate_transports(manifest)?;
    validate_requirements(manifest)?;
    validate_controls(manifest)?;
    validate_data_contract(manifest)?;
    match manifest.plugin_type {
        PluginKind::Device => {
            if manifest.devices.is_empty() {
                bail!("device plugin declares no devices");
            }
            for spec in &manifest.devices {
                if spec.vendor.is_empty() || spec.model.is_empty() {
                    bail!("every device must declare a non-empty vendor and model");
                }
                validate_control_layout(spec)?;
                match descriptor_for(&spec.transport) {
                    Some(desc) => {
                        if let Some(validate) = desc.validate {
                            validate(spec)?;
                        }
                    }
                    None => bail!(
                        "unsupported device transport '{}' (known: {})",
                        spec.transport,
                        known_kinds().join(", ")
                    ),
                }
                if spec.transport == "smbus" && !manifest.permissions.contains(&Permission::Smbus) {
                    bail!("a device using the smbus transport must declare the `smbus` permission");
                }
                if spec.transport == "hid" && !manifest.permissions.contains(&Permission::Hid) {
                    bail!("a device using the hid transport must declare the `hid` permission");
                }
                if spec.transport == "usb" && !manifest.permissions.contains(&Permission::Usb) {
                    bail!("a device using the usb transport must declare the `usb` permission");
                }
                let required_permission = match spec.transport.as_str() {
                    "command" => Some(Permission::Command),
                    "amd_smn" => Some(Permission::AmdSmn),
                    "lpcio" => Some(Permission::Lpcio),
                    _ => None,
                };
                if let Some(permission) = required_permission {
                    if !manifest.permissions.contains(&permission) {
                        bail!(
                            "a device using the {} transport requires its matching permission",
                            spec.transport
                        );
                    }
                }
            }
        }
        PluginKind::Effect => {
            if manifest.effects.is_empty() {
                bail!("effect plugin declares no effects");
            }
            if !manifest.devices.is_empty() {
                bail!("effect plugin must not declare devices");
            }
            if !manifest.transports.is_empty() {
                bail!("effect plugin must not declare transports");
            }
        }
        PluginKind::Integration => {
            if !manifest.devices.is_empty() {
                bail!("integration plugin must not declare devices");
            }
            if manifest.transports.integration_transport_kind().is_none() {
                bail!(
                    "integration plugin must declare exactly one tcp, hwmon, or command transport"
                );
            }
            if manifest.transports.hid.is_some()
                || manifest.transports.usb.is_some()
                || manifest.transports.amd_smn.is_some()
                || manifest.transports.lpcio.is_some()
            {
                bail!("integration plugin declares a non-integration transport");
            }
        }
        PluginKind::Lcd => {
            if manifest.widgets.is_empty() && manifest.presets.is_empty() {
                bail!("lcd plugin declares no widgets or presets");
            }
            if !manifest.devices.is_empty() {
                bail!("lcd plugin must not declare devices");
            }
            if !manifest.effects.is_empty() {
                bail!("lcd plugin must not declare effects");
            }
            if !manifest.transports.is_empty() {
                bail!("lcd plugin must not declare transports");
            }
            if manifest.dynamic_children {
                bail!("lcd plugin must not declare dynamic children");
            }
        }
    }
    // A tcp transport reaches the network, so the manifest must declare the
    // `network` permission — that's what drives the consent prompt and what the
    // tcp backend gates its connect on. Without this a plugin could ship a tcp
    // integration with an empty permission list and auto-activate silently.
    if manifest.transports.tcp.is_some() && !manifest.permissions.contains(&Permission::Network) {
        bail!("a tcp transport requires the 'network' permission to be declared");
    }
    if manifest.transports.hwmon.is_some() {
        if !manifest.permissions.contains(&Permission::Hwmon) {
            bail!("an hwmon transport requires the 'hwmon' permission to be declared");
        }
        if !declares_only_platform(&manifest.platforms, "linux") {
            bail!("an hwmon integration must declare platforms: [linux]");
        }
    }
    if manifest.transports.amd_smn.is_some() {
        if !manifest.permissions.contains(&Permission::AmdSmn) {
            bail!("an amd_smn transport requires the 'amd_smn' permission to be declared");
        }
        if !declares_only_platform(&manifest.platforms, "windows") {
            bail!("an amd_smn transport must declare platforms: [windows]");
        }
    }
    if manifest.transports.lpcio.is_some() {
        if !manifest.permissions.contains(&Permission::Lpcio) {
            bail!("an lpcio transport requires the 'lpcio' permission to be declared");
        }
        if !declares_only_platform(&manifest.platforms, "windows") {
            bail!("an lpcio transport must declare platforms: [windows]");
        }
    }
    if manifest.transports.usb.is_some() && !manifest.permissions.contains(&Permission::Usb) {
        bail!("a usb transport requires the 'usb' permission to be declared");
    }
    validate_component("plugin id", &manifest.plugin_id)?;
    Ok(())
}

fn validate_data_contract(manifest: &PluginManifest) -> Result<()> {
    anyhow::ensure!(
        manifest.provides.len() <= 32,
        "plugin declares more than 32 data records"
    );
    anyhow::ensure!(
        manifest.consumes.len() <= 64,
        "plugin declares more than 64 data reads"
    );
    let prefix = format!("{}.", manifest.plugin_id);
    let mut keys = HashSet::new();
    for item in &manifest.provides {
        validate_component("provided data key", &item.key)?;
        anyhow::ensure!(
            item.key.starts_with(&prefix),
            "provided data key '{}' is outside plugin namespace '{}.*'",
            item.key,
            manifest.plugin_id
        );
        anyhow::ensure!(
            (1_000..=604_800_000).contains(&item.stale_after_ms),
            "provided data key '{}' stale_after_ms must be between 1000 and 604800000",
            item.key
        );
        anyhow::ensure!(
            (16..=60_000).contains(&item.min_notify_interval_ms),
            "provided data key '{}' min_notify_interval_ms must be between 16 and 60000",
            item.key
        );
        anyhow::ensure!(
            keys.insert(item.key.as_str()),
            "duplicate provided data key '{}'",
            item.key
        );
    }
    let mut reads = HashSet::new();
    for key in &manifest.consumes {
        if key != "host.sensors.*" {
            validate_component("consumed data key", key)?;
            anyhow::ensure!(
                !key.contains('*'),
                "only host.sensors.* may use a wildcard data read"
            );
        }
        anyhow::ensure!(
            reads.insert(key.as_str()),
            "duplicate consumed data key '{key}'"
        );
    }
    Ok(())
}

pub(super) fn validate_accessories(accessories: &[AccessoryManifest]) -> Result<()> {
    check_count(
        "chain accessories",
        accessories.len(),
        MAX_CHAIN_ACCESSORIES,
    )?;
    let mut ids = HashSet::new();
    for acc in accessories {
        validate_short_text("accessory name", &acc.name)?;
        if !ids.insert(acc.id) {
            bail!("chain accessory id '{}' is declared more than once", acc.id);
        }
        check_led_count(&acc.name, acc.led_count)?;
        match acc.topology.as_str() {
            "ring" | "linear" | "grid" if acc.rings == 0 => {}
            "ring" | "linear" | "grid" => bail!(
                "accessory '{}' declares rings for non-rings topology",
                acc.name
            ),
            "rings" if acc.rings > 0 => {}
            "rings" => bail!(
                "accessory '{}' with rings topology must declare rings",
                acc.name
            ),
            _ => bail!(
                "accessory '{}' has unsupported topology '{}'",
                acc.name,
                acc.topology
            ),
        }
    }
    Ok(())
}

fn normalize_device_matches(manifest: &mut PluginManifest) -> Result<()> {
    for device in &mut manifest.devices {
        let count = device.r#match.count();
        if count == 0 {
            continue;
        }
        if count != 1 || !device.transport.is_empty() {
            bail!("a device must declare exactly one nested match and no `transport`");
        }
        if let Some(hid) = &device.r#match.hid {
            if hid.any && (hid.vid.is_some() || hid.pid.is_some() || !hid.pids.is_empty()) {
                bail!("generic hid match cannot also declare identifiers");
            }
            if !hid.any && hid.vid.is_none() {
                bail!("hid match requires a `vid` or explicit `any`");
            }
            device.transport = "hid".to_owned();
            device.vid = hid.vid;
            device.pid = hid.pid;
            device.pids = hid.pids.clone();
            device.usage_page = hid.usage_page;
            device.usage = hid.usage;
            device.interface = hid.interface;
            device.max_bytes_per_sec = hid.max_bytes_per_sec;
            device.generic_hid = hid.any;
        } else if let Some(usb) = &device.r#match.usb {
            if usb.vid == 0 || usb.pid == 0 {
                bail!("usb match requires non-zero vid and pid");
            }
            device.transport = "usb".to_owned();
            device.vid = Some(usb.vid);
            device.pid = Some(usb.pid);
            device.interface = Some(usb.interface.into());
        } else if let Some(smbus) = &device.r#match.smbus {
            device.transport = "smbus".to_owned();
            device.bus = Some(smbus.bus.clone());
            device.addresses = Some(smbus.addresses.clone());
            device.extra_addresses = Some(smbus.extra_addresses.clone());
            device.max_bytes_per_sec = smbus.max_bytes_per_sec;
            device.pre_scan = smbus.pre_scan;
            device.probe = smbus.probe;
            device.pci_match = smbus.pci_match.clone();
        } else if let Some(command) = &device.r#match.command {
            if command.command().is_empty() {
                bail!("command match must name an executable");
            }
            device.transport = "command".to_owned();
        } else if let Some(amd_smn) = &device.r#match.amd_smn {
            if !amd_smn.any {
                bail!("amd_smn match must explicitly declare `any: true`");
            }
            device.transport = "amd_smn".to_owned();
        } else if let Some(lpcio) = &device.r#match.lpcio {
            if lpcio.any != lpcio.chip_ids.is_empty() {
                bail!("lpcio match must declare chip_ids or explicit `any: true`");
            }
            device.transport = "lpcio".to_owned();
        } else {
            bail!("the declared transport match is not implemented by this daemon build");
        }
    }
    Ok(())
}

fn validate_device_identifiers(manifest: &PluginManifest) -> Result<()> {
    let mut hid_identifiers = HashSet::new();
    for device in &manifest.devices {
        if device.transport != "hid" || device.generic_hid {
            continue;
        }
        for pid in device.pids.iter().copied().chain(device.pid) {
            let Some(vid) = device.vid else { continue };
            if !hid_identifiers.insert((
                vid,
                pid,
                device.usage_page,
                device.usage,
                device.interface,
            )) {
                bail!("duplicate concrete HID match {vid:04x}:{pid:04x}");
            }
        }
    }
    Ok(())
}

/// Capability identifiers accepted by the canonical package contract.
pub(in crate::plugin) const SUPPORTED_CAPABILITIES: &[&str] = &[
    "rgb",
    "fan",
    "sensors",
    "battery",
    "connection",
    "dpi",
    "key_remap",
    "keyboard_layout",
    "onboard_profiles",
    "lcd",
    "equalizer",
    "pairing",
    "controls",
    "chain",
];

fn validate_catalog(manifest: &PluginManifest) -> Result<()> {
    let mut platforms = HashSet::new();
    for platform in &manifest.platforms {
        if !matches!(platform.as_str(), "linux" | "windows" | "macos") {
            bail!("unsupported plugin platform '{platform}'");
        }
        if !platforms.insert(platform) {
            bail!("plugin platform '{platform}' is declared more than once");
        }
    }
    let mut capabilities = HashSet::new();
    for capability in &manifest.capabilities {
        if !SUPPORTED_CAPABILITIES.contains(&capability.as_str()) {
            bail!("unknown advertised capability '{capability}'");
        }
        if !capabilities.insert(capability) {
            bail!("advertised capability '{capability}' is declared more than once");
        }
    }
    Ok(())
}

fn check_count(what: &str, count: usize, max: usize) -> Result<()> {
    if count > max {
        bail!("declares {count} {what}, exceeding the {max} limit");
    }
    Ok(())
}

/// A safe requirement component name: a bare module/executable name, never a
/// path, argument, or shell fragment (those belong to runtime data). Mirrors the
/// bare-name rule already enforced on command transport names.
fn validate_requirement_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 {
        bail!("requirement name '{name}' must be 1-128 characters");
    }
    if name == "." || name == ".." {
        bail!("requirement name '{name}' is not a valid component");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("requirement name '{name}' may only contain letters, digits, '.', '_', '-'");
    }
    Ok(())
}

fn validate_requirements(manifest: &PluginManifest) -> Result<()> {
    let mut seen = HashSet::new();
    for req in &manifest.requirements {
        validate_requirement_name(&req.name)?;
        let mut platforms = HashSet::new();
        for platform in &req.platforms {
            if !matches!(platform.as_str(), "linux" | "windows" | "macos") {
                bail!("unsupported requirement platform '{platform}'");
            }
            if !platforms.insert(platform) {
                bail!("requirement platform '{platform}' is declared more than once");
            }
        }
        if req.kind == RequirementDefKind::KernelModule
            && (req.platforms.is_empty() || req.platforms.iter().any(|p| p != "linux"))
        {
            bail!(
                "kernel_module requirement '{}' must declare platforms: [linux]",
                req.name
            );
        }
        // Reject duplicate normalized requirements (same kind + name), which
        // would otherwise produce two contradictory rows for one component.
        let key = (req.kind, req.name.to_ascii_lowercase());
        if !seen.insert(key) {
            bail!(
                "duplicate requirement '{}' declared more than once",
                req.name
            );
        }
    }
    Ok(())
}

pub(super) fn validate_short_text(what: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_TEXT_BYTES || value.contains('\0') {
        bail!("{what} must be non-empty, contain no NUL, and be at most {MAX_TEXT_BYTES} bytes");
    }
    Ok(())
}

fn validate_optional_text(what: &str, value: &Option<String>, max: usize) -> Result<()> {
    if let Some(value) = value {
        if value.len() > max || value.contains('\0') {
            bail!("{what} must contain no NUL and be at most {max} bytes");
        }
    }
    Ok(())
}

fn validate_identity(identity: &Identity) -> Result<()> {
    validate_optional_text("identity name", &identity.name, MAX_TEXT_BYTES)?;
    if let Some(id) = &identity.id {
        validate_component("identity id", id)?;
    }
    validate_optional_text("identity author", &identity.author, MAX_TEXT_BYTES)?;
    validate_optional_text("identity version", &identity.version, MAX_TEXT_BYTES)?;
    validate_optional_text("identity license", &identity.license, MAX_TEXT_BYTES)?;
    validate_optional_text(
        "identity description",
        &identity.description,
        MAX_LONG_TEXT_BYTES,
    )
}

fn validate_effects(effects: &[EffectManifest], what: &str) -> Result<()> {
    let mut ids = HashSet::new();
    for effect in effects {
        validate_component(&format!("{what} id"), &effect.id)?;
        validate_short_text(&format!("{what} '{}' name", effect.id), &effect.name)?;
        if !ids.insert(&effect.id) {
            bail!("{what} id '{}' is declared more than once", effect.id);
        }
        validate_effect_params(&effect.params, &format!("{what} '{}'", effect.id))?;
    }
    Ok(())
}

fn validate_lcd_content(manifest: &PluginManifest) -> Result<()> {
    let mut widget_ids = HashSet::new();
    let mut package_assets = HashSet::new();
    let mut package_asset_bytes = 0u64;
    for widget in &manifest.widgets {
        validate_component("widget id", &widget.id)?;
        validate_short_text("widget name", &widget.name)?;
        if !widget_ids.insert(&widget.id) {
            bail!("widget id '{}' is declared more than once", widget.id);
        }
        check_count(
            &format!("widget '{}' assets", widget.id),
            widget.assets.len(),
            MAX_WIDGET_ASSETS,
        )?;
        let mut widget_assets = HashSet::new();
        let mut widget_asset_bytes = 0u64;
        for name in std::iter::once(&widget.icon).chain(&widget.assets) {
            if !widget_assets.insert(name) {
                bail!(
                    "widget '{}' declares asset '{name}' more than once",
                    widget.id
                );
            }
            let bytes = validate_widget_svg(manifest, widget, name)?;
            widget_asset_bytes = widget_asset_bytes.saturating_add(bytes);
            if widget_asset_bytes > MAX_WIDGET_ASSET_BYTES {
                bail!("widget '{}' assets exceed the total size limit", widget.id);
            }
            if package_assets.insert(name) {
                if package_assets.len() > MAX_PLUGIN_WIDGET_ASSETS {
                    bail!("plugin declares more than {MAX_PLUGIN_WIDGET_ASSETS} widget assets");
                }
                package_asset_bytes = package_asset_bytes.saturating_add(bytes);
                if package_asset_bytes > MAX_PLUGIN_WIDGET_ASSET_BYTES {
                    bail!("plugin widget assets exceed the total size limit");
                }
            }
        }
        validate_effect_params(&widget.params, &format!("widget '{}'", widget.id))?;
        if !widget.min_scale.is_finite() || !(0.1..=0.6).contains(&widget.min_scale) {
            bail!(
                "widget '{}' min_scale must be between 0.1 and 0.6",
                widget.id
            );
        }
        if !widget.default_scale.is_finite()
            || !(widget.min_scale..=3.0).contains(&widget.default_scale)
        {
            bail!(
                "widget '{}' default_scale must be between min_scale and 3",
                widget.id,
            );
        }
        if !widget.default_aspect.is_finite() || !(0.1..=3.0).contains(&widget.default_aspect) {
            bail!(
                "widget '{}' default_aspect must be between 0.1 and 3",
                widget.id
            );
        }
        if widget.default_aspect != 1.0 && widget.resize != LcdWidgetResize::Box {
            bail!(
                "widget '{}' needs resize: box for a non-square default",
                widget.id
            );
        }
        if let Some(default_font) = &widget.default_font {
            if !widget.uses_font {
                bail!(
                    "widget '{}' declares default_font without uses_font",
                    widget.id
                );
            }
            validate_short_text("default font family", default_font)?;
        }
        if !widget.font_controls && !widget.uses_font {
            bail!(
                "widget '{}' disables font_controls without uses_font",
                widget.id
            );
        }
        if let Some(weight) = &widget.fixed_text_weight {
            if !widget.uses_font || !matches!(weight.as_str(), "normal" | "semibold" | "bold") {
                bail!(
                    "widget '{}' fixed_text_weight must be normal, semibold, or bold on a font widget",
                    widget.id
                );
            }
            if widget.font_controls {
                bail!(
                    "widget '{}' fixed_text_weight requires font_controls: false",
                    widget.id
                );
            }
        }
        if let Some(param) = &widget.auto_width_param {
            let valid = widget.params.iter().any(|candidate| {
                candidate.id == *param && matches!(candidate.kind, ParamKind::Text)
            });
            if !valid || widget.resize != LcdWidgetResize::Box {
                bail!(
                    "widget '{}' auto_width_param must name a text parameter on a box widget",
                    widget.id
                );
            }
        }
        for (target, rule) in &widget.param_visibility {
            if !widget.params.iter().any(|param| param.id == *target) {
                bail!(
                    "widget '{}' visibility target '{target}' is not declared",
                    widget.id
                );
            }
            let valid_source = widget.params.iter().any(|param| {
                param.id == rule.param
                    && matches!(&param.kind, ParamKind::Enum { options } if options.contains(&rule.equals))
            });
            if !valid_source {
                bail!(
                    "widget '{}' visibility rule for '{target}' is invalid",
                    widget.id
                );
            }
        }
        for (source, enabled, rule) in [
            (
                "sensors",
                widget.updates.sensors,
                widget.updates.sensors_when.as_ref(),
            ),
            (
                "audio",
                widget.updates.audio,
                widget.updates.audio_when.as_ref(),
            ),
        ] {
            let Some(rule) = rule else { continue };
            let valid_param = widget.params.iter().any(|param| {
                param.id == rule.param
                    && matches!(&param.kind, ParamKind::Enum { options } if options.contains(&rule.equals))
            });
            if !enabled || !valid_param {
                bail!(
                    "widget '{}' {source}_when requires the matching update source and enum value",
                    widget.id
                );
            }
        }
        if let Some(ms) = widget.updates.interval_ms {
            if !(16..=60_000).contains(&ms) {
                bail!(
                    "widget '{}' interval_ms must be between 16 and 60000",
                    widget.id
                );
            }
        }
    }

    let mut preset_ids = HashSet::new();
    for preset in &manifest.presets {
        validate_component("preset id", &preset.id)?;
        validate_short_text("preset name", &preset.name)?;
        if !preset_ids.insert(&preset.id) {
            bail!("preset id '{}' is declared more than once", preset.id);
        }
        validate_asset_name("preset file", &preset.file)?;
        if Path::new(&preset.file).extension().and_then(|v| v.to_str()) != Some("json") {
            bail!("preset '{}' must use a .json file", preset.id);
        }
        if !manifest.plugin_dir.as_os_str().is_empty() {
            let path = manifest.plugin_dir.join("assets").join(&preset.file);
            check_package_file(&path, 512 * 1024)?;
            let bytes = std::fs::read(&path)?;
            let def: halod_shared::lcd_custom::CustomTemplateDef =
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing preset '{}'", preset.id))?;
            halod_shared::lcd_custom::validate_widgets(&def)
                .map_err(|error| anyhow!("preset '{}' is invalid: {error}", preset.id))?;
        }
    }
    Ok(())
}

fn validate_widget_svg(
    manifest: &PluginManifest,
    widget: &WidgetManifest,
    name: &str,
) -> Result<u64> {
    validate_asset_name("widget asset", name)?;
    if Path::new(name).extension().and_then(|value| value.to_str()) != Some("svg") {
        bail!("widget asset '{name}' must be an SVG");
    }
    if manifest.plugin_dir.as_os_str().is_empty() {
        return Ok(0);
    }
    let path = manifest.plugin_dir.join("assets").join(name);
    check_package_file(&path, halod_shared::types::MAX_PLUGIN_ASSET_BYTES)?;
    let bytes = std::fs::read(&path)?;
    resvg::usvg::Tree::from_data(&bytes, &resvg::usvg::Options::default())
        .map_err(|error| anyhow!("widget '{}' asset '{name}' is invalid: {error}", widget.id))?;
    Ok(bytes.len() as u64)
}

fn validate_effect_params(params: &[EffectParamDescriptor], owner: &str) -> Result<()> {
    check_count(
        &format!("parameters for {owner}"),
        params.len(),
        MAX_EFFECT_PARAMS,
    )?;
    let mut ids = HashSet::new();
    for param in params {
        validate_component(&format!("parameter id for {owner}"), &param.id)?;
        validate_short_text(&format!("parameter '{}' label", param.id), &param.label)?;
        if !ids.insert(&param.id) {
            bail!("parameter id '{}' is duplicated in {owner}", param.id);
        }
        match (&param.kind, &param.default) {
            (ParamKind::Range { min, max, step }, EffectParamValue::Float(value)) => {
                validate_numeric_param(&param.id, *min, *max, Some(*step), *value)?;
            }
            (ParamKind::Number { min, max }, EffectParamValue::Float(value)) => {
                validate_numeric_param(&param.id, *min, *max, None, *value)?;
            }
            (ParamKind::Enum { options }, EffectParamValue::Str(value)) => {
                check_count("enum options", options.len(), MAX_CONTROL_DEFS)?;
                if options.is_empty() || !options.iter().any(|option| option == value) {
                    bail!(
                        "enum parameter '{}' default is not one of its options",
                        param.id
                    );
                }
                for option in options {
                    validate_short_text("enum option", option)?;
                }
            }
            (ParamKind::Color, EffectParamValue::Color(_))
            | (ParamKind::Boolean, EffectParamValue::Bool(_)) => {}
            (
                ParamKind::Text | ParamKind::Sensor | ParamKind::Image,
                EffectParamValue::Str(value),
            ) => {
                if value.len() > MAX_LONG_TEXT_BYTES || value.contains('\0') {
                    bail!("parameter '{}' default text is invalid", param.id);
                }
            }
            (ParamKind::Steps, EffectParamValue::Steps(steps)) => {
                check_count("effect parameter steps", steps.len(), MAX_CONTROL_DEFS)?;
                if steps.iter().any(|step| !step.value.is_finite()) {
                    bail!(
                        "steps parameter '{}' contains a non-finite threshold",
                        param.id
                    );
                }
            }
            _ => bail!(
                "parameter '{}' default does not match its declared kind",
                param.id
            ),
        }
    }
    Ok(())
}

fn validate_numeric_param(
    id: &str,
    min: f64,
    max: f64,
    step: Option<f64>,
    default: f64,
) -> Result<()> {
    if !min.is_finite() || !max.is_finite() || !default.is_finite() || min > max {
        bail!("numeric parameter '{id}' has invalid bounds or default");
    }
    if default < min || default > max {
        bail!("numeric parameter '{id}' default is outside its bounds");
    }
    if let Some(step) = step {
        if !step.is_finite() || step <= 0.0 {
            bail!("range parameter '{id}' has an invalid step");
        }
    }
    Ok(())
}

fn validate_effect_assets(manifest: &PluginManifest) -> Result<()> {
    let mut thumbnails = HashSet::new();
    for asset in &manifest.effect_thumbnails {
        validate_component("effect thumbnail id", &asset.id)?;
        if !thumbnails.insert(&asset.id) {
            bail!(
                "effect thumbnail id '{}' is declared more than once",
                asset.id
            );
        }
        validate_asset_name("effect thumbnail", &asset.thumbnail)?;
    }
    if let Some(logo) = &manifest.logo {
        validate_asset_name("plugin logo", logo)?;
    }
    Ok(())
}

fn validate_asset_name(what: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_TEXT_BYTES
        || value.contains('\0')
        || Path::new(value).components().count() != 1
    {
        bail!("{what} must be a non-empty bare filename no longer than {MAX_TEXT_BYTES} bytes");
    }
    Ok(())
}

pub(crate) fn validate_asset_filename(value: &str) -> Result<()> {
    validate_asset_name("plugin asset", value)
}

fn validate_transports(manifest: &PluginManifest) -> Result<()> {
    if let Some(hid) = &manifest.transports.hid {
        if hid.report_size > 1024 || !(1..=60_000).contains(&hid.timeout_ms) {
            bail!("hid transport report_size must be 0 (raw) or 1..=1024 and timeout_ms 1..=60000");
        }
    }
    if let Some(tcp) = &manifest.transports.tcp {
        validate_component("tcp host_key", &tcp.host_key)?;
        validate_component("tcp port_key", &tcp.port_key)?;
        if tcp.host_key == tcp.port_key {
            bail!("tcp host_key and port_key must differ");
        }
        if !(1..=60_000).contains(&tcp.timeout_ms) {
            bail!("tcp timeout_ms must be 1..=60000");
        }
        if manifest.config.is_some() {
            let host = manifest
                .config_fields()
                .iter()
                .find(|field| field.key == tcp.host_key);
            let port = manifest
                .config_fields()
                .iter()
                .find(|field| field.key == tcp.port_key);
            if !host.is_some_and(|field| field.kind == PluginConfigFieldKind::Host)
                || !port.is_some_and(|field| field.kind == PluginConfigFieldKind::Port)
            {
                bail!("tcp host_key and port_key must name host and port config fields");
            }
        }
    }
    if let Some(usb) = &manifest.transports.usb {
        if manifest.plugin_type != PluginKind::Device {
            bail!("usb transport is only valid for a device plugin");
        }
        check_count("usb devices", usb.devices.len(), MAX_USB_ENDPOINTS)?;
        if usb.devices.is_empty() {
            bail!("usb transport must declare a named `primary` device");
        }
        let mut device_ids = HashSet::new();
        for device in &usb.devices {
            validate_component("usb device id", &device.id)?;
            if !device_ids.insert(device.id.as_str()) {
                bail!("usb device id '{}' is declared more than once", device.id);
            }
            if device.id == "primary" {
                if device.vid.is_some() || device.pid.is_some() {
                    bail!("usb device `primary` inherits vid/pid from discovery");
                }
            } else if device.vid.is_none_or(|v| v == 0) || device.pid.is_none_or(|p| p == 0) {
                bail!(
                    "companion usb device '{}' requires non-zero vid and pid",
                    device.id
                );
            }
            if device.endpoints.is_empty() && device.control.is_none() {
                bail!(
                    "usb device '{}' declares no endpoint or control authority",
                    device.id
                );
            }
            let mut addresses = HashSet::new();
            for endpoint in &device.endpoints {
                if !addresses.insert(endpoint.address) {
                    bail!(
                        "usb device '{}' repeats endpoint 0x{:02x}",
                        device.id,
                        endpoint.address
                    );
                }
                if endpoint.address & 0x0f == 0 || endpoint.address & 0x70 != 0 {
                    bail!("usb endpoint address 0x{:02x} is invalid", endpoint.address);
                }
                if endpoint.max_transfer_size == 0 || endpoint.max_transfer_size > 16 * 1024 * 1024
                {
                    bail!(
                        "usb endpoint 0x{:02x} max_transfer_size must be 1..=16777216",
                        endpoint.address
                    );
                }
                if !(1..=60_000).contains(&endpoint.max_timeout_ms) {
                    bail!(
                        "usb endpoint 0x{:02x} max_timeout_ms must be 1..=60000",
                        endpoint.address
                    );
                }
            }
            if let Some(control) = &device.control {
                if control.max_transfer_size == 0 || control.max_transfer_size > 1024 * 1024 {
                    bail!("usb control max_transfer_size must be 1..=1048576");
                }
                if !(1..=60_000).contains(&control.max_timeout_ms) {
                    bail!("usb control max_timeout_ms must be 1..=60000");
                }
            }
        }
        if !device_ids.contains("primary") {
            bail!("usb transport must declare a device named `primary`");
        }
    }
    if let Some(command) = &manifest.transports.command {
        if !manifest.permissions.contains(&Permission::Command) {
            bail!("a command transport requires the 'command' permission");
        }
        if command.commands.is_empty() {
            bail!("command transport must declare at least one executable");
        }
        let mut names = HashSet::new();
        for name in &command.commands {
            validate_component("command executable", name)?;
            if is_disallowed_command(name) {
                bail!(
                    "command executable '{name}' is a shell, interpreter, or command launcher and cannot be granted to a plugin"
                );
            }
            if !names.insert(name.as_str()) {
                bail!("command executable '{name}' is declared more than once");
            }
        }
        for device in &manifest.devices {
            if let Some(command_match) = &device.r#match.command {
                if !names.contains(command_match.command()) {
                    bail!(
                        "command match '{}' is not declared by transports.command",
                        command_match.command()
                    );
                }
            }
        }
    } else if manifest
        .devices
        .iter()
        .any(|device| device.r#match.command.is_some())
    {
        bail!("a command match requires a transports.command declaration");
    }
    Ok(())
}

pub(super) fn is_disallowed_command(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase();
    let normalized = normalized.strip_suffix(".exe").unwrap_or(&normalized);
    matches!(
        normalized,
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "cmd"
            | "powershell"
            | "pwsh"
            | "env"
            | "busybox"
            | "python"
            | "python2"
            | "python3"
            | "perl"
            | "ruby"
            | "node"
            | "nodejs"
            | "deno"
            | "bun"
            | "lua"
            | "luajit"
            | "php"
            | "java"
            | "wscript"
            | "cscript"
            | "mshta"
            | "rundll32"
            | "regsvr32"
    )
}

/// Categories are matched against the controls a device reports at runtime, so
/// an entry naming a category that never shows up is dropped by the GUI rather
/// than rejected here.
fn validate_control_layout(spec: &DeviceSpec) -> Result<()> {
    check_count(
        "control layout entries",
        spec.control_layout.len(),
        MAX_CONTROL_LAYOUT_ENTRIES,
    )?;
    let mut seen = HashSet::new();
    for entry in &spec.control_layout {
        validate_short_text("control layout category", &entry.category)?;
        if entry.span == 0 {
            bail!(
                "control layout category '{}' must span at least one column",
                entry.category
            );
        }
        if !seen.insert(&entry.category) {
            bail!(
                "control layout category '{}' is placed more than once",
                entry.category
            );
        }
    }
    Ok(())
}

fn validate_controls(manifest: &PluginManifest) -> Result<()> {
    if let Some(config) = &manifest.config {
        if config.fields.iter().any(|field| field.secure)
            && !manifest.permissions.contains(&Permission::SecureStorage)
        {
            bail!("secure config fields require the 'secure_storage' permission");
        }
        check_count("config fields", config.fields.len(), MAX_CONFIG_FIELDS)?;
        let mut keys = HashSet::new();
        for field in &config.fields {
            validate_component("config field key", &field.key)?;
            validate_short_text("config field label", &field.label)?;
            if !field.category.is_empty() {
                validate_short_text("config field category", &field.category)?;
            }
            validate_optional_text("config field help", &field.help, MAX_LONG_TEXT_BYTES)?;
            if let Some(placeholder) = &field.placeholder {
                validate_short_text("config field placeholder", placeholder)?;
            }
            if !keys.insert(&field.key) {
                bail!(
                    "config field key '{}' is declared more than once",
                    field.key
                );
            }
            if field.default.len() > MAX_LONG_TEXT_BYTES || field.default.contains('\0') {
                bail!(
                    "config default for '{}' is too long or contains NUL",
                    field.key
                );
            }
            if field.kind == PluginConfigFieldKind::Enum {
                if field.options.is_empty() {
                    bail!("enum config field '{}' has no options", field.key);
                }
                check_count("enum config options", field.options.len(), 128)?;
                let mut options = HashSet::new();
                for option in &field.options {
                    validate_short_text("enum config option", option)?;
                    if !options.insert(option) {
                        bail!(
                            "enum config field '{}' declares option '{option}' more than once",
                            field.key
                        );
                    }
                }
            } else if !field.options.is_empty() {
                bail!(
                    "non-enum config field '{}' must not declare options",
                    field.key
                );
            }
            validate_config_value(field, &field.default)
                .with_context(|| format!("invalid default for config field '{}'", field.key))?;
        }
        for field in &config.fields {
            let Some(rule) = &field.visible_when else {
                continue;
            };
            if rule.field == field.key {
                bail!(
                    "config field '{}' cannot control its own visibility",
                    field.key
                );
            }
            let source = config
                .fields
                .iter()
                .find(|candidate| candidate.key == rule.field)
                .ok_or_else(|| {
                    anyhow!(
                        "config field '{}' visibility source '{}' is not declared",
                        field.key,
                        rule.field
                    )
                })?;
            validate_config_value(source, &rule.equals).with_context(|| {
                format!(
                    "config field '{}' has an invalid visibility value for '{}'",
                    field.key, rule.field
                )
            })?;
        }
    }
    Ok(())
}

/// Validate one persisted or incoming config value according to its manifest field.
pub(super) fn validate_config_value(field: &ConfigFieldDef, value: &str) -> Result<()> {
    anyhow::ensure!(
        value.len() <= MAX_LONG_TEXT_BYTES && !value.contains('\0'),
        "config '{}' exceeds the text bounds",
        field.key
    );
    anyhow::ensure!(
        field.min.is_none_or(f64::is_finite)
            && field.max.is_none_or(f64::is_finite)
            && field.min.zip(field.max).is_none_or(|(min, max)| min <= max),
        "config '{}' has invalid numeric bounds",
        field.key
    );

    let number = match field.kind {
        PluginConfigFieldKind::Text => {
            reject_bounds(field)?;
            None
        }
        PluginConfigFieldKind::Boolean => {
            reject_bounds(field)?;
            anyhow::ensure!(
                matches!(value, "true" | "false"),
                "config '{}' must be true or false",
                field.key
            );
            None
        }
        PluginConfigFieldKind::Enum => {
            reject_bounds(field)?;
            anyhow::ensure!(
                field.options.iter().any(|option| option == value),
                "config '{}' is not a declared option",
                field.key
            );
            None
        }
        PluginConfigFieldKind::Host => {
            reject_bounds(field)?;
            if !value.is_empty() {
                url::Host::parse(value)
                    .map_err(|_| anyhow!("config '{}' must be a valid host", field.key))?;
            }
            None
        }
        PluginConfigFieldKind::Url => {
            reject_bounds(field)?;
            if !value.is_empty() {
                let parsed = url::Url::parse(value)
                    .map_err(|_| anyhow!("config '{}' must be an absolute URL", field.key))?;
                anyhow::ensure!(
                    matches!(parsed.scheme(), "http" | "https") && parsed.host().is_some(),
                    "config '{}' must be an HTTP(S) URL with a host",
                    field.key
                );
            }
            None
        }
        PluginConfigFieldKind::Number => {
            if value.is_empty() {
                None
            } else {
                let parsed: f64 = value
                    .parse()
                    .map_err(|_| anyhow!("config '{}' must be a number", field.key))?;
                anyhow::ensure!(parsed.is_finite(), "config '{}' must be finite", field.key);
                Some(parsed)
            }
        }
        PluginConfigFieldKind::Port => {
            if value.is_empty() {
                None
            } else {
                let parsed: u16 = value
                    .parse()
                    .map_err(|_| anyhow!("config '{}' must be a port", field.key))?;
                anyhow::ensure!(parsed != 0, "config '{}' must be in 1..=65535", field.key);
                Some(f64::from(parsed))
            }
        }
        PluginConfigFieldKind::DurationMs => {
            if value.is_empty() {
                None
            } else {
                let parsed: u64 = value
                    .parse()
                    .map_err(|_| anyhow!("config '{}' must be milliseconds", field.key))?;
                anyhow::ensure!(
                    parsed <= i64::MAX as u64,
                    "config '{}' duration is too large",
                    field.key
                );
                Some(parsed as f64)
            }
        }
    };
    if let Some(number) = number {
        if let Some(min) = field.min {
            anyhow::ensure!(
                number >= min,
                "config '{}' is below the minimum {min}",
                field.key
            );
        }
        if let Some(max) = field.max {
            anyhow::ensure!(
                number <= max,
                "config '{}' is above the maximum {max}",
                field.key
            );
        }
    }
    Ok(())
}

fn reject_bounds(field: &ConfigFieldDef) -> Result<()> {
    anyhow::ensure!(
        field.min.is_none() && field.max.is_none(),
        "config '{}' kind must not declare numeric bounds",
        field.key
    );
    Ok(())
}

/// Charset an id/key must satisfy before it is concatenated into a namespaced
/// identifier — the keyring account `{plugin_id}/{key}` and the effect catalog id
/// `{plugin_id}:{effect_id}` (reparsed by stripping the `{plugin_id}:` prefix). A
/// stray `:`/`/` could otherwise forge another plugin's namespace or make the
/// catalog id ambiguous, so every component is pinned to `[A-Za-z0-9._-]` at the
/// manifest — the point where each value is born.
pub(super) fn validate_component(what: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{what} is empty");
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("{what} '{value}' contains characters outside [A-Za-z0-9._-]");
    }
    Ok(())
}

/// Parse a directory plugin. `plugin.yaml` is the only declarative manifest;
/// `dir/<entry>` is read as inert source for the consent hash and later runtime
/// execution, but is deliberately not compiled or evaluated here.
pub fn parse_manifest_from_dir(dir: &Path) -> Result<PluginManifest> {
    let manifest = build_manifest_from_dir(dir)?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

pub(super) fn build_manifest_from_dir(dir: &Path) -> Result<PluginManifest> {
    let dir_name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("plugin directory has no name: {}", dir.display()))?;

    let meta_path = dir.join("plugin.yaml");
    check_package_file(&meta_path, MAX_ENTRY_BYTES)?;
    let manifest_bytes =
        std::fs::read(&meta_path).with_context(|| format!("reading {}", meta_path.display()))?;
    let meta: PluginMeta = serde_yaml::from_slice(&manifest_bytes)
        .with_context(|| format!("parsing {}", meta_path.display()))?;

    if meta.id.is_empty() {
        bail!("{} declares an empty id", meta_path.display());
    }
    if meta.id != dir_name {
        bail!(
            "plugin.yaml id '{}' does not match its directory name '{}'",
            meta.id,
            dir_name
        );
    }

    validate_entry_path(&meta.entry)?;
    let entry_path = dir.join(&meta.entry);
    check_package_file(&entry_path, MAX_ENTRY_BYTES)?;
    let source = std::fs::read_to_string(&entry_path)
        .with_context(|| format!("reading {}", entry_path.display()))?;

    // An undeclared logo defaults to the conventional `assets/logo.png` if one
    // is present, so a plugin author only needs to drop the file in.
    let logo = meta.logo.or_else(|| {
        dir.join("assets")
            .join(DEFAULT_LOGO_NAME)
            .is_file()
            .then(|| DEFAULT_LOGO_NAME.to_owned())
    });
    let mut manifest = PluginManifest {
        plugin_id: meta.id.clone(),
        source_path: entry_path,
        script_source: source,
        module_sources: read_package_modules(dir)?,
        plugin_dir: dir.to_path_buf(),
        devices: meta.devices,
        identity: Identity {
            name: meta.name.or(meta.identity.name),
            id: meta.identity.id.or(Some(meta.id)),
            author: meta.author.or(meta.identity.author),
            version: meta.version.or(meta.identity.version),
            license: meta.license.or(meta.identity.license),
            description: meta.description.or(meta.identity.description),
        },
        logo,
        effect_thumbnails: meta.effect_assets,
        plugin_type: meta.plugin_type,
        dynamic_children: meta.dynamic_children,
        effects: meta.effects,
        widgets: meta.widgets,
        presets: meta.presets,
        transports: meta.transports,
        requirements: meta.requirements,
        permissions: meta.permissions,
        provides: meta.provides,
        consumes: meta.consumes,
        platforms: meta.platforms,
        capabilities: meta.capabilities,
        config: meta.config,
    };
    normalize_device_matches(&mut manifest)?;

    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_launchers_are_disallowed_case_insensitively() {
        for name in ["sh", "bash", "python3", "env", "perl", "node", "CMD.EXE"] {
            assert!(is_disallowed_command(name), "{name} must be rejected");
        }
        assert!(!is_disallowed_command("nvidia-smi"));
        assert!(!is_disallowed_command("liquidctl"));
    }

    // ── directory plugins (`plugin.yaml` is authoritative) ─────────────

    const ENTRY_LUA: &str = "return {}";

    fn write_plugin_dir(root: &Path, id: &str, yaml_extra: &str, lua: &str) -> PathBuf {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.yaml"), format!("id: {id}\n{yaml_extra}")).unwrap();
        std::fs::write(dir.join("main.lua"), lua).unwrap();
        dir
    }

    fn write_widget_icon(dir: &Path) {
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(
            dir.join("assets/widget.svg"),
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16"/></svg>"#,
        )
        .unwrap();
    }

    fn write_widget_asset(dir: &Path, name: &str, contents: &str) {
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets").join(name), contents).unwrap();
    }

    #[test]
    fn data_contract_enforces_namespace_policy_and_sensor_wildcard() {
        let tmp = tempfile::tempdir().unwrap();
        let valid = write_plugin_dir(
            tmp.path(),
            "telemetry",
            "permissions: [hid]\nprovides: [telemetry.current]\nconsumes: [host.sensors.*]\ndevices:\n  - vendor: Test\n    model: Telemetry\n    match: { hid: { vid: 1, pid: 2 } }\n",
            ENTRY_LUA,
        );
        let manifest = parse_manifest_from_dir(&valid).unwrap();
        assert_eq!(manifest.provides[0].key, "telemetry.current");
        assert_eq!(manifest.provides[0].stale_after_ms, 60_000);
        assert_eq!(manifest.consumes, ["host.sensors.*"]);

        let foreign = write_plugin_dir(
            tmp.path(),
            "diagnostics",
            "permissions: [hid]\nprovides:\n  - { key: telemetry.current, stale_after_ms: 60000, min_notify_interval_ms: 250 }\ndevices:\n  - vendor: Test\n    model: Diagnostics\n    match: { hid: { vid: 1, pid: 3 } }\n",
            ENTRY_LUA,
        );
        assert!(parse_manifest_from_dir(&foreign)
            .unwrap_err()
            .to_string()
            .contains("outside plugin namespace"));
    }

    #[test]
    fn lcd_widget_assets_are_validated_and_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "lcd_assets",
            "type: lcd\nwidgets:\n  - id: weather\n    name: Weather\n    icon: widget.svg\n    assets: [sun.svg, cloud.svg]\n",
            ENTRY_LUA,
        );
        write_widget_icon(&dir);
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><circle cx="8" cy="8" r="6"/></svg>"#;
        write_widget_asset(&dir, "sun.svg", svg);
        write_widget_asset(&dir, "cloud.svg", svg);

        let manifest = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(manifest.widgets[0].assets, ["sun.svg", "cloud.svg"]);
        assert_eq!(
            manifest.widgets[0].descriptor("lcd_assets").assets,
            ["sun.svg", "cloud.svg"]
        );
    }

    #[test]
    fn lcd_widget_assets_reject_traversal_invalid_svg_and_excessive_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let traversal = write_plugin_dir(
            tmp.path(),
            "lcd_asset_traversal",
            "type: lcd\nwidgets:\n  - id: weather\n    name: Weather\n    icon: widget.svg\n    assets: [../secret.svg]\n",
            ENTRY_LUA,
        );
        write_widget_icon(&traversal);
        assert!(
            format!("{:#}", parse_manifest_from_dir(&traversal).unwrap_err())
                .contains("bare filename")
        );

        let invalid = write_plugin_dir(
            tmp.path(),
            "lcd_asset_invalid",
            "type: lcd\nwidgets:\n  - id: weather\n    name: Weather\n    icon: widget.svg\n    assets: [sun.svg]\n",
            ENTRY_LUA,
        );
        write_widget_icon(&invalid);
        write_widget_asset(&invalid, "sun.svg", "not svg");
        assert!(
            format!("{:#}", parse_manifest_from_dir(&invalid).unwrap_err())
                .contains("sun.svg' is invalid")
        );

        let assets = (0..=MAX_WIDGET_ASSETS)
            .map(|index| format!("asset_{index}.svg"))
            .collect::<Vec<_>>()
            .join(", ");
        let excessive = write_plugin_dir(
            tmp.path(),
            "lcd_asset_count",
            &format!(
                "type: lcd\nwidgets:\n  - id: weather\n    name: Weather\n    icon: widget.svg\n    assets: [{assets}]\n"
            ),
            ENTRY_LUA,
        );
        write_widget_icon(&excessive);
        assert!(
            format!("{:#}", parse_manifest_from_dir(&excessive).unwrap_err())
                .contains("exceeding the 16 limit")
        );
    }

    #[test]
    fn lcd_widget_default_font_requires_and_preserves_font_support() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "lcd_system_font",
            "type: lcd\nwidgets:\n  - id: wordmark\n    name: Wordmark\n    icon: widget.svg\n    uses_font: true\n    font_controls: false\n    default_font: Inter Tight\n    fixed_text_weight: bold\n",
            ENTRY_LUA,
        );
        write_widget_icon(&dir);

        let manifest = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(
            manifest.widgets[0].default_font.as_deref(),
            Some("Inter Tight")
        );
        assert!(!manifest.widgets[0].font_controls);
        assert_eq!(
            manifest.widgets[0].fixed_text_weight.as_deref(),
            Some("bold")
        );

        let invalid = write_plugin_dir(
            tmp.path(),
            "lcd_no_font_support",
            "type: lcd\nwidgets:\n  - id: wordmark\n    name: Wordmark\n    icon: widget.svg\n    default_font: Inter Tight\n",
            ENTRY_LUA,
        );
        write_widget_icon(&invalid);
        let error = parse_manifest_from_dir(&invalid).unwrap_err();
        assert!(format!("{error:#}").contains("default_font without uses_font"));
    }

    const LAYOUT_YAML: &str = "permissions: [hid]\ncapabilities: [controls]\ndevices:\n  - vendor: Acme\n    model: K1\n    type: headset\n    match:\n      hid: { vid: 1, pid: 2 }\n    control_layout:\n";

    #[test]
    fn device_control_layout_parses_with_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "layout_ok",
            &format!(
                "{LAYOUT_YAML}      - {{ category: Microphone, order: 0, column: 0 }}\n      - {{ category: Noise Cancelling, order: 1, column: 1, span: 2 }}\n      - {{ category: Audio }}\n"
            ),
            ENTRY_LUA,
        );

        let manifest = parse_manifest_from_dir(&dir).unwrap();
        let layout = &manifest.devices[0].control_layout;
        assert_eq!(
            layout[1],
            CategoryLayout {
                category: "Noise Cancelling".into(),
                order: 1,
                column: 1,
                span: 2,
            }
        );
        assert_eq!(
            layout[2],
            CategoryLayout {
                category: "Audio".into(),
                order: 0,
                column: 0,
                span: 1,
            }
        );
    }

    #[test]
    fn device_control_layout_rejects_duplicate_and_zero_span_categories() {
        let tmp = tempfile::tempdir().unwrap();
        let duplicate = write_plugin_dir(
            tmp.path(),
            "layout_duplicate",
            &format!(
                "{LAYOUT_YAML}      - {{ category: Microphone, column: 0 }}\n      - {{ category: Microphone, column: 1 }}\n"
            ),
            ENTRY_LUA,
        );
        let error = parse_manifest_from_dir(&duplicate).unwrap_err();
        assert!(format!("{error:#}").contains("placed more than once"));

        let zero_span = write_plugin_dir(
            tmp.path(),
            "layout_zero_span",
            &format!("{LAYOUT_YAML}      - {{ category: Microphone, span: 0 }}\n"),
            ENTRY_LUA,
        );
        let error = parse_manifest_from_dir(&zero_span).unwrap_err();
        assert!(format!("{error:#}").contains("span at least one column"));
    }

    #[test]
    fn command_transport_rejects_interpreters() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "unsafe_command",
            "type: integration\npermissions: [command]\ntransports:\n  command:\n    commands: [python3]\n",
            ENTRY_LUA,
        );
        let error = parse_manifest_from_dir(&dir).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("shell, interpreter, or command launcher"),
            "{error:#}"
        );
    }

    #[test]
    fn requirement_rejects_unsafe_name_and_kind() {
        let tmp = tempfile::tempdir().unwrap();
        // A path is runtime data, never a requirement component name.
        let dir = write_plugin_dir(
            tmp.path(),
            "unsafe_req",
            "type: integration\nplatforms: [linux]\npermissions: [hwmon]\ntransports:\n  hwmon: {}\nrequirements:\n  - { kind: kernel_module, name: ../evil, platforms: [linux] }\n",
            ENTRY_LUA,
        );
        assert!(parse_manifest_from_dir(&dir).is_err());

        // An unknown requirement kind is rejected outright.
        let dir = write_plugin_dir(
            tmp.path(),
            "unknown_req",
            "type: integration\ntransports:\n  hwmon: {}\nrequirements:\n  - { kind: firmware, name: x }\n",
            ENTRY_LUA,
        );
        assert!(parse_manifest_from_dir(&dir).is_err());
    }

    #[test]
    fn kernel_module_requirement_must_be_linux_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        // No platforms declared → could be evaluated on Windows where it can
        // never apply; reject it.
        let dir = write_plugin_dir(
            tmp.path(),
            "unscoped_module",
            "type: integration\ntransports:\n  hwmon: {}\nrequirements:\n  - { kind: kernel_module, name: nct6775 }\n",
            ENTRY_LUA,
        );
        let err = parse_manifest_from_dir(&dir).unwrap_err();
        assert!(err.to_string().contains("platforms: [linux]"), "{err:#}");
    }

    #[test]
    fn duplicate_requirements_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "dup_req",
            "type: integration\npermissions: [command]\ntransports:\n  command:\n    commands: [pactl]\nrequirements:\n  - { kind: command, name: pactl }\n  - { kind: command, name: PACTL }\n",
            ENTRY_LUA,
        );
        let err = parse_manifest_from_dir(&dir).unwrap_err();
        assert!(err.to_string().contains("duplicate requirement"), "{err:#}");
    }

    #[test]
    fn command_requirement_can_be_a_presence_only_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "presence_req",
            "type: integration\npermissions: [network]\ntransports:\n  tcp: {}\nrequirements:\n  - { kind: command, name: openrgb }
",
            ENTRY_LUA,
        );
        let manifest = parse_manifest_from_dir(&dir).unwrap();
        assert!(manifest.transports.command.is_none());
        assert!(!manifest.permissions.contains(&Permission::Command));
        assert_eq!(manifest.requirements[0].name, "openrgb");
    }

    #[test]
    fn host_inferred_requirement_kinds_are_not_manifest_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let bad_broker = write_plugin_dir(
            tmp.path(),
            "bad_broker",
            "requirements:\n  - { kind: broker_capability, name: pawnio.lpcio, platforms: [linux] }\n",
            ENTRY_LUA,
        );
        assert!(parse_manifest_from_dir(&bad_broker).is_err());

        let bad_hwmon = write_plugin_dir(
            tmp.path(),
            "bad_hwmon",
            "requirements:\n  - { kind: linux_hwmon, name: other, platforms: [linux] }\n",
            ENTRY_LUA,
        );
        assert!(parse_manifest_from_dir(&bad_hwmon).is_err());
    }

    #[test]
    fn directory_plugin_parses_required_fields_and_default_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "dirplug",
            "permissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: Acme\n    model: K1\n    type: led_strip\n    match:\n      hid: { vid: 1, pid: 2 }\n",
            ENTRY_LUA,
        );
        let m = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(m.plugin_id, "dirplug");
        assert_eq!(m.devices.len(), 1);
        assert_eq!(m.devices[0].vendor, "Acme");
        assert_eq!(m.capabilities, vec!["rgb"]);
    }

    #[test]
    fn rich_config_fields_parse_with_metadata_and_typed_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let common = "permissions: [hid, secure_storage]\ncapabilities: [rgb]\ndevices:\n  - vendor: Acme\n    model: Device\n    match:\n      hid: { vid: 1, pid: 2 }\nconfig:\n  fields:\n    - { key: enabled, label: Enabled, kind: boolean, default: true }\n    - { key: mode, label: Mode, kind: enum, options: [auto, manual], default: auto }\n    - { key: host, label: Host, kind: host, default: localhost }\n    - { key: port, label: Port, kind: port, default: 6742 }\n    - { key: endpoint, label: Endpoint, kind: url, default: 'https://example.com/api' }\n    - { key: timeout, label: Timeout, kind: duration_ms, default: 2500, min: 100, max: 10000 }\n    - key: detail\n      label: Detail\n      placeholder: Optional value\n      help: Used only in manual mode.\n      visible_when: { field: mode, equals: manual }\n    - { key: token, label: Token, secure: true }\n";
        let dir = write_plugin_dir(tmp.path(), "rich_config", common, ENTRY_LUA);

        let manifest = parse_manifest_from_dir(&dir).unwrap();
        let fields = manifest.config_fields();
        assert_eq!(fields[0].default, "true");
        assert_eq!(fields[3].default, "6742");
        assert_eq!(fields[1].options, ["auto", "manual"]);
        assert_eq!(fields[6].placeholder.as_deref(), Some("Optional value"));
        assert_eq!(fields[6].help.as_deref(), Some("Used only in manual mode."));
        assert_eq!(
            fields[6].visible_when,
            Some(PluginConfigVisibility {
                field: "mode".into(),
                equals: "manual".into(),
            })
        );
    }

    #[test]
    fn secure_config_requires_secure_storage_permission() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "insecure_secret",
            "permissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: Acme\n    model: Device\n    match:\n      hid: { vid: 1, pid: 2 }\nconfig:\n  fields:\n    - { key: token, label: Token, secure: true }\n",
            ENTRY_LUA,
        );

        let error = parse_manifest_from_dir(&dir).unwrap_err();
        assert!(format!("{error:#}").contains("secure_storage"));
    }

    #[test]
    fn directory_plugin_indexes_only_its_package_local_lib_modules() {
        let tmp = tempfile::tempdir().unwrap();
        let integration = "type: integration\nplatforms: [linux]\npermissions: [hwmon]\ntransports:\n  hwmon: {}\n";
        let dir = write_plugin_dir(tmp.path(), "modules", integration, "return {}");
        std::fs::create_dir_all(dir.join("lib").join("hidpp")).unwrap();
        std::fs::write(
            dir.join("lib").join("hidpp").join("v1.lua"),
            "return { version = 1 }",
        )
        .unwrap();
        let sibling = write_plugin_dir(tmp.path(), "sibling", integration, "return {}");
        std::fs::create_dir_all(sibling.join("lib")).unwrap();
        std::fs::write(sibling.join("lib").join("secret.lua"), "return 42").unwrap();

        let manifest = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(
            manifest.module_sources.keys().collect::<Vec<_>>(),
            vec!["lib.hidpp.v1"]
        );
        assert!(!manifest.module_sources.contains_key("lib.secret"));
    }

    #[test]
    fn package_rejects_repository_only_compatibility_field() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("missing_compatibility");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: missing_compatibility\ncompatibility:\n  halod: '>=0.2.0'\n  plugin_api: 1\ntype: integration\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "return { type = 'integration' }").unwrap();

        let error = parse_manifest_from_dir(&dir).unwrap_err();
        assert!(
            format!("{error:#}").contains("unknown field `compatibility`"),
            "{error:#}"
        );
    }

    #[test]
    fn directory_plugin_explicit_entry_and_license_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dirplug2");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: dirplug2\nentry: driver.lua\nlicense: GPL-3.0-or-later\npermissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: Acme\n    model: K1\n    type: led_strip\n    match:\n      hid: { vid: 1, pid: 2 }\n",
        )
        .unwrap();
        std::fs::write(dir.join("driver.lua"), ENTRY_LUA).unwrap();

        let m = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(m.license(), "GPL-3.0-or-later");
        assert_eq!(m.source_path, dir.join("driver.lua"));
    }

    #[test]
    fn directory_plugin_ignores_all_lua_declarations() {
        let tmp = tempfile::tempdir().unwrap();
        // Even valid-looking declarative fields in the entry Lua are inert at
        // manifest load time; only plugin.yaml contributes declarations.
        let lua = r#"return {
            type = "integration",
            devices = { { transport = "hid", vid = 9, pid = 9, vendor = "Lua", model = "Ignored" } },
            identity = { name = "Ignored Name" },
            rgb = { zones = {} },
        }"#;
        let dir = write_plugin_dir(
            tmp.path(),
            "overlaid",
            "type: device\nname: Real Name\npermissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: Real\n    model: K1\n    type: led_strip\n    match:\n      hid: { vid: 1, pid: 2 }\n",
            lua,
        );
        let m = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(m.plugin_type, PluginKind::Device);
        assert_eq!(m.devices.len(), 1);
        assert_eq!(m.devices[0].vendor, "Real");
        assert_eq!(m.identity.name.as_deref(), Some("Real Name"));
    }

    #[test]
    fn directory_manifest_never_compiles_or_executes_entry_lua() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "inert_entry",
            "type: integration\nplatforms: [linux]\npermissions: [hwmon]\ntransports:\n  hwmon: {}\n",
            "this is deliberately not valid Lua",
        );
        let manifest = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(manifest.plugin_type, PluginKind::Integration);
        assert_eq!(manifest.script_source, "this is deliberately not valid Lua");
    }

    #[test]
    fn linux_hwmon_integration_is_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "hwmon",
            "type: integration\nplatforms: [linux]\npermissions: [hwmon]\ncapabilities: [sensors, fan]\ntransports:\n  hwmon: {}\n",
            ENTRY_LUA,
        );
        let manifest = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(
            manifest.transports.integration_transport_kind(),
            Some("hwmon")
        );
    }

    #[test]
    fn hwmon_integration_requires_permission_and_linux_platform() {
        let tmp = tempfile::tempdir().unwrap();
        let missing_permission = write_plugin_dir(
            tmp.path(),
            "missing_permission",
            "type: integration\nplatforms: [linux]\ntransports:\n  hwmon: {}\n",
            ENTRY_LUA,
        );
        assert!(parse_manifest_from_dir(&missing_permission)
            .unwrap_err()
            .to_string()
            .contains("hwmon"));

        let wrong_platform = write_plugin_dir(
            tmp.path(),
            "wrong_platform",
            "type: integration\nplatforms: [windows]\npermissions: [hwmon]\ntransports:\n  hwmon: {}\n",
            ENTRY_LUA,
        );
        assert!(parse_manifest_from_dir(&wrong_platform)
            .unwrap_err()
            .to_string()
            .contains("platforms: [linux]"));
    }

    #[test]
    fn amd_smn_transport_must_declare_windows_platform() {
        let tmp = tempfile::tempdir().unwrap();
        let body = |platforms: &str| {
            format!(
                "{platforms}permissions: [amd_smn]\ncapabilities: [sensors]\ndevices:\n  - vendor: AMD\n    model: AMD Zen CPU\n    type: sensor\n    match:\n      amd_smn: {{ any: true }}\ntransports:\n  amd_smn: {{}}\n"
            )
        };

        let windows = write_plugin_dir(
            tmp.path(),
            "smn_win",
            &body("platforms: [windows]\n"),
            ENTRY_LUA,
        );
        assert!(parse_manifest_from_dir(&windows).is_ok());

        for (name, platforms) in [("smn_none", ""), ("smn_linux", "platforms: [linux]\n")] {
            let dir = write_plugin_dir(tmp.path(), name, &body(platforms), ENTRY_LUA);
            assert!(parse_manifest_from_dir(&dir)
                .unwrap_err()
                .to_string()
                .contains("platforms: [windows]"));
        }
    }

    #[test]
    fn integration_rejects_multiple_root_transports() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "multiple_roots",
            "type: integration\nplatforms: [linux]\npermissions: [hwmon, network]\ntransports:\n  hwmon: {}\n  tcp:\n    host_key: host\n    port_key: port\n",
            ENTRY_LUA,
        );
        assert!(parse_manifest_from_dir(&dir)
            .unwrap_err()
            .to_string()
            .contains("exactly one"));
    }

    #[test]
    fn directory_name_mismatch_with_yaml_id_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "actual-dir-name",
            "permissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: A\n    model: B\n    type: led_strip\n    match:\n      hid: { vid: 1, pid: 2 }\n",
            ENTRY_LUA,
        );
        // Rewrite plugin.yaml claiming a different id than the directory name.
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: someone-else\npermissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: A\n    model: B\n    type: led_strip\n    match:\n      hid: { vid: 1, pid: 2 }\n",
        )
        .unwrap();
        assert!(parse_manifest_from_dir(&dir).is_err());
    }

    #[test]
    fn directory_plugin_missing_yaml_or_entry_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let no_yaml = tmp.path().join("no_yaml");
        std::fs::create_dir_all(&no_yaml).unwrap();
        std::fs::write(no_yaml.join("main.lua"), ENTRY_LUA).unwrap();
        assert!(parse_manifest_from_dir(&no_yaml).is_err());

        let no_entry = write_plugin_dir(
            tmp.path(),
            "no_entry",
            "permissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: A\n    model: B\n    type: led_strip\n    match:\n      hid: { vid: 1, pid: 2 }\n",
            ENTRY_LUA,
        );
        std::fs::remove_file(no_entry.join("main.lua")).unwrap();
        assert!(parse_manifest_from_dir(&no_entry).is_err());
    }

    #[test]
    fn plugin_meta_devices_round_trip_through_yaml() {
        let yaml = "id: rt\ndevices:\n  - vendor: Acme\n    model: K1\n    type: led_strip\n    match:\n      hid: { vid: 1 }\n  - vendor: Acme\n    model: K2\n    type: led_strip\n    match:\n      hid: { vid: 2 }\n";
        let parsed: PluginMeta = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.devices.len(), 2);
        assert_eq!(parsed.devices[0].vendor, "Acme");
        assert_eq!(parsed.devices[0].model, "K1");
        assert_eq!(parsed.devices[1].model, "K2");
    }

    #[test]
    fn directory_plugin_without_assets_leaves_logo_and_thumbnails_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "noassets",
            "permissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: A\n    model: B\n    type: led_strip\n    match:\n      hid: { vid: 1, pid: 2 }\n",
            ENTRY_LUA,
        );
        let m = parse_manifest_from_dir(&dir).unwrap();
        assert!(m.logo.is_none());
        assert!(m.effect_thumbnails.is_empty());
    }

    #[test]
    fn directory_plugin_surfaces_logo_and_effect_thumbnails() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "withassets",
            "permissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: A\n    model: B\n    type: led_strip\n    match:\n      hid: { vid: 1, pid: 2 }\n\
             logo: logo.png\n\
             effects:\n  - kind: pixmap\n    id: rainbow\n    name: Rainbow\n\
             effect_assets:\n  - id: rainbow\n    thumbnail: rainbow.png\n",
            ENTRY_LUA,
        );
        let m = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(m.logo.as_deref(), Some("logo.png"));
        assert_eq!(m.effect_thumbnails.len(), 1);
        assert_eq!(m.effect_thumbnails[0].id, "rainbow");
        assert_eq!(m.effect_thumbnails[0].thumbnail, "rainbow.png");
    }

    #[test]
    fn undeclared_logo_defaults_to_conventional_asset_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "conv",
            "permissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: A\n    model: B\n    type: led_strip\n    match:\n      hid: { vid: 1, pid: 2 }\n",
            ENTRY_LUA,
        );
        // No `logo:` in the manifest, but the file is present.
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets").join("logo.png"), b"x").unwrap();
        assert_eq!(
            parse_manifest_from_dir(&dir).unwrap().logo.as_deref(),
            Some("logo.png")
        );
    }

    #[test]
    fn directory_plugin_uses_nested_hid_match() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested_match");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: nested_match\nname: Nested Match\npermissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: Acme\n    model: K1\n    type: mouse\n    match:\n      hid:\n        vid: 0x1234\n        pid: 0x5678\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), ENTRY_LUA).unwrap();

        let manifest = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(manifest.devices[0].transport, "hid");
        assert_eq!(manifest.devices[0].vid, Some(0x1234));
        assert_eq!(manifest.devices[0].pid, Some(0x5678));
    }

    #[test]
    fn directory_plugin_uses_nested_smbus_match() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested_smbus");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: nested_smbus\npermissions: [smbus]\ncapabilities: [rgb]\ndevices:\n  - vendor: Acme\n    model: DRAM\n    type: ram\n    match:\n      smbus:\n        bus: chipset\n        addresses: [0x50]\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), ENTRY_LUA).unwrap();

        let manifest = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(manifest.devices[0].transport, "smbus");
        assert_eq!(manifest.devices[0].bus.as_deref(), Some("chipset"));
        assert_eq!(manifest.devices[0].addresses.as_deref(), Some(&[0x50][..]));
    }

    #[test]
    fn directory_plugin_uses_nested_usb_match() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested_usb");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: nested_usb\npermissions: [usb]\ncapabilities: [controls]\ndevices:\n  - vendor: Acme\n    model: Panel\n    type: monitor\n    match:\n      usb: { vid: 0x1234, pid: 0x5678, interface: 0 }\ntransports:\n  usb:\n    devices:\n      - id: primary\n        interface: 0\n        control: { max_transfer_size: 64, max_timeout_ms: 1000 }\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), ENTRY_LUA).unwrap();
        let manifest = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(manifest.devices[0].transport, "usb");
        assert_eq!(manifest.devices[0].vid, Some(0x1234));
    }

    #[test]
    fn usb_manifest_requires_permission_and_primary_device() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "usb_no_permission",
            "capabilities: [controls]\ndevices:\n  - vendor: A\n    model: B\n    match: { usb: { vid: 1, pid: 2, interface: 0 } }\ntransports:\n  usb:\n    devices:\n      - id: primary\n        control: { max_transfer_size: 64, max_timeout_ms: 1000 }\n",
            ENTRY_LUA,
        );
        let error = parse_manifest_from_dir(&dir).unwrap_err();
        assert!(format!("{error:#}").contains("usb` permission"));
    }

    #[test]
    fn usb_manifest_rejects_undeclared_or_duplicate_endpoint_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "usb_bad_endpoints",
            "permissions: [usb]\ncapabilities: [controls]\ndevices:\n  - vendor: A\n    model: B\n    match: { usb: { vid: 1, pid: 2, interface: 0 } }\ntransports:\n  usb:\n    devices:\n      - id: primary\n        endpoints:\n          - { address: 0x02, type: bulk, max_transfer_size: 64, max_timeout_ms: 1000 }\n          - { address: 0x02, type: interrupt, max_transfer_size: 64, max_timeout_ms: 1000 }\n",
            ENTRY_LUA,
        );
        let error = parse_manifest_from_dir(&dir).unwrap_err();
        assert!(format!("{error:#}").contains("repeats endpoint 0x02"));
    }

    #[test]
    fn removed_usb_control_manifest_field_has_no_compatibility_shim() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "old_usb",
            "capabilities: [controls]\ndevices:\n  - vendor: A\n    model: B\n    match: { usb_control: { vid: 1, pid: 2 } }\n",
            ENTRY_LUA,
        );
        let error = parse_manifest_from_dir(&dir).unwrap_err();
        assert!(format!("{error:#}").contains("unknown field `usb_control`"));
    }
}
