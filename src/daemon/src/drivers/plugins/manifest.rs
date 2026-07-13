// SPDX-License-Identifier: GPL-3.0-or-later
//! Parsing a plugin's manifest into a [`PluginManifest`].

use anyhow::{anyhow, bail, Context, Result};
use halod_shared::types::{
    Animation, ButtonDescriptor, ButtonMapping, ChoiceDisplay, ChoiceOption, DeviceType,
    EffectParamDescriptor, EffectParamValue, NativeEffect, ParamKind, Permission,
    PluginConfigFieldKind, PluginKind, RangeDisplay, RgbDescriptor, RgbZone, ZoneTopology,
};
use mlua::{DeserializeOptions, LuaSerdeExt};
use serde::{Deserialize, Deserializer, Serialize};
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
}

impl Default for HidConfig {
    fn default() -> Self {
        Self {
            report_size: default_report_size(),
            timeout_ms: default_timeout_ms(),
            feature_report: false,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransportsConfig {
    #[serde(default)]
    pub hid: Option<HidConfig>,
    #[serde(default)]
    pub tcp: Option<TcpConfig>,
    #[serde(default)]
    pub usb_control: Option<UsbControlConfig>,
}

impl TransportsConfig {
    fn is_empty(&self) -> bool {
        self.hid.is_none() && self.tcp.is_none() && self.usb_control.is_none()
    }
}

/// RGB capability data (zones + native effects). Callbacks (`apply`,
/// `write_frame`) live as sibling functions the worker reads separately.
#[derive(Debug, Clone, Deserialize)]
pub struct RgbManifest {
    pub zones: Vec<RgbZone>,
    #[serde(default)]
    pub native_effects: Vec<NativeEffect>,
}

/// Fan capability marker (pump/fan channel). Presence enables the capability.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FanManifest {
    #[serde(default)]
    pub channel: u8,
}

/// Readings come from the `get_sensors` callback.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SensorManifest {}

/// LCD capability marker. The panel descriptor (resolution, rotations, …) is
/// reported dynamically by `initialize` (it can vary by device variant), so this
/// carries only device-wide LCD policy.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LcdManifest {
    /// Re-apply the RGB state after an image upload (some panels reset the LEDs).
    #[serde(default)]
    pub needs_rgb_restore: bool,
}

/// DPI capability. The host owns the step-list state machine (clamp/index); the
/// plugin only writes the chosen value through its `set_dpi(dev, dpi)` callback.
#[derive(Debug, Clone, Deserialize)]
pub struct DpiManifest {
    pub min: u16,
    pub max: u16,
    /// Ordered DPI steps the step-cycle walks.
    pub steps: Vec<u16>,
    /// `true` = steps live in the device's onboard profile; default host-managed.
    #[serde(default)]
    pub onboard: bool,
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

/// Choice capability: a set of discrete selectors. The host caches the selection
/// and calls `set_choice(dev, key, selected)` to apply it.
#[derive(Debug, Clone, Deserialize)]
pub struct ChoiceManifest {
    pub choices: Vec<ChoiceDef>,
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

/// Range capability: a set of integer controls. The host caches the current
/// value and calls `set_range(dev, key, value)` to apply it.
#[derive(Debug, Clone, Deserialize)]
pub struct RangeManifest {
    pub ranges: Vec<RangeDef>,
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

/// Boolean capability: a set of toggles. The plugin's `get_booleans(dev)`
/// reports current values (readable state may be live, unlike range/choice);
/// `set_boolean(dev, key, value)` applies a write.
#[derive(Debug, Clone, Deserialize)]
pub struct BooleanManifest {
    #[serde(default)]
    pub booleans: Vec<BooleanDef>,
}

/// One fire-and-forget action (button).
#[derive(Debug, Clone, Deserialize)]
pub struct ActionDef {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub category: String,
}

/// Action capability: a set of triggerable buttons, applied via
/// `trigger_action(dev, key)`.
#[derive(Debug, Clone, Deserialize)]
pub struct ActionManifest {
    #[serde(default)]
    pub actions: Vec<ActionDef>,
}

/// Readings come from the `get_batteries` callback.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BatteryManifest {}

/// State comes from the `connection_status` callback.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConnectionManifest {}

/// State comes from the `get_equalizer` callback.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EqualizerManifest {}

/// State comes from the `pairing_status` callback. Unpairing a slot does not
/// (yet) remove a live child `Device` from the registry — wiring paired slots
/// to owned children is a follow-up once a plugin needs it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PairingManifest {}

/// State comes from the `onboard_profiles_status` callback.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OnboardProfilesManifest {}

/// Key-remap capability: the device's remappable buttons + policy, declared
/// statically since they're fixed hardware. Cached mappings are host-owned;
/// writes go through `set_button_mapping`/`reset_button_mapping`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct KeyRemapManifest {
    pub buttons: Vec<ButtonDescriptor>,
    /// True when remapping only takes effect in the device's host mode (as for
    /// Logitech HID++); the GUI shows a "requires host mode" notice.
    #[serde(default)]
    pub requires_host_mode: bool,
    /// Out-of-the-box mappings, seeded on first run and restored by the reset
    /// callbacks.
    #[serde(default)]
    pub default_mappings: Vec<ButtonMapping>,
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

/// One chainable output channel the parent exposes (e.g. an ARGB/accessory port).
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelManifest {
    pub id: String,
    pub name: String,
    pub max_leds: u32,
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

/// Chainable-children capability: the parent hosts child accessories on one or
/// more channels. Requires `detect_accessories` + `write_ext_frame` (and, for
/// accessories with fans, the fan-hub callbacks).
#[derive(Debug, Clone, Deserialize)]
pub struct ChainManifest {
    pub channels: Vec<ChannelManifest>,
    #[serde(default)]
    pub accessories: Vec<AccessoryManifest>,
}

fn default_poll_interval_ms() -> u64 {
    1000
}

/// Background polling. The host runs the loop on the declared interval and calls
/// the plugin's `read_status(dev)` callback; the returned table is stored as
/// `dev.status` for other callbacks (sensors/fan) to read without hitting
/// hardware on every call.
#[derive(Debug, Clone, Deserialize)]
pub struct PollManifest {
    #[serde(default = "default_poll_interval_ms")]
    pub interval_ms: u64,
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
    #[serde(default)]
    pub device_type: Option<DeviceType>,

    pub transport: String,

    // ── HID ──────────────────────────────────────────────────────────────
    #[serde(default)]
    pub vid: Option<u16>,
    #[serde(default)]
    pub pid: Option<u16>,
    /// Match any of several product ids (for device families). Takes precedence
    /// over `pid` when non-empty.
    #[serde(default)]
    pub pids: Vec<u16>,
    #[serde(default)]
    pub usage_page: Option<u16>,
    #[serde(default)]
    pub usage: Option<u16>,
    #[serde(default)]
    pub interface: Option<i32>,

    // ── SMBus (match + scan declaration in one) ──────────────────────────
    /// Bus family to scan/match: "chipset" or "gpu".
    #[serde(default)]
    pub bus: Option<String>,
    /// Addresses the host may probe on the bus (the security boundary).
    #[serde(default)]
    pub addresses: Option<Vec<u8>>,
    /// Extra addresses `pre_scan` may write beyond `addresses` (e.g. an ENE
    /// DRAM broadcast address). Never probed or matched — only in `pre_scan`.
    #[serde(default)]
    pub extra_addresses: Option<Vec<u8>>,
    /// Bus write-rate ceiling applied before any scan traffic.
    #[serde(default)]
    pub max_bytes_per_sec: Option<u32>,
    /// Run the plugin's `pre_scan` callback on each matching bus before probing.
    #[serde(default)]
    pub pre_scan: bool,
    #[serde(default)]
    pub probe: ProbeMode,
    /// PCI-identity gate for GPU buses. Each entry is a `{ vendor, device,
    /// sub_vendor, sub_device, confirmed }` tuple (unset fields are wildcards).
    /// A `bus = "gpu"` spec MUST declare at least one; chipset specs leave it
    /// empty. See [`PciMatch`] and the smbus backend's `validate`.
    #[serde(default)]
    pub pci_match: Vec<PciMatch>,
}

/// Deserialize `devices` as either a single device table or an array of them.
fn de_device_specs<'de, D>(deserializer: D) -> Result<Vec<DeviceSpec>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Box<DeviceSpec>),
        Many(Vec<DeviceSpec>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(m) => vec![*m],
        OneOrMany::Many(v) => v,
    })
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

/// A parsed, validated plugin, built by [`parse_manifest`] or [`parse_manifest_from_dir`].
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    /// Unique per plugin: the directory name, or the script file stem for a built-in.
    #[serde(skip)]
    pub plugin_id: String,
    #[serde(skip)]
    pub source_path: PathBuf,
    /// Full entry-script text, re-executed by the worker to build its own VM.
    #[serde(skip)]
    pub script_source: String,
    /// Directory a plugin was loaded from; empty for a built-in / single-file import.
    #[serde(skip)]
    pub plugin_dir: PathBuf,
    /// Raw bytes of `plugin.yaml`, folded into [`Self::content_hash`]; empty otherwise.
    #[serde(skip)]
    pub manifest_bytes: Vec<u8>,
    #[serde(rename = "devices", default, deserialize_with = "de_device_specs")]
    pub devices: Vec<DeviceSpec>,
    #[serde(default)]
    pub identity: Identity,
    /// Display-only logo asset; directory plugins only, empty for a built-in.
    #[serde(skip)]
    pub logo: Option<String>,
    /// Per-effect thumbnails; directory plugins only, empty for a built-in.
    #[serde(skip)]
    pub effect_thumbnails: Vec<EffectAssetRef>,
    #[serde(rename = "type", default)]
    pub plugin_type: PluginKind,
    #[serde(default)]
    pub effects: Vec<EffectManifest>,
    #[serde(default)]
    pub transports: TransportsConfig,
    #[serde(default)]
    pub rgb: Option<RgbManifest>,
    #[serde(default)]
    pub fan: Option<FanManifest>,
    #[serde(default)]
    pub sensor: Option<SensorManifest>,
    #[serde(default)]
    pub lcd: Option<LcdManifest>,
    #[serde(default)]
    pub dpi: Option<DpiManifest>,
    #[serde(default)]
    pub choice: Option<ChoiceManifest>,
    #[serde(default)]
    pub range: Option<RangeManifest>,
    #[serde(default)]
    pub boolean: Option<BooleanManifest>,
    #[serde(default)]
    pub action: Option<ActionManifest>,
    #[serde(default)]
    pub battery: Option<BatteryManifest>,
    #[serde(default)]
    pub connection: Option<ConnectionManifest>,
    #[serde(default)]
    pub equalizer: Option<EqualizerManifest>,
    #[serde(default)]
    pub pairing: Option<PairingManifest>,
    #[serde(default)]
    pub onboard_profiles: Option<OnboardProfilesManifest>,
    #[serde(default)]
    pub key_remap: Option<KeyRemapManifest>,
    /// Privileged capabilities this plugin needs, gated by user consent.
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub poll: Option<PollManifest>,
    #[serde(default)]
    pub chain: Option<ChainManifest>,
    #[serde(default)]
    pub config: Option<ConfigManifest>,
}

/// `plugin.yaml`: the authoritative manifest for a directory plugin (see [`parse_manifest_from_dir`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default = "default_entry")]
    pub entry: String,
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default, deserialize_with = "de_device_specs")]
    pub devices: Vec<DeviceSpec>,
    #[serde(default)]
    pub transports: TransportsConfig,
    /// Display-only logo, a bare filename under the plugin's `assets/` directory.
    #[serde(default)]
    pub logo: Option<String>,
    /// Per-effect thumbnails, keyed by the entry Lua's declared effect ids.
    #[serde(default)]
    pub effects: Vec<EffectAssetRef>,
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

