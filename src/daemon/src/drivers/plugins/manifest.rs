// SPDX-License-Identifier: GPL-3.0-or-later
//! Parsing a plugin's manifest into a [`PluginManifest`].

use anyhow::{anyhow, bail, Context, Result};
use halod_shared::types::{
    Animation, ChoiceDisplay, ChoiceOption, DeviceType, EffectParamDescriptor, EffectParamValue,
    ParamKind, Permission, PluginConfigFieldKind, PluginKind, RangeDisplay, RgbDescriptor, RgbZone,
    ZoneTopology,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::transport::{descriptor_for, known_kinds};
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

/// A secondary USB vendor-control device a plugin bundles alongside its matched
/// device, so several physical chips present as one merged device. Opened by
/// VID/PID and reached from Lua by `id` (the matched device is the unnamed
/// primary endpoint).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsbControlEndpoint {
    pub id: String,
    pub vid: u16,
    pub pid: u16,
    #[serde(default)]
    pub interface: u8,
}

/// USB vendor-control transport parameters. `interface` claims the matched
/// device's interface; `endpoints` lists any extra control devices the plugin
/// drives (the DDC controller + Ambiglow LED controller of one monitor, say).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsbControlConfig {
    #[serde(default)]
    pub interface: u8,
    #[serde(default)]
    pub endpoints: Vec<UsbControlEndpoint>,
}

/// Executables a command plugin may launch. Names are deliberately bare
/// program names: a path, shell fragment, or argument belongs to runtime data
/// and is rejected before a process is spawned.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandConfig {
    #[serde(default)]
    pub commands: Vec<String>,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransportsConfig {
    #[serde(default)]
    pub hid: Option<HidConfig>,
    #[serde(default)]
    pub tcp: Option<TcpConfig>,
    #[serde(default)]
    pub usb_control: Option<UsbControlConfig>,
    #[serde(default)]
    pub command: Option<CommandConfig>,
    #[serde(default)]
    pub amd_smn: Option<AmdSmnConfig>,
    #[serde(default)]
    pub lpcio: Option<LpcioConfig>,
}

