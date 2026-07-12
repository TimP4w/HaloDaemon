// SPDX-License-Identifier: GPL-3.0-or-later
//! Parsing a plugin's manifest into a [`PluginManifest`].

use anyhow::{anyhow, bail, Context, Result};
use halod_shared::types::{
    Animation, ButtonDescriptor, ButtonMapping, ChoiceDisplay, ChoiceOption, DeviceType,
    EffectParamDescriptor, NativeEffect, Permission, PluginConfigFieldKind, PluginKind,
    RangeDisplay, RgbDescriptor, RgbZone, ZoneTopology,
};
use mlua::{DeserializeOptions, Lua, LuaSerdeExt};
use serde::{Deserialize, Deserializer, Serialize};
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
        descriptor_for(&self.transport).is_some_and(|d| (d.matches)(self, handle))
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
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&self.manifest_bytes);
        hasher.update(self.script_source.as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
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

    /// Look up a declared accessory by id (for `discover_children`).
    pub fn accessory(&self, id: u8) -> Option<&AccessoryManifest> {
        self.chain.as_ref()?.accessories.iter().find(|a| a.id == id)
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
    let lua = Lua::new();
    // Reading the manifest evaluates the whole script, so strip the same escape
    // hatches the runtime sandbox does and bound its work — a dropped-in file
    // must not run `os`/`io`/`require` or hang the daemon before consent.
    super::sandbox::strip_escape_hatches(&lua).map_err(|e| anyhow!("sandbox setup failed: {e}"))?;
    let _ = lua.set_memory_limit(MANIFEST_MEMORY_LIMIT);
    super::sandbox::install_instruction_budget_hook(&lua, MANIFEST_INSTRUCTION_BUDGET);
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

/// Cross-field validation, gated by `plugin_type`.
fn validate_manifest(manifest: &PluginManifest) -> Result<()> {
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
                    Some(desc) => (desc.validate)(spec)?,
                    None => bail!(
                        "unsupported device transport '{}' (known: {})",
                        spec.transport,
                        known_kinds().join(", ")
                    ),
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
    // Only a `device` plugin may hold a usb_control endpoint open.
    if manifest.transports.usb_control.is_some() && manifest.plugin_type != PluginKind::Device {
        bail!("usb_control transport is only valid for a device plugin");
    }
    // A tcp transport reaches the network, so the manifest must declare the
    // `network` permission — that's what drives the consent prompt and what the
    // tcp backend gates its connect on. Without this a plugin could ship a tcp
    // integration with an empty permission list and auto-activate silently.
    if manifest.transports.tcp.is_some() && !manifest.permissions.contains(&Permission::Network) {
        bail!("a tcp transport requires the 'network' permission to be declared");
    }
    if manifest.effects.iter().any(|e| e.id.is_empty()) {
        bail!("effect declares an empty id");
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
    let dir_name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("plugin directory has no name: {}", dir.display()))?;

    let meta_path = dir.join("plugin.yaml");
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

    let entry_path = dir.join(&meta.entry);
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

    validate_manifest(&manifest)?;
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
    fn gpu_spec_with_pci_match_parses_and_round_trips() {
        let src = r#"return {
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