    /// Hex SHA-256 of `manifest_bytes` + `script_source`; consent is pinned to this.
    pub fn content_hash(&self) -> String {
        plugin_content_hash(&self.manifest_bytes, self.script_source.as_bytes())
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

    /// The RGB descriptor a matched device advertises, if it has RGB.
    pub fn rgb_descriptor(&self) -> Option<RgbDescriptor> {
        self.rgb.as_ref().map(|r| RgbDescriptor {
            zones: r.zones.clone(),
            native_effects: r.native_effects.clone(),
        })
    }

    /// True when the plugin declares any capability that needs a live transport
    /// + worker. Device-only plugins skip the worker.
    pub fn needs_worker(&self) -> bool {
        // An integration plugin declares no capability section (it isn't a
        // capability-bearing device itself), but its root always needs a live
        // transport + Lua worker for `enumerate_controllers`/frame writes.
        self.plugin_type == PluginKind::Integration
            || self.rgb.is_some()
            || self.fan.is_some()
            || self.sensor.is_some()
            || self.lcd.is_some()
            || self.dpi.is_some()
            || self.choice.is_some()
            || self.range.is_some()
            || self.boolean.is_some()
            || self.action.is_some()
            || self.battery.is_some()
            || self.connection.is_some()
            || self.equalizer.is_some()
            || self.pairing.is_some()
            || self.onboard_profiles.is_some()
            || self.key_remap.is_some()
            || self.chain.is_some()
    }

    /// Human-readable capability labels for the management UI.
    pub fn capability_labels(&self) -> Vec<String> {
        let mut labels = Vec::new();
        if self.rgb.is_some() {
            labels.push("RGB".to_owned());
        }
        if self.fan.is_some() {
            labels.push("Fan".to_owned());
        }
        if self.sensor.is_some() {
            labels.push("Sensor".to_owned());
        }
        if self.lcd.is_some() {
            labels.push("LCD".to_owned());
        }
        if self.dpi.is_some() {
            labels.push("DPI".to_owned());
        }
        if self.choice.is_some() {
            labels.push("Settings".to_owned());
        }
        if self.range.is_some() || self.boolean.is_some() || self.action.is_some() {
            labels.push("Controls".to_owned());
        }
        if self.battery.is_some() {
            labels.push("Battery".to_owned());
        }
        if self.connection.is_some() {
            labels.push("Connection".to_owned());
        }
        if self.equalizer.is_some() {
            labels.push("Equalizer".to_owned());
        }
        if self.pairing.is_some() {
            labels.push("Pairing".to_owned());
        }
        if self.onboard_profiles.is_some() {
            labels.push("Onboard".to_owned());
        }
        if self.key_remap.is_some() {
            labels.push("Keys".to_owned());
        }
        if self.chain.is_some() {
            labels.push("Accessories".to_owned());
        }
        labels
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

/// Hash the two text files that define a plugin package. Git may materialize
/// LF blobs as CRLF in a Windows working tree, so normalize CRLF before hashing
/// to keep consent and tamper baselines stable across checkout configurations.
pub(super) fn plugin_content_hash(manifest: &[u8], script: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    fn update_text(hasher: &mut Sha256, bytes: &[u8]) {
        let mut start = 0;
        for (i, pair) in bytes.windows(2).enumerate() {
            if pair == b"\r\n" {
                hasher.update(&bytes[start..i]);
                hasher.update(b"\n");
                start = i + 2;
            }
        }
        hasher.update(&bytes[start..]);
    }

    let mut hasher = Sha256::new();
    update_text(&mut hasher, manifest);
    update_text(&mut hasher, script);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Parse (and validate) a plugin script's manifest. Does not register it.
/// Cap on the throwaway parse VM's heap (8 MiB) — a manifest is small
/// declarative data, so this only bites a script trying to exhaust memory
/// before it ever gets consent.
const MANIFEST_MEMORY_LIMIT: usize = 8 * 1024 * 1024;

/// Instruction budget for evaluating a manifest. Ample for the declarative
/// table plus any trivial helper definitions, but bounds a top-level
/// `while true do end` from hanging daemon load.
const MANIFEST_INSTRUCTION_BUDGET: u64 = 5_000_000;

/// Wall-clock ceiling on evaluating one manifest. The instruction budget catches
/// an *uncaught* runaway, but a `pcall`-catching loop (or a pathological alloc/
/// GC storm) can burn the whole budget repeatedly; since parsing happens on the
/// scanner thread for every dropped-in file *before* consent, a wedged parse
/// would otherwise hang discovery. On timeout the eval thread is abandoned
/// (memory-capped, so bounded) and the plugin is skipped.
const MANIFEST_EVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Evaluate a plugin's Lua source and deserialize its returned table into a bare
/// `PluginManifest`, on a throwaway thread bounded by [`MANIFEST_EVAL_TIMEOUT`].
fn eval_manifest_table(source: &str) -> Result<PluginManifest> {
    let source = source.to_owned();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("halod-manifest-eval".into())
        .spawn(move || {
            let _ = tx.send(eval_manifest_table_inner(&source));
        })
        .map_err(|e| anyhow!("spawning manifest eval thread failed: {e}"))?;
    match rx.recv_timeout(MANIFEST_EVAL_TIMEOUT) {
        Ok(res) => res,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            bail!("manifest evaluation exceeded its {MANIFEST_EVAL_TIMEOUT:?} deadline")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            bail!("manifest eval thread died")
        }
    }
}

fn eval_manifest_table_inner(source: &str) -> Result<PluginManifest> {
    // Reading the manifest evaluates the whole script, so go through the shared VM
    // chokepoint with `StripOnly` — same escape-hatch stripping, memory cap, and
    // instruction budget as the runtime sandbox, but no `halod` runtime surface: a
    // dropped-in file must not run `os`/`io`/`require` or hang the daemon before consent.
    let (lua, _budget) = super::sandbox::bootstrap_vm(
        super::sandbox::InjectSurface::StripOnly,
        MANIFEST_MEMORY_LIMIT,
        MANIFEST_INSTRUCTION_BUDGET,
    )
    .map_err(|e| anyhow!("sandbox setup failed: {e}"))?;
    let value: mlua::Value = lua
        .load(source)
        .eval()
        .map_err(|e| anyhow!("lua evaluation failed: {e}"))?;
    // The manifest table also holds callback *functions* as sibling keys; skip
    // unsupported types (functions → nil) so serde ignores them, rather than
    // erroring on the first function it meets.
    let options = DeserializeOptions::new().deny_unsupported_types(false);
    lua.from_value_with(value, options)
        .map_err(|e| anyhow!("manifest table is malformed: {e}"))
}

/// Upper bound on a plugin-declared LED count for one zone/accessory/channel.
/// Plugins return these as `u32`; the daemon turns each into a native LED-position
/// table and per-frame color buffer, so an unbounded value like `u32::MAX` drives a
/// multi-gigabyte allocation in the root daemon. Rejected (not clamped) so a
/// misdeclaring plugin fails loudly instead of silently truncating.
pub const MAX_PLUGIN_LEDS: u32 = 4_096;

/// Upper bound on a plugin-declared LCD panel dimension. Matches the image-decoder
/// limit in [`crate::util::image`]; bounds the `width * height * 4` host buffers the
/// LCD path allocates from a dynamically-reported panel size.
pub const MAX_PLUGIN_LCD_DIM: u32 = 8_192;

/// Upper bound on the number of RGB zones one device/controller may declare. Each
/// zone's `led_count` is capped, but the daemon builds one native LED-position table
/// *per zone* **after** the plugin VM's memory cap no longer applies, so an unbounded
/// zone count multiplies `MAX_PLUGIN_LEDS` into a multi-gigabyte host allocation.
pub const MAX_PLUGIN_ZONES: usize = 256;

/// Upper bound on the number of controllers one integration may enumerate. Each
/// controller becomes a child device with its own transport connection, VM, and
/// worker thread, so an unbounded count is a memory/connection/thread-exhaustion
/// vector (the deeper thread-ceiling / process-isolation fix is tracked as ARCH-R1).
pub const MAX_PLUGIN_CONTROLLERS: usize = 256;

const MAX_PLUGIN_DEVICES: usize = 256;
const MAX_PLUGIN_EFFECTS: usize = 256;
const MAX_EFFECT_PARAMS: usize = 64;
const MAX_CONFIG_FIELDS: usize = 128;
const MAX_USB_ENDPOINTS: usize = 32;
const MAX_CHAIN_CHANNELS: usize = 64;
const MAX_CHAIN_ACCESSORIES: usize = 256;
const MAX_CONTROL_DEFS: usize = 256;
const MAX_KEY_MAPPINGS: usize = 256;
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

/// Upper bound on the entry Lua source read from disk. The script is a manifest,
/// not a data file; without a cap a symlinked/pathological `entry` (e.g.
/// `/dev/zero`) would drive an unbounded `read_to_string`.
const MAX_ENTRY_BYTES: u64 = 1024 * 1024;

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
    validate_effects(&manifest.effects, "effect")?;
    validate_effect_assets(manifest)?;
    validate_transports(manifest)?;
    validate_controls(manifest)?;
    if let Some(rgb) = &manifest.rgb {
        check_zone_count(rgb.zones.len())?;
        validate_rgb(rgb)?;
    }
    if let Some(chain) = &manifest.chain {
        check_count("chain channels", chain.channels.len(), MAX_CHAIN_CHANNELS)?;
        check_count(
            "chain accessories",
            chain.accessories.len(),
            MAX_CHAIN_ACCESSORIES,
        )?;
        let mut channels = HashSet::new();
        for ch in &chain.channels {
            validate_component("chain channel id", &ch.id)?;
            validate_short_text("chain channel name", &ch.name)?;
            if !channels.insert(&ch.id) {
                bail!("chain channel id '{}' is declared more than once", ch.id);
            }
            check_led_count(&ch.id, ch.max_leds)?;
        }
        let mut accessories = HashSet::new();
        for acc in &chain.accessories {
            validate_short_text("accessory name", &acc.name)?;
            if !accessories.insert(acc.id) {
                bail!("chain accessory id '{}' is declared more than once", acc.id);
            }
            check_led_count(&acc.name, acc.led_count)?;
            match acc.topology.as_str() {
                "ring" | "linear" | "grid" => {
                    if acc.rings != 0 {
                        bail!(
                            "accessory '{}' declares rings for non-rings topology",
                            acc.name
                        );
                    }
                }
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
    }
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
            if !manifest.capability_labels().is_empty() {
                bail!(
                    "integration plugin must not declare capability sections; capabilities are \
                     reported per controller by enumerate_controllers"
                );
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
    validate_component("plugin id", &manifest.plugin_id)?;
    Ok(())
}

fn check_count(what: &str, count: usize, max: usize) -> Result<()> {
    if count > max {
        bail!("declares {count} {what}, exceeding the {max} limit");
    }
    Ok(())
}

fn validate_short_text(what: &str, value: &str) -> Result<()> {
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

fn validate_rgb(rgb: &RgbManifest) -> Result<()> {
    let mut zones = HashSet::new();
    for zone in &rgb.zones {
        validate_component("RGB zone id", &zone.id)?;
        validate_short_text("RGB zone name", &zone.name)?;
        if !zones.insert(&zone.id) {
            bail!("RGB zone id '{}' is declared more than once", zone.id);
        }
        check_led_count(&zone.id, u32::try_from(zone.leds.len()).unwrap_or(u32::MAX))?;
    }
    let effects: Vec<EffectManifest> = rgb
        .native_effects
        .iter()
        .map(|effect| EffectManifest {
            kind: EffectKind::Pixmap,
            id: effect.id.clone(),
            name: effect.name.clone(),
            params: effect.params.clone(),
        })
        .collect();
    check_count("native effects", effects.len(), MAX_PLUGIN_EFFECTS)?;
    validate_effects(&effects, "native effect")
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
        if !(1..=1024).contains(&hid.report_size) || !(1..=60_000).contains(&hid.timeout_ms) {
            bail!("hid transport report_size must be 1..=1024 and timeout_ms 1..=60000");
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
    Ok(())
}

fn validate_controls(manifest: &PluginManifest) -> Result<()> {
    if let Some(dpi) = &manifest.dpi {
        check_count("DPI steps", dpi.steps.len(), MAX_CONTROL_DEFS)?;
        if dpi.min > dpi.max
            || dpi.steps.is_empty()
            || dpi.steps.windows(2).any(|pair| pair[0] >= pair[1])
            || dpi
                .steps
                .iter()
                .any(|step| *step < dpi.min || *step > dpi.max)
        {
            bail!("DPI steps must be non-empty, strictly increasing, and within min/max");
        }
    }
    validate_keyed_controls(manifest)?;
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
    if let Some(poll) = &manifest.poll {
        if !(100..=60_000).contains(&poll.interval_ms) {
            bail!("poll interval_ms must be 100..=60000");
        }
    }
    if let Some(remap) = &manifest.key_remap {
        check_count("key remap buttons", remap.buttons.len(), MAX_KEY_MAPPINGS)?;
        check_count(
            "key remap default mappings",
            remap.default_mappings.len(),
            MAX_KEY_MAPPINGS,
        )?;
        let mut buttons = HashSet::new();
        for button in &remap.buttons {
            validate_short_text("key remap button label", &button.label)?;
            if !buttons.insert(button.cid) {
                bail!("key remap CID {} is declared more than once", button.cid);
            }
        }
        let mut mappings = HashSet::new();
        for mapping in &remap.default_mappings {
            if !mappings.insert(mapping.cid) || !buttons.contains(&mapping.cid) {
                bail!(
                    "key remap default mapping references duplicate or unknown CID {}",
                    mapping.cid
                );
            }
        }
    }
    Ok(())
}

fn validate_keyed_controls(manifest: &PluginManifest) -> Result<()> {
    let mut keys = HashSet::new();
    if let Some(choice) = &manifest.choice {
        check_count("choices", choice.choices.len(), MAX_CONTROL_DEFS)?;
        for value in &choice.choices {
            validate_component("choice key", &value.key)?;
            validate_short_text("choice label", &value.label)?;
            if !keys.insert(&value.key)
                || value.options.is_empty()
                || value.options.len() > MAX_CONTROL_DEFS
                || value.default >= value.options.len()
            {
                bail!(
                    "choice '{}' has duplicate key, invalid option count, or invalid default",
                    value.key
                );
            }
        }
    }
    if let Some(range) = &manifest.range {
        check_count("ranges", range.ranges.len(), MAX_CONTROL_DEFS)?;
        for value in &range.ranges {
            validate_component("range key", &value.key)?;
            validate_short_text("range label", &value.label)?;
            if !keys.insert(&value.key)
                || value.min > value.max
                || value.step <= 0
                || value.default < value.min
                || value.default > value.max
                || (value.default - value.min) % value.step != 0
            {
                bail!(
                    "range '{}' has duplicate key or invalid bounds, step, or default",
                    value.key
                );
            }
        }
    }
    if let Some(boolean) = &manifest.boolean {
        check_count("boolean controls", boolean.booleans.len(), MAX_CONTROL_DEFS)?;
        for value in &boolean.booleans {
            validate_component("boolean key", &value.key)?;
            validate_short_text("boolean label", &value.label)?;
            if !keys.insert(&value.key) {
                bail!("control key '{}' is declared more than once", value.key);
            }
        }
    }
    if let Some(action) = &manifest.action {
        check_count("action controls", action.actions.len(), MAX_CONTROL_DEFS)?;
        for value in &action.actions {
            validate_component("action key", &value.key)?;
            validate_short_text("action label", &value.label)?;
            if !keys.insert(&value.key) {
                bail!("control key '{}' is declared more than once", value.key);
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
fn validate_component(what: &str, value: &str) -> Result<()> {
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

/// Parse a single-file plugin source straight from an in-memory string, with
/// the Lua table as the whole manifest. Every real plugin is a directory
/// package (see [`parse_manifest_from_dir`]); this exists only to build inline
/// Lua fixtures in tests without writing them to disk.
#[cfg(test)]
pub fn parse_manifest(source: &str, path: &Path) -> Result<PluginManifest> {
    let plugin_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("plugin path has no file stem: {}", path.display()))?;

    let mut manifest = eval_manifest_table(source)?;
    manifest.plugin_id = plugin_id;
    manifest.source_path = path.to_path_buf();
    manifest.script_source = source.to_owned();

    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Parse a directory plugin: `dir/plugin.yaml` overlaid on `dir/<entry>`'s capability sections/callbacks.
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

    // plugin.yaml overlays the entry Lua's table for these fields.
    let mut manifest = eval_manifest_table(&source)?;
    manifest.plugin_type = meta.plugin_type;
    manifest.devices = meta.devices;
    manifest.transports = meta.transports;
    manifest.permissions = meta.permissions;
    manifest.identity = Identity {
        name: meta.name,
        id: Some(meta.id.clone()),
        author: meta.author,
        version: meta.version,
        license: meta.license,
        description: meta.description,
    };
    // An undeclared logo defaults to the conventional `assets/logo.png` if one
    // is present, so a plugin author only needs to drop the file in.
    manifest.logo = meta.logo.or_else(|| {
        dir.join("assets")
            .join(DEFAULT_LOGO_NAME)
            .is_file()
            .then(|| DEFAULT_LOGO_NAME.to_owned())
    });
    manifest.effect_thumbnails = meta.effects;

    manifest.plugin_id = meta.id;
    manifest.source_path = entry_path;
    manifest.script_source = source;
    manifest.plugin_dir = dir.to_path_buf();
    manifest.manifest_bytes = manifest_bytes;

    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        return {
          devices = { { transport = "hid", vid = 0x1234, pid = 0x5678, vendor = "Acme", model = "K1" } },
          identity = { name = "Acme K1" },
        }
    "#;

    fn hid<'a>(vid: u16, pid: u16, serial: Option<&'a str>) -> DiscoveryHandle<'a> {
        DiscoveryHandle::Hid {
            vid,
            pid,
            path: "p",
            serial,
            idx: 0,
            usage_page: 0,
            usage: 0,
            interface_number: None,
        }
    }

    #[test]
    fn parses_devices_and_identity() {
        let m = parse_manifest(SAMPLE, Path::new("acme_k1.lua")).unwrap();
        assert_eq!(m.plugin_id, "acme_k1");
        assert_eq!(m.devices[0].vendor, "Acme");
        assert_eq!(m.devices[0].model, "K1");
        assert_eq!(m.display_name_for(&m.devices[0]), "K1");
        assert_eq!(m.devices[0].vid, Some(0x1234));
        assert_eq!(m.devices[0].pid, Some(0x5678));
        assert_eq!(m.id_prefix(), "acme_k1");
        assert_eq!(m.author(), "");
        assert_eq!(m.version(), "");
        assert_eq!(m.license(), "");
        assert_eq!(m.description(), "");
    }

    #[test]
    fn led_and_lcd_caps_reject_oversize_declarations() {
        assert!(check_led_count("z", MAX_PLUGIN_LEDS).is_ok());
        assert!(check_led_count("z", MAX_PLUGIN_LEDS + 1).is_err());
        assert!(check_led_count("z", u32::MAX).is_err());
        assert!(check_lcd_dims(MAX_PLUGIN_LCD_DIM, MAX_PLUGIN_LCD_DIM).is_ok());
        assert!(check_lcd_dims(MAX_PLUGIN_LCD_DIM + 1, 1).is_err());
        assert!(check_lcd_dims(1, u32::MAX).is_err());
    }

    #[test]
    fn zone_count_cap_rejects_oversize_and_allows_boundary() {
        assert!(check_zone_count(MAX_PLUGIN_ZONES).is_ok());
        assert!(check_zone_count(MAX_PLUGIN_ZONES + 1).is_err());
    }

    #[test]
    fn validate_manifest_rejects_too_many_static_rgb_zones() {
        let zones = (0..MAX_PLUGIN_ZONES + 1)
            .map(|i| format!(r#"{{ id = "z{i}", name = "Z{i}", topology = "ring", leds = {{}} }}"#))
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!(
            r#"return {{
              devices = {{ {{ transport = "hid", vid = 1, pid = 2, vendor = "V", model = "M" }} }},
              rgb = {{ zones = {{ {zones} }} }},
            }}"#
        );
        assert!(parse_manifest(&src, Path::new("zones.lua")).is_err());
    }

    #[test]
    fn validate_manifest_rejects_oversize_chain_accessory() {
        let src = format!(
            r#"return {{
              devices = {{ {{ transport = "hid", vid = 1, pid = 2, vendor = "V", model = "M" }} }},
              chain = {{
                channels = {{ {{ id = "c0", name = "C0", max_leds = 8 }} }},
                accessories = {{ {{ id = 1, name = "Strip", led_count = {} }} }},
              }},
            }}"#,
            u32::MAX
        );
        assert!(parse_manifest(&src, Path::new("chain.lua")).is_err());
    }

    #[test]
    fn validate_entry_path_rejects_traversal_and_absolute() {
        assert!(validate_entry_path("main.lua").is_ok());
        assert!(validate_entry_path("sub/main.lua").is_ok());
        assert!(validate_entry_path("./main.lua").is_ok());
        assert!(validate_entry_path("../main.lua").is_err());
        assert!(validate_entry_path("a/../../etc/passwd").is_err());
        assert!(validate_entry_path("/etc/shadow").is_err());
        assert!(validate_entry_path("").is_err());
    }

    #[test]
    fn validate_component_pins_charset() {
        assert!(validate_component("id", "abc_1.2-x").is_ok());
        assert!(validate_component("id", "a:b").is_err());
        assert!(validate_component("id", "a/b").is_err());
        assert!(validate_component("id", "a b").is_err());
        assert!(validate_component("id", "").is_err());
    }

    #[test]
    fn validate_manifest_rejects_bad_usb_endpoints() {
        let dup = r#"return {
            devices = { { transport = "usb_control", vid = 1, pid = 2, vendor = "V", model = "M" } },
            transports = { usb_control = { endpoints = {
              { id = "e", vid = 3, pid = 4 }, { id = "e", vid = 5, pid = 6 } } } },
        }"#;
        assert!(parse_manifest(dup, Path::new("usb.lua")).is_err());

        let zero = r#"return {
            devices = { { transport = "usb_control", vid = 1, pid = 2, vendor = "V", model = "M" } },
            transports = { usb_control = { endpoints = { { id = "e", vid = 0, pid = 4 } } } },
        }"#;
        assert!(parse_manifest(zero, Path::new("usb.lua")).is_err());
    }

    #[test]
    fn manifest_rejects_duplicate_and_invalid_control_declarations() {
        let duplicate = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "V", model = "M" } },
            range = { ranges = {
              { key = "speed", label = "Speed", min = 1, max = 10, step = 1, default = 5 },
              { key = "speed", label = "Again", min = 1, max = 10, step = 1, default = 5 },
            } },
        }"#;
        assert!(parse_manifest(duplicate, Path::new("controls.lua")).is_err());

        let bad_range = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "V", model = "M" } },
            range = { ranges = { { key = "speed", label = "Speed", min = 10, max = 1, default = 5 } } },
        }"#;
        assert!(parse_manifest(bad_range, Path::new("range.lua")).is_err());
    }

    #[test]
    fn manifest_rejects_invalid_config_and_tcp_cross_fields() {
        let bad_default = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "V", model = "M" } },
            config = { fields = { { key = "port", label = "Port", kind = "number", default = "99999", min = 1, max = 65535 } } },
        }"#;
        assert!(parse_manifest(bad_default, Path::new("config.lua")).is_err());

        let missing_tcp_field = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "V", model = "M" } },
            permissions = { "network" },
            config = { fields = { { key = "host", label = "Host" } } },
            transports = { tcp = { host_key = "host", port_key = "port" } },
        }"#;
        assert!(parse_manifest(missing_tcp_field, Path::new("tcp.lua")).is_err());
    }

    #[test]
    fn manifest_rejects_invalid_effect_parameter_definition() {
        let src = r#"return {
            type = "effect",
            effects = { { id = "wave", name = "Wave", params = {
                { id = "speed", label = "Speed", kind = { kind = "range", min = 10, max = 1, step = 1 }, default = 5 },
            } } },
        }"#;
        assert!(parse_manifest(src, Path::new("effect.lua")).is_err());
    }

    #[test]
    fn manifest_rejects_duplicate_key_remap_defaults() {
        let src = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "V", model = "M" } },
            key_remap = {
              buttons = { { cid = 1, label = "Primary", divertable = true, group = 0 } },
              default_mappings = { { cid = 1 }, { cid = 1 } },
            },
        }"#;
        assert!(parse_manifest(src, Path::new("remap.lua")).is_err());
    }

    #[test]
    fn author_version_license_description_parse() {
        let src = r#"
            return {
              devices = { { transport = "hid", vid = 1, pid = 2, vendor = "Acme", model = "K1" } },
              identity = {
                author = "Jane", version = "2.1.0", license = "MIT",
                description = "A keyboard.",
              },
            }
        "#;
        let m = parse_manifest(src, Path::new("k.lua")).unwrap();
        assert_eq!(m.author(), "Jane");
        assert_eq!(m.version(), "2.1.0");
        assert_eq!(m.license(), "MIT");
        assert_eq!(m.description(), "A keyboard.");
    }

    #[test]
    fn target_labels_dedupe_per_device_names() {
        let src = r#"
            return {
              devices = {
                { transport = "hid", vid = 1, pid = 2, vendor = "Acme", model = "K1", name = "Acme K1" },
                { transport = "hid", vid = 1, pid = 3, vendor = "Acme", model = "K2", name = "Acme K2" },
                { transport = "hid", vid = 1, pid = 4, vendor = "Acme", model = "K1b", name = "Acme K1" },
              },
            }
        "#;
        let m = parse_manifest(src, Path::new("k.lua")).unwrap();
        assert_eq!(m.target_labels(), vec!["Acme K1", "Acme K2"]);
    }

    #[test]
    fn multi_device_plugin_parses_distinct_vendor_model() {
        let src = r#"
            return {
              devices = {
                { transport = "hid", vid = 1, pid = 2, vendor = "Acme", model = "K1" },
                { transport = "hid", vid = 1, pid = 3, vendor = "Acme", model = "K2" },
              },
            }
        "#;
        let m = parse_manifest(src, Path::new("multi.lua")).unwrap();
        assert_eq!(m.devices.len(), 2);
        assert_eq!(m.devices[0].model, "K1");
        assert_eq!(m.devices[1].model, "K2");

        let a = m
            .device_for(&hid(1, 2, None))
            .expect("first device matches");
        assert_eq!(a.model, "K1");
        let b = m
            .device_for(&hid(1, 3, None))
            .expect("second device matches");
        assert_eq!(b.model, "K2");
    }

    #[test]
    fn match_predicate_respects_wildcards_and_specifics() {
        let m = parse_manifest(SAMPLE, Path::new("acme_k1.lua")).unwrap();
        assert!(m.device_for(&hid(0x1234, 0x5678, None)).is_some());
        assert!(
            m.device_for(&hid(0x1234, 0x9999, None)).is_none(),
            "pid differs"
        );
        assert!(
            m.device_for(&hid(0x9999, 0x5678, None)).is_none(),
            "vid differs"
        );
    }

    #[test]
    fn pids_list_matches_any_listed_product() {
        let src = r#"return {
            devices = { { transport = "hid", vid = 0x1E71, pids = { 0x3008, 0x300C },
                          vendor = "NZXT", model = "Kraken" } },
        }"#;
        let m = parse_manifest(src, Path::new("k.lua")).unwrap();
        assert!(m.device_for(&hid(0x1E71, 0x3008, None)).is_some());
        assert!(m.device_for(&hid(0x1E71, 0x300C, None)).is_some());
        assert!(
            m.device_for(&hid(0x1E71, 0x2007, None)).is_none(),
            "unlisted pid"
        );
    }

    #[test]
    fn non_table_return_is_error() {
        assert!(parse_manifest("return 42", Path::new("bad.lua")).is_err());
    }

    #[test]
    fn parse_vm_has_no_escape_hatches() {
        // A dropped-in file that tries to run `os`/`io`/`require` at load
        // time must not reach them — evaluating its manifest errors instead.
        for hatch in [
            "os.execute('touch /tmp/pwned')",
            "io.open('/tmp/x', 'w')",
            "require('os')",
        ] {
            let src = format!(
                r#"{hatch}
                   return {{ devices = {{ transport = "hid", vid = 1, pid = 2,
                                           vendor = "x", model = "y" }} }}"#
            );
            assert!(
                parse_manifest(&src, Path::new("evil.lua")).is_err(),
                "escape hatch reachable at parse time: {hatch}"
            );
        }
    }

    #[test]
    fn parse_vm_bounds_a_runaway_loop() {
        let src = r#"while true do end
                     return { devices = { transport = "hid", vid = 1, pid = 2,
                                           vendor = "x", model = "y" } }"#;
        assert!(
            parse_manifest(src, Path::new("loop.lua")).is_err(),
            "an infinite top-level loop must be bounded, not hang"
        );
    }

    #[test]
    fn device_without_vendor_or_model_is_rejected() {
        let src = r#"return { devices = { transport = "hid", vid = 1 } }"#;
        assert!(parse_manifest(src, Path::new("bad.lua")).is_err());
    }

    #[test]
    fn unknown_transport_kind_rejected() {
        let src = r#"return {
            devices = { { transport = "carrier_pigeon", vid = 1, vendor = "x", model = "y" } },
        }"#;
        assert!(parse_manifest(src, Path::new("bad.lua")).is_err());
    }

    #[test]
    fn tcp_transport_requires_the_network_permission() {
        // A tcp transport without a declared `network` permission is rejected,
        // so a plugin can't open a socket without the consent prompt firing.
        let without = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            transports = { tcp = {} },
        }"#;
        assert!(parse_manifest(without, Path::new("bad.lua")).is_err());

        let with = r#"return {
            permissions = {"network"},
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            transports = { tcp = {} },
        }"#;
        assert!(parse_manifest(with, Path::new("ok.lua")).is_ok());
    }

    #[test]
    fn smbus_requires_bus_and_addresses() {
        // Missing bus/addresses is rejected by the smbus backend's validate.
        let src = r#"return {
            devices = { { transport = "smbus", vendor = "x", model = "y" } },
        }"#;
        assert!(parse_manifest(src, Path::new("bad.lua")).is_err());
    }

    #[test]
    fn array_of_devices_parses() {
        let src = r#"return {
            permissions = { "smbus" },
            devices = {
              { transport = "smbus", bus = "chipset", addresses = { 0x70, 0x71 },
                device_type = "ram", vendor = "ENE", model = "DRAM", name = "DRAM" },
              { transport = "smbus", bus = "gpu", addresses = { 0x67 },
                device_type = "gpu", vendor = "ENE", model = "GPU",
                pci_match = { { vendor = 0x10DE, sub_vendor = 0x1043, confirmed = true } } },
            },
          }"#;
        let m = parse_manifest(src, Path::new("ene.lua")).unwrap();
        assert_eq!(m.devices.len(), 2);
        assert_eq!(m.smbus_devices().count(), 2);
        assert_eq!(m.devices[0].device_type, Some(DeviceType::Ram));
    }

    #[test]
    fn device_type_aio_serializes_as_a_i_o_not_aio() {
        // Regression guard: serde's snake_case rename inserts an underscore
        // before every uppercase letter (not just word boundaries), so the
        // all-caps `AIO` variant becomes "a_i_o", not the more intuitive
        // "aio" — a real footgun for plugin authors declaring `device_type`.
        let src = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, device_type = "a_i_o",
                           vendor = "x", model = "y" } },
        }"#;
        let m = parse_manifest(src, Path::new("k.lua")).unwrap();
        assert_eq!(m.devices[0].device_type, Some(DeviceType::AIO));
    }

    #[test]
    fn gpu_spec_without_pci_match_is_rejected() {
        // The GPU I²C bus is shared with the display; a gate is mandatory.
        let src = r#"return {
            devices = { { transport = "smbus", bus = "gpu", addresses = { 0x67 },
                           vendor = "x", model = "y" } },
        }"#;
        assert!(parse_manifest(src, Path::new("bad.lua")).is_err());
    }

    #[test]
    fn smbus_device_requires_smbus_permission() {
        let base = r#"return {
            PERMS
            devices = { { transport = "smbus", bus = "chipset", addresses = { 0x70 },
                           vendor = "x", model = "y" } },
        }"#;
        assert!(parse_manifest(&base.replace("PERMS", ""), Path::new("no.lua")).is_err());
        assert!(parse_manifest(
            &base.replace("PERMS", "permissions = { \"smbus\" },"),
            Path::new("yes.lua")
        )
        .is_ok());
    }

    #[test]
    fn gpu_spec_with_pci_match_parses_and_round_trips() {
        let src = r#"return {
            permissions = { "smbus" },
            devices = { { transport = "smbus", bus = "gpu", addresses = { 0x67 },
              vendor = "ENE", model = "GPU",
              pci_match = {
                { vendor = 0x10DE, device = 0x2684, sub_vendor = 0x1043,
                  sub_device = 0x88BF, confirmed = true },
              } } },
        }"#;
        let m = parse_manifest(src, Path::new("ene.lua")).unwrap();
        let gate = &m.devices[0].pci_match;
        assert_eq!(gate.len(), 1);
        assert_eq!(gate[0].vendor, Some(0x10DE));
        assert_eq!(gate[0].sub_device, Some(0x88BF));
        assert!(gate[0].confirmed);
    }

    #[test]
    fn range_boolean_action_battery_connection_equalizer_sections_parse() {
        let src = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            range = { ranges = { { key = "hz", label = "Hz", min = 125, max = 1000, default = 500 } } },
            boolean = { booleans = { { key = "sniper", label = "Sniper" } } },
            action = { actions = { { key = "cal", label = "Calibrate" } } },
            battery = {},
            connection = {},
            equalizer = {},
        }"#;
        let m = parse_manifest(src, Path::new("controls.lua")).unwrap();
        assert!(m.needs_worker());
        let labels = m.capability_labels();
        assert!(labels.contains(&"Controls".to_owned()));
        assert!(labels.contains(&"Battery".to_owned()));
        assert!(labels.contains(&"Connection".to_owned()));
        assert!(labels.contains(&"Equalizer".to_owned()));
        assert_eq!(m.range.unwrap().ranges[0].key, "hz");
        assert_eq!(m.boolean.unwrap().booleans[0].key, "sniper");
        assert_eq!(m.action.unwrap().actions[0].key, "cal");
        assert!(m.battery.is_some());
        assert!(m.connection.is_some());
        assert!(m.equalizer.is_some());
    }

    #[test]
    fn pairing_onboard_profiles_key_remap_sections_parse() {
        let src = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            pairing = {},
            onboard_profiles = {},
            key_remap = {
              buttons = { { cid = 1, label = "Left", divertable = true, group = 0 } },
              requires_host_mode = true,
            },
        }"#;
        let m = parse_manifest(src, Path::new("receiver.lua")).unwrap();
        assert!(m.needs_worker());
        let labels = m.capability_labels();
        assert!(labels.contains(&"Pairing".to_owned()));
        assert!(labels.contains(&"Onboard".to_owned()));
        assert!(labels.contains(&"Keys".to_owned()));
        assert!(m.pairing.is_some());
        assert!(m.onboard_profiles.is_some());
        let key_remap = m.key_remap.unwrap();
        assert_eq!(key_remap.buttons[0].cid, 1);
        assert!(key_remap.requires_host_mode);
    }

    #[test]
    fn permissions_section_parses_and_defaults_to_empty() {
        let src = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            permissions = { "network", "os" },
        }"#;
        let m = parse_manifest(src, Path::new("net.lua")).unwrap();
        assert_eq!(m.permissions, vec![Permission::Network, Permission::Os]);

        let no_perms = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        }"#;
        let m = parse_manifest(no_perms, Path::new("no_perms.lua")).unwrap();
        assert!(m.permissions.is_empty());
    }

    #[test]
    fn effect_only_plugin_needs_no_devices() {
        let src = r#"return {
            identity = { name = "Effects" },
            type = "effect",
            effects = {
              { kind = "pixmap", id = "plasma", name = "Plasma",
                params = { { id = "speed", label = "Speed",
                             kind = { kind = "range", min = 0.1, max = 3.0, step = 0.1 },
                             default = 0.5 } } },
              { kind = "direct", id = "comet", name = "Comet" },
            },
        }"#;
        let m = parse_manifest(src, Path::new("fx.lua")).unwrap();
        assert!(m.devices.is_empty());
        assert_eq!(m.plugin_type, PluginKind::Effect);
        assert!(!m.needs_worker(), "effects never need the device worker");
        assert!(
            m.capability_labels().is_empty(),
            "effects aren't a capability"
        );
        assert_eq!(m.effects.len(), 2);
        assert_eq!(m.effects[0].catalog_id("fx"), "fx:plasma");
        assert_eq!(m.effects[0].kind, EffectKind::Pixmap);
        assert_eq!(m.effects[1].kind, EffectKind::Direct);

        let descriptor = m.effects[0].descriptor("fx");
        assert_eq!(descriptor.id, "fx:plasma");
        assert_eq!(descriptor.params.len(), 1);
    }

    #[test]
    fn effect_plugin_with_devices_is_rejected() {
        let src = r#"return {
            type = "effect",
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            effects = { { kind = "direct", id = "pulse", name = "Pulse" } },
        }"#;
        assert!(parse_manifest(src, Path::new("bad.lua")).is_err());
    }

    #[test]
    fn integration_plugin_with_devices_is_rejected() {
        let src = r#"return {
            type = "integration",
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        }"#;
        assert!(parse_manifest(src, Path::new("bad.lua")).is_err());
    }

    #[test]
    fn integration_plugin_with_static_capability_section_is_rejected() {
        // Use a simple capability section (fan) that deserialises cleanly
        // without needing complex sub-types, so the error comes from the
        // integration validation rule, not from field-level parsing.
        let src = r#"return {
            type = "integration",
            fan = { channels = 1 },
        }"#;
        let err = parse_manifest(src, Path::new("bad.lua")).unwrap_err();
        assert!(
            err.to_string()
                .contains("integration plugin must not declare capability sections"),
            "expected capability-section rejection, got: {err}"
        );
    }

    #[test]
    fn device_plugin_with_empty_devices_is_rejected() {
        let src = r#"return { identity = { name = "y" } }"#;
        assert!(parse_manifest(src, Path::new("bad.lua")).is_err());
    }

    #[test]
    fn effect_with_empty_id_is_rejected() {
        let src = r#"return {
            type = "effect",
            effects = { { kind = "pixmap", id = "", name = "Nope" } },
        }"#;
        assert!(parse_manifest(src, Path::new("bad2.lua")).is_err());
    }

    #[test]
    fn plugin_type_defaults_to_device() {
        let m = parse_manifest(SAMPLE, Path::new("acme_k1.lua")).unwrap();
        assert_eq!(m.plugin_type, PluginKind::Device);
    }

    #[test]
    fn device_plugin_can_also_bundle_effects() {
        let src = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            effects = { { kind = "direct", id = "pulse", name = "Pulse" } },
        }"#;
        let m = parse_manifest(src, Path::new("bundled.lua")).unwrap();
        assert_eq!(m.devices.len(), 1);
        assert_eq!(m.effects.len(), 1);
    }

    #[test]
    fn config_section_parses_fields_with_defaults() {
        let src = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            config = { fields = {
              { key = "host", label = "Host", default = "127.0.0.1" },
              { key = "port", label = "Port", kind = "number", default = "6742" },
              { key = "token", label = "API Token", secure = true },
            } },
        }"#;
        let m = parse_manifest(src, Path::new("cfg.lua")).unwrap();
        let fields = m.config_fields();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].key, "host");
        assert_eq!(fields[0].kind, PluginConfigFieldKind::Text);
        assert_eq!(fields[0].default, "127.0.0.1");
        assert!(!fields[0].secure);
        assert_eq!(fields[1].kind, PluginConfigFieldKind::Number);
        assert!(fields[2].secure);
    }

    #[test]
    fn secure_config_keys_returns_only_secure_fields() {
        let src = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            config = { fields = {
              { key = "host", label = "Host" },
              { key = "token", label = "Token", secure = true },
              { key = "secret2", label = "Secret 2", secure = true },
            } },
        }"#;
        let m = parse_manifest(src, Path::new("cfg2.lua")).unwrap();
        assert_eq!(m.secure_config_keys(), vec!["token", "secret2"]);
    }

    #[test]
    fn config_fields_empty_when_no_config_section() {
        let m = parse_manifest(SAMPLE, Path::new("acme_k1.lua")).unwrap();
        assert!(m.config_fields().is_empty());
        assert!(m.secure_config_keys().is_empty());
    }

    #[test]
    fn config_section_does_not_affect_capability_labels_or_needs_worker() {
        let src = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            config = { fields = { { key = "host", label = "Host" } } },
        }"#;
        let m = parse_manifest(src, Path::new("cfg3.lua")).unwrap();
        assert!(m.capability_labels().is_empty());
        assert!(!m.needs_worker());
    }

    #[test]
    fn tcp_transport_config_parses_with_defaults() {
        let src = r#"return {
            permissions = {"network"},
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            transports = { tcp = { host_key = "ip", port_key = "svc_port" } },
        }"#;
        let m = parse_manifest(src, Path::new("tcpcfg.lua")).unwrap();
        let tcp = m.transports.tcp.expect("tcp transport config");
        assert_eq!(tcp.host_key, "ip");
        assert_eq!(tcp.port_key, "svc_port");
        assert_eq!(tcp.timeout_ms, 5000);
    }

    #[test]
    fn tcp_transport_config_defaults_keys_when_omitted() {
        let src = r#"return {
            permissions = {"network"},
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            transports = { tcp = { timeout_ms = 2000 } },
        }"#;
        let m = parse_manifest(src, Path::new("tcpcfg2.lua")).unwrap();
        let tcp = m.transports.tcp.expect("tcp transport config");
        assert_eq!(tcp.host_key, "host");
        assert_eq!(tcp.port_key, "port");
        assert_eq!(tcp.timeout_ms, 2000);
    }

    #[test]
    fn integration_plugin_parses_with_no_devices() {
        let src = r#"return {
            identity = { name = "OpenRGB" },
            type = "integration",
            permissions = { "network" },
            config = { fields = {
              { key = "host", label = "Host", default = "127.0.0.1" },
              { key = "port", label = "Port", default = "6742" },
            } },
            transports = { tcp = {} },
        }"#;
        let m = parse_manifest(src, Path::new("integ.lua")).unwrap();
        assert!(m.devices.is_empty());
        assert_eq!(m.plugin_type, PluginKind::Integration);
    }

    #[test]
    fn integration_plugin_needs_a_worker_even_with_no_capability_sections() {
        let src = r#"return {
            type = "integration",
        }"#;
        let m = parse_manifest(src, Path::new("integ2.lua")).unwrap();
        assert!(m.needs_worker());
        assert!(
            m.capability_labels().is_empty(),
            "an integration root isn't a capability-bearing device itself"
        );
    }

    #[test]
    fn integration_plugin_with_neither_devices_nor_config_still_parses() {
        // Integration only exempts the devices guard, not config fields.
        let src = r#"return {
            type = "integration",
        }"#;
        assert!(parse_manifest(src, Path::new("integ3.lua")).is_ok());
    }

    // ── directory plugins (`plugin.yaml` overlay) ───────────────────────

    const ENTRY_LUA: &str = r#"
        return {
          rgb = { zones = {} },
        }
    "#;

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
            "devices:\n  - vendor: Acme\n    model: K1\n    transport: hid\n    vid: 1\n    pid: 2\n",
            ENTRY_LUA,
        );
        let m = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(m.plugin_id, "dirplug");
        assert_eq!(m.devices.len(), 1);
        assert_eq!(m.devices[0].vendor, "Acme");
        assert!(
            m.rgb.is_some(),
            "entry Lua's capability sections still apply"
        );
    }

    #[test]
    fn directory_plugin_explicit_entry_and_license_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dirplug2");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: dirplug2\nentry: driver.lua\nlicense: GPL-3.0-or-later\ndevices:\n  - vendor: Acme\n    model: K1\n    transport: hid\n    vid: 1\n    pid: 2\n",
        )
        .unwrap();
        std::fs::write(dir.join("driver.lua"), ENTRY_LUA).unwrap();

        let m = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(m.license(), "GPL-3.0-or-later");
        assert_eq!(m.source_path, dir.join("driver.lua"));
    }

    #[test]
    fn directory_plugin_yaml_type_overlay_wins_over_lua_declarations() {
        let tmp = tempfile::tempdir().unwrap();
        // The entry Lua's devices/identity/type must be discarded in favor of plugin.yaml.
        let lua = r#"return {
            type = "integration",
            devices = { { transport = "hid", vid = 9, pid = 9, vendor = "Lua", model = "Ignored" } },
            identity = { name = "Ignored Name" },
            rgb = { zones = {} },
        }"#;
        let dir = write_plugin_dir(
            tmp.path(),
            "overlaid",
            "type: device\nname: Real Name\ndevices:\n  - vendor: Real\n    model: K1\n    transport: hid\n    vid: 1\n    pid: 2\n",
            lua,
        );
        let m = parse_manifest_from_dir(&dir).unwrap();
        assert_eq!(m.plugin_type, PluginKind::Device);
        assert_eq!(m.devices.len(), 1);
        assert_eq!(m.devices[0].vendor, "Real");
        assert_eq!(m.identity.name.as_deref(), Some("Real Name"));
        assert!(
            m.rgb.is_some(),
            "non-overlaid capability sections still come from Lua"
        );
    }

    #[test]
    fn directory_name_mismatch_with_yaml_id_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "actual-dir-name",
            "devices:\n  - vendor: A\n    model: B\n    transport: hid\n    vid: 1\n    pid: 2\n",
            ENTRY_LUA,
        );
        // Rewrite plugin.yaml claiming a different id than the directory name.
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: someone-else\ndevices:\n  - vendor: A\n    model: B\n    transport: hid\n    vid: 1\n    pid: 2\n",
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
            "devices:\n  - vendor: A\n    model: B\n    transport: hid\n    vid: 1\n    pid: 2\n",
            ENTRY_LUA,
        );
        std::fs::remove_file(no_entry.join("main.lua")).unwrap();
        assert!(parse_manifest_from_dir(&no_entry).is_err());
    }

    #[test]
    fn content_hash_changes_when_plugin_yaml_or_entry_lua_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(
            tmp.path(),
            "hashtest",
            "devices:\n  - vendor: A\n    model: B\n    transport: hid\n    vid: 1\n    pid: 2\n",
            ENTRY_LUA,
        );
        let original = parse_manifest_from_dir(&dir).unwrap().content_hash();

        // Entry Lua changes, plugin.yaml unchanged.
        std::fs::write(
            dir.join("main.lua"),
            "return { rgb = { zones = {} }, x = 1 }",
        )
        .unwrap();
        let lua_changed = parse_manifest_from_dir(&dir).unwrap().content_hash();
        assert_ne!(
            original, lua_changed,
            "editing the entry Lua must move the hash"
        );

        // plugin.yaml changes, entry Lua restored.
        std::fs::write(dir.join("main.lua"), ENTRY_LUA).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: hashtest\ndevices:\n  - vendor: A\n    model: C\n    transport: hid\n    vid: 1\n    pid: 2\n",
        )
        .unwrap();
        let yaml_changed = parse_manifest_from_dir(&dir).unwrap().content_hash();
        assert_ne!(
            original, yaml_changed,
            "editing plugin.yaml must move the hash"
        );
    }

    #[test]
    fn plugin_meta_devices_round_trip_through_yaml() {
        // `devices:` accepts a plain YAML list, same as the Lua one-or-many helper.
        let yaml = "id: rt\ndevices:\n  - vendor: Acme\n    model: K1\n    transport: hid\n    vid: 1\n  - vendor: Acme\n    model: K2\n    transport: hid\n    vid: 2\n";
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
            "devices:\n  - vendor: A\n    model: B\n    transport: hid\n    vid: 1\n    pid: 2\n",
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
            "devices:\n  - vendor: A\n    model: B\n    transport: hid\n    vid: 1\n    pid: 2\n\
             logo: logo.png\n\
             effects:\n  - id: rainbow\n    thumbnail: rainbow.png\n",
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
            "devices:\n  - vendor: A\n    model: B\n    transport: hid\n    vid: 1\n    pid: 2\n",
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
    fn builtin_plugin_never_sets_logo_or_thumbnails() {
        // No `plugin.yaml` overlay for a built-in, so these stay at their defaults.
        let m = parse_manifest(SAMPLE, Path::new("acme_k1.lua")).unwrap();
        assert!(m.logo.is_none());
        assert!(m.effect_thumbnails.is_empty());
    }

    #[test]
    fn plugin_meta_single_device_may_be_a_bare_table() {
        let yaml = "id: rt2\ndevices:\n  vendor: Acme\n  model: K1\n  transport: hid\n  vid: 1\n";
        let parsed: PluginMeta = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.devices.len(), 1);
        assert_eq!(parsed.devices[0].vendor, "Acme");
    }
}