impl TransportsConfig {
    fn is_empty(&self) -> bool {
        self.hid.is_none()
            && self.tcp.is_none()
            && self.usb_control.is_none()
            && self.command.is_none()
            && self.amd_smn.is_none()
            && self.lpcio.is_none()
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
    #[serde(default)]
    pub default: String,
    #[serde(default)]
    pub category: String,
    /// When true, the value is a secret: encrypted at rest, masked in the GUI,
    /// never sent to the GUI in plaintext, and readable from Lua only when the
    /// plugin was granted `Permission::SecureStorage`.
    #[serde(default)]
    pub secure: bool,
    /// Inclusive bounds enforced on a `Number` value at ingress.
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
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
/// function named `render_<id>` (pixmap) or `led_colors_<id>` (direct), or
/// bare `render`/`led_colors` when the plugin declares exactly one effect.
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

impl From<&ConfigFieldDef> for halod_shared::types::PluginConfigField {
    fn from(f: &ConfigFieldDef) -> Self {
        halod_shared::types::PluginConfigField {
            key: f.key.clone(),
            label: f.label.clone(),
            kind: f.kind,
            category: f.category.clone(),
            secure: f.secure,
            min: f.min,
            max: f.max,
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
    pub usb_control: Option<UsbControlMatch>,
    #[serde(default)]
    pub smbus: Option<SmbusMatch>,
    #[serde(default)]
    pub hwmon: Option<HwmonMatch>,
    #[serde(default)]
    pub command: Option<CommandMatch>,
    #[serde(default)]
    pub amd_smn: Option<AmdSmnMatch>,
    #[serde(default)]
    pub lpcio: Option<LpcioMatch>,
}

/// The deliberately broad hwmon discovery declaration. Generic matching is an
/// explicit opt-in so a missing identifier cannot turn into a catch-all.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HwmonMatch {
    #[serde(default)]
    pub any: bool,
}

/// USB vendor-control hardware identity. The transport configuration declares
/// endpoint behavior; this match only selects the primary physical device.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsbControlMatch {
    pub vid: u16,
    pub pid: u16,
    #[serde(default)]
    pub interface: u8,
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
            + usize::from(self.usb_control.is_some())
            + usize::from(self.smbus.is_some())
            + usize::from(self.hwmon.is_some())
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
    pub transports: TransportsConfig,
    /// Privileged capabilities this plugin needs, gated by user consent.
    pub permissions: Vec<Permission>,
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
    pub devices: Vec<DeviceSpec>,
    #[serde(default)]
    pub transports: TransportsConfig,
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub dynamic_children: bool,
    #[serde(default)]
    pub effects: Vec<EffectManifest>,
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
const MAX_EFFECT_PARAMS: usize = 64;
const MAX_CONFIG_FIELDS: usize = 128;
const MAX_USB_ENDPOINTS: usize = 32;
const MAX_CHAIN_ACCESSORIES: usize = 256;
const MAX_CONTROL_DEFS: usize = 256;
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

/// Cross-field validation, gated by `plugin_type`.
pub(super) fn validate_manifest(manifest: &PluginManifest) -> Result<()> {
    check_count("devices", manifest.devices.len(), MAX_PLUGIN_DEVICES)?;
    check_count("effects", manifest.effects.len(), MAX_PLUGIN_EFFECTS)?;
    check_count(
        "effect thumbnails",
        manifest.effect_thumbnails.len(),
        MAX_PLUGIN_EFFECTS,
    )?;
    validate_identity(&manifest.identity)?;
    validate_catalog(manifest)?;
    validate_device_identifiers(manifest)?;
    validate_effects(&manifest.effects, "effect")?;
    validate_effect_assets(manifest)?;
    validate_transports(manifest)?;
    validate_controls(manifest)?;
    match manifest.plugin_type {
        PluginKind::Device => {
            if manifest.devices.is_empty() {
                bail!("device plugin declares no devices");
            }
            for spec in &manifest.devices {
                if spec.vendor.is_empty() || spec.model.is_empty() {
                    bail!("every device must declare a non-empty vendor and model");
                }
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
                let required_permission = match spec.transport.as_str() {
                    "hwmon" => Some(Permission::Hwmon),
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
        }
    }
    // A tcp transport reaches the network, so the manifest must declare the
    // `network` permission — that's what drives the consent prompt and what the
    // tcp backend gates its connect on. Without this a plugin could ship a tcp
    // integration with an empty permission list and auto-activate silently.
    if manifest.transports.tcp.is_some() && !manifest.permissions.contains(&Permission::Network) {
        bail!("a tcp transport requires the 'network' permission to be declared");
    }
    if manifest.transports.amd_smn.is_some() && !manifest.permissions.contains(&Permission::AmdSmn)
    {
        bail!("an amd_smn transport requires the 'amd_smn' permission to be declared");
    }
    if manifest.transports.lpcio.is_some() && !manifest.permissions.contains(&Permission::Lpcio) {
        bail!("an lpcio transport requires the 'lpcio' permission to be declared");
    }
    validate_component("plugin id", &manifest.plugin_id)?;
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
        } else if let Some(usb_control) = &device.r#match.usb_control {
            if usb_control.vid == 0 || usb_control.pid == 0 {
                bail!("usb_control match requires non-zero vid and pid");
            }
            device.transport = "usb_control".to_owned();
            device.vid = Some(usb_control.vid);
            device.pid = Some(usb_control.pid);
            device.interface = Some(usb_control.interface.into());
        } else if let Some(smbus) = &device.r#match.smbus {
            device.transport = "smbus".to_owned();
            device.bus = Some(smbus.bus.clone());
            device.addresses = Some(smbus.addresses.clone());
            device.extra_addresses = Some(smbus.extra_addresses.clone());
            device.max_bytes_per_sec = smbus.max_bytes_per_sec;
            device.pre_scan = smbus.pre_scan;
            device.probe = smbus.probe;
            device.pci_match = smbus.pci_match.clone();
        } else if let Some(hwmon) = &device.r#match.hwmon {
            if !hwmon.any {
                bail!("hwmon match must explicitly declare `any: true`");
            }
            device.transport = "hwmon".to_owned();
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
const SUPPORTED_CAPABILITIES: &[&str] = &[
    "rgb",
    "fan",
    "sensors",
    "battery",
    "connection",
    "dpi",
    "report_rate",
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
            let fields: HashSet<&str> = manifest
                .config_fields()
                .iter()
                .map(|field| field.key.as_str())
                .collect();
            if !fields.contains(tcp.host_key.as_str()) || !fields.contains(tcp.port_key.as_str()) {
                bail!("tcp host_key and port_key must name declared config fields");
            }
        }
    }
    if let Some(usb) = &manifest.transports.usb_control {
        if manifest.plugin_type != PluginKind::Device {
            bail!("usb_control transport is only valid for a device plugin");
        }
        check_count(
            "usb_control endpoints",
            usb.endpoints.len(),
            MAX_USB_ENDPOINTS,
        )?;
        let mut seen = HashSet::new();
        for ep in &usb.endpoints {
            validate_component("usb_control endpoint id", &ep.id)?;
            if !seen.insert(&ep.id) {
                bail!(
                    "usb_control endpoint id '{}' is declared more than once",
                    ep.id
                );
            }
            if ep.vid == 0 || ep.pid == 0 {
                bail!(
                    "usb_control endpoint '{}' must declare a non-zero vid and pid",
                    ep.id
                );
            }
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

fn validate_controls(manifest: &PluginManifest) -> Result<()> {
    if let Some(config) = &manifest.config {
        check_count("config fields", config.fields.len(), MAX_CONFIG_FIELDS)?;
        let mut keys = HashSet::new();
        for field in &config.fields {
            validate_component("config field key", &field.key)?;
            validate_short_text("config field label", &field.label)?;
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
            match field.kind {
                PluginConfigFieldKind::Text => {
                    if field.min.is_some() || field.max.is_some() {
                        bail!(
                            "text config field '{}' must not declare numeric bounds",
                            field.key
                        );
                    }
                }
                PluginConfigFieldKind::Number => {
                    if field.default.is_empty() {
                        continue;
                    }
                    let value: f64 = field.default.parse().map_err(|_| {
                        anyhow!(
                            "number config field '{}' has a non-numeric default",
                            field.key
                        )
                    })?;
                    if !value.is_finite()
                        || field.min.is_some_and(|v| !v.is_finite())
                        || field.max.is_some_and(|v| !v.is_finite())
                        || field.min.zip(field.max).is_some_and(|(min, max)| min > max)
                        || field.min.is_some_and(|min| value < min)
                        || field.max.is_some_and(|max| value > max)
                    {
                        bail!(
                            "number config field '{}' has invalid bounds or default",
                            field.key
                        );
                    }
                }
            }
        }
    }
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
        transports: meta.transports,
        permissions: meta.permissions,
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

    // ── directory plugins (`plugin.yaml` is authoritative) ─────────────

    const ENTRY_LUA: &str = "return {}";

    fn write_plugin_dir(root: &Path, id: &str, yaml_extra: &str, lua: &str) -> PathBuf {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.yaml"), format!("id: {id}\n{yaml_extra}")).unwrap();
        std::fs::write(dir.join("main.lua"), lua).unwrap();
        dir
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
    fn directory_plugin_indexes_only_its_package_local_lib_modules() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(tmp.path(), "modules", "type: integration\n", "return {}");
        std::fs::create_dir_all(dir.join("lib").join("hidpp")).unwrap();
        std::fs::write(
            dir.join("lib").join("hidpp").join("v1.lua"),
            "return { version = 1 }",
        )
        .unwrap();
        let sibling = write_plugin_dir(tmp.path(), "sibling", "type: integration\n", "return {}");
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
            "type: integration\n",
            "this is deliberately not valid Lua",
        );
        let manifest = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(manifest.plugin_type, PluginKind::Integration);
        assert_eq!(manifest.script_source, "this is deliberately not valid Lua");
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
    fn directory_plugin_uses_nested_usb_control_match() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested_usb");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: nested_usb\ncapabilities: [controls]\ndevices:\n  - vendor: Acme\n    model: Panel\n    type: monitor\n    match:\n      usb_control: { vid: 0x1234, pid: 0x5678 }\ntransports:\n  usb_control: { interface: 0 }\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), ENTRY_LUA).unwrap();
        let manifest = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(manifest.devices[0].transport, "usb_control");
        assert_eq!(manifest.devices[0].vid, Some(0x1234));
    }
}
