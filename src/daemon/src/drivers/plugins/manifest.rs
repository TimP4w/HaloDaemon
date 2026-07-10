// SPDX-License-Identifier: GPL-3.0-or-later
//! Parsing a plugin script's `return`ed table into a `PluginManifest`.
//!
//! Parsing runs the script once in a throwaway Lua VM purely to read the
//! declarative `match`/`identity` tables; that VM is dropped immediately. The
//! per-device worker later builds its own VM from `script_source` when a device
//! actually matches.

use anyhow::{anyhow, bail, Result};
use halod_shared::types::{
    Animation, ButtonDescriptor, ButtonMapping, ChoiceDisplay, ChoiceOption, DeviceType,
    EffectParamDescriptor, NativeEffect, Permission, RangeDisplay, RgbDescriptor, RgbZone,
    ZoneTopology,
};
use mlua::{DeserializeOptions, Lua, LuaSerdeExt};
use serde::Deserialize;
use std::path::Path;

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
#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TransportsConfig {
    #[serde(default)]
    pub hid: Option<HidConfig>,
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

/// Sensor capability marker (data-less; readings come from `get_sensors`).
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

/// Battery capability marker (data-less; readings come from `get_batteries`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BatteryManifest {}

/// Connection-status capability marker (data-less; state comes from
/// `connection_status`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConnectionManifest {}

/// Equalizer capability marker (data-less; presets/bands/values all come from
/// `get_equalizer`; applied via `set_eq_preset`/`set_eq_bands`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EqualizerManifest {}

/// Pairing capability marker (data-less; state comes from `pairing_status`,
/// applied via `start_pairing`/`stop_pairing`/`unpair`). Unpairing a slot does
/// not (yet) remove a live child `Device` from the registry — the plugin
/// handles the hardware side; wiring paired slots to owned children is a
/// follow-up once a plugin needs it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PairingManifest {}

/// Onboard-profile capability marker (data-less; state comes from
/// `onboard_profiles_status`, applied via `switch_profile`/`restore_profile`/
/// `set_profile_enabled`).
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

/// Which discovery path a plugin registers into. `Device` (the default)
/// declares hardware via `match`; `Effect` declares RGB effects and needs no
/// `match` spec at all — it never opens a transport or a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginType {
    #[default]
    Device,
    Effect,
}

impl From<PluginType> for halod_shared::types::PluginKind {
    fn from(t: PluginType) -> Self {
        match t {
            PluginType::Device => halod_shared::types::PluginKind::Device,
            PluginType::Effect => halod_shared::types::PluginKind::Effect,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
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

/// Declarative device match. One spec per hardware shape a plugin drives; a
/// plugin may declare several (e.g. an SMBus DRAM controller *and* a GPU one).
/// `None` fields mean "don't care". Which fields are required is enforced by
/// the transport backend's `validate` (HID needs `vid`; SMBus needs
/// `bus`+`addresses`).
#[derive(Debug, Clone, Deserialize)]
pub struct MatchSpec {
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

    // ── Per-spec identity overrides (so one plugin covers several devices) ─
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub device_type: Option<DeviceType>,
}

/// Accepts either a single `match` table or an array of them.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MatchSpecs {
    One(MatchSpec),
    Many(Vec<MatchSpec>),
}

impl MatchSpecs {
    fn into_vec(self) -> Vec<MatchSpec> {
        match self {
            MatchSpecs::One(m) => vec![m],
            MatchSpecs::Many(v) => v,
        }
    }
}

impl MatchSpec {
    /// The SMBus bus family this spec targets, if it is an SMBus spec.
    pub fn bus_kind(&self) -> Option<SmbusBusKind> {
        match self.bus.as_deref() {
            Some("chipset") => Some(SmbusBusKind::Chipset),
            Some("gpu") => Some(SmbusBusKind::Gpu),
            _ => None,
        }
    }

    /// Addresses a `pre_scan` on this spec may write: declared + extras.
    pub fn pre_scan_scope(&self) -> Vec<u8> {
        let mut v = self.addresses.clone().unwrap_or_default();
        v.extend(self.extra_addresses.iter().flatten().copied());
        v
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Identity {
    pub vendor: String,
    pub model: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Optional stable id prefix; defaults to the plugin id (script file stem).
    #[serde(default)]
    pub id: Option<String>,
    /// Who wrote the plugin (surfaced in the Plugins screen).
    #[serde(default)]
    pub author: Option<String>,
    /// Plugin version string, e.g. "1.2.0".
    #[serde(default)]
    pub version: Option<String>,
    /// Free-text description of what the plugin does.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    #[serde(rename = "match", default)]
    match_spec: Option<MatchSpecs>,
    identity: Identity,
    #[serde(rename = "type", default)]
    plugin_type: PluginType,
    #[serde(default)]
    effects: Vec<EffectManifest>,
    #[serde(default)]
    transports: TransportsConfig,
    #[serde(default)]
    rgb: Option<RgbManifest>,
    #[serde(default)]
    fan: Option<FanManifest>,
    #[serde(default)]
    sensor: Option<SensorManifest>,
    #[serde(default)]
    lcd: Option<LcdManifest>,
    #[serde(default)]
    dpi: Option<DpiManifest>,
    #[serde(default)]
    choice: Option<ChoiceManifest>,
    #[serde(default)]
    range: Option<RangeManifest>,
    #[serde(default)]
    boolean: Option<BooleanManifest>,
    #[serde(default)]
    action: Option<ActionManifest>,
    #[serde(default)]
    battery: Option<BatteryManifest>,
    #[serde(default)]
    connection: Option<ConnectionManifest>,
    #[serde(default)]
    equalizer: Option<EqualizerManifest>,
    #[serde(default)]
    pairing: Option<PairingManifest>,
    #[serde(default)]
    onboard_profiles: Option<OnboardProfilesManifest>,
    #[serde(default)]
    key_remap: Option<KeyRemapManifest>,
    /// Privileged capabilities this plugin needs, gated by user consent.
    #[serde(default)]
    permissions: Vec<Permission>,
    #[serde(default)]
    poll: Option<PollManifest>,
    #[serde(default)]
    chain: Option<ChainManifest>,
}

/// A parsed, validated plugin ready to be matched against discovery handles.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    /// Unique per plugin (the script file stem).
    pub plugin_id: String,
    pub source_path: std::path::PathBuf,
    /// Full script text, re-executed by the worker to build its own VM.
    pub script_source: String,
    pub match_specs: Vec<MatchSpec>,
    pub identity: Identity,
    pub plugin_type: PluginType,
    pub effects: Vec<EffectManifest>,
    pub transports: TransportsConfig,
    pub rgb: Option<RgbManifest>,
    pub fan: Option<FanManifest>,
    pub sensor: Option<SensorManifest>,
    pub lcd: Option<LcdManifest>,
    pub dpi: Option<DpiManifest>,
    pub choice: Option<ChoiceManifest>,
    pub range: Option<RangeManifest>,
    pub boolean: Option<BooleanManifest>,
    pub action: Option<ActionManifest>,
    pub battery: Option<BatteryManifest>,
    pub connection: Option<ConnectionManifest>,
    pub equalizer: Option<EqualizerManifest>,
    pub pairing: Option<PairingManifest>,
    pub onboard_profiles: Option<OnboardProfilesManifest>,
    pub key_remap: Option<KeyRemapManifest>,
    pub permissions: Vec<Permission>,
    pub poll: Option<PollManifest>,
    pub chain: Option<ChainManifest>,
}

impl MatchSpec {
    /// Does this spec accept the handle a bus scanner produced? Delegated to the
    /// transport backend registered for `self.transport` (unknown kind → never).
    pub fn matches(&self, handle: &DiscoveryHandle<'_>) -> bool {
        descriptor_for(&self.transport).is_some_and(|d| (d.matches)(self, handle))
    }
}

impl PluginManifest {
    /// The first declared spec that accepts `handle`, if any.
    pub fn match_spec_for(&self, handle: &DiscoveryHandle<'_>) -> Option<&MatchSpec> {
        self.match_specs.iter().find(|s| s.matches(handle))
    }

    /// SMBus specs that request a bus scan (all SMBus specs declare addresses).
    pub fn smbus_specs(&self) -> impl Iterator<Item = &MatchSpec> {
        self.match_specs.iter().filter(|s| s.bus_kind().is_some())
    }

    /// Whether any declared spec drives an SMBus bus.
    pub fn has_smbus(&self) -> bool {
        self.smbus_specs().next().is_some()
    }

    /// Stable id prefix a matched device's id is built from.
    pub fn id_prefix(&self) -> &str {
        self.identity.id.as_deref().unwrap_or(&self.plugin_id)
    }

    /// Human-readable device name for a matched spec (per-spec override wins).
    pub fn display_name_for(&self, spec: &MatchSpec) -> String {
        spec.name
            .clone()
            .or_else(|| self.identity.name.clone())
            .unwrap_or_else(|| self.identity.model.clone())
    }

    /// Human-readable device name (first spec / identity fallback).
    pub fn display_name(&self) -> &str {
        self.identity
            .name
            .as_deref()
            .unwrap_or(&self.identity.model)
    }

    /// Declared plugin author (empty when unset).
    pub fn author(&self) -> &str {
        self.identity.author.as_deref().unwrap_or("")
    }

    /// Declared plugin version (empty when unset).
    pub fn version(&self) -> &str {
        self.identity.version.as_deref().unwrap_or("")
    }

    /// Declared plugin description (empty when unset).
    pub fn description(&self) -> &str {
        self.identity.description.as_deref().unwrap_or("")
    }

    /// Device labels the plugin targets — the per-spec display name of every
    /// match spec, de-duplicated in declaration order.
    pub fn target_labels(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for spec in &self.match_specs {
            let label = self.display_name_for(spec);
            if !out.contains(&label) {
                out.push(label);
            }
        }
        out
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
        self.rgb.is_some()
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
}

/// Parse (and validate) a plugin script's manifest. Does not register it.
pub fn parse_manifest(source: &str, path: &Path) -> Result<PluginManifest> {
    let plugin_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("plugin path has no file stem: {}", path.display()))?;

    let lua = Lua::new();
    let value: mlua::Value = lua
        .load(source)
        .eval()
        .map_err(|e| anyhow!("lua evaluation failed: {e}"))?;
    // The manifest table also holds callback *functions* as sibling keys; skip
    // unsupported types (functions → nil) so serde ignores them, rather than
    // erroring on the first function it meets.
    let options = DeserializeOptions::new().deny_unsupported_types(false);
    let raw: RawManifest = lua
        .from_value_with(value, options)
        .map_err(|e| anyhow!("manifest table is malformed: {e}"))?;

    let match_specs = raw.match_spec.map(MatchSpecs::into_vec).unwrap_or_default();
    if match_specs.is_empty() && raw.effects.is_empty() {
        bail!("plugin declares neither a match spec nor any effects");
    }
    // Validate every spec against its registered transport backend: unknown
    // kinds and missing required fields are rejected here, not at match time.
    for spec in &match_specs {
        match descriptor_for(&spec.transport) {
            Some(desc) => (desc.validate)(spec)?,
            None => bail!(
                "unsupported match transport '{}' (known: {})",
                spec.transport,
                known_kinds().join(", ")
            ),
        }
    }
    if raw.effects.iter().any(|e| e.id.is_empty()) {
        bail!("effect declares an empty id");
    }

    Ok(PluginManifest {
        plugin_id,
        source_path: path.to_path_buf(),
        script_source: source.to_owned(),
        match_specs,
        identity: raw.identity,
        plugin_type: raw.plugin_type,
        effects: raw.effects,
        transports: raw.transports,
        rgb: raw.rgb,
        fan: raw.fan,
        sensor: raw.sensor,
        lcd: raw.lcd,
        dpi: raw.dpi,
        choice: raw.choice,
        range: raw.range,
        boolean: raw.boolean,
        action: raw.action,
        battery: raw.battery,
        connection: raw.connection,
        equalizer: raw.equalizer,
        pairing: raw.pairing,
        onboard_profiles: raw.onboard_profiles,
        key_remap: raw.key_remap,
        permissions: raw.permissions,
        poll: raw.poll,
        chain: raw.chain,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        return {
          match = { transport = "hid", vid = 0x1234, pid = 0x5678 },
          identity = { vendor = "Acme", model = "K1", name = "Acme K1" },
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
    fn parses_match_and_identity() {
        let m = parse_manifest(SAMPLE, Path::new("acme_k1.lua")).unwrap();
        assert_eq!(m.plugin_id, "acme_k1");
        assert_eq!(m.identity.vendor, "Acme");
        assert_eq!(m.display_name(), "Acme K1");
        assert_eq!(m.match_specs[0].vid, Some(0x1234));
        assert_eq!(m.match_specs[0].pid, Some(0x5678));
        assert_eq!(m.id_prefix(), "acme_k1");
    }

    #[test]
    fn author_version_description_default_empty_and_parse() {
        let m = parse_manifest(SAMPLE, Path::new("acme_k1.lua")).unwrap();
        assert_eq!(m.author(), "");
        assert_eq!(m.version(), "");
        assert_eq!(m.description(), "");

        let src = r#"
            return {
              match = { transport = "hid", vid = 1, pid = 2 },
              identity = {
                vendor = "Acme", model = "K1",
                author = "Jane", version = "2.1.0", description = "A keyboard.",
              },
            }
        "#;
        let m = parse_manifest(src, Path::new("k.lua")).unwrap();
        assert_eq!(m.author(), "Jane");
        assert_eq!(m.version(), "2.1.0");
        assert_eq!(m.description(), "A keyboard.");
    }

    #[test]
    fn target_labels_dedupe_per_spec_names() {
        let src = r#"
            return {
              match = {
                { transport = "hid", vid = 1, pid = 2, name = "Acme K1" },
                { transport = "hid", vid = 1, pid = 3, name = "Acme K2" },
                { transport = "hid", vid = 1, pid = 4, name = "Acme K1" },
              },
              identity = { vendor = "Acme", model = "K" },
            }
        "#;
        let m = parse_manifest(src, Path::new("k.lua")).unwrap();
        assert_eq!(m.target_labels(), vec!["Acme K1", "Acme K2"]);
    }

    #[test]
    fn match_predicate_respects_wildcards_and_specifics() {
        let m = parse_manifest(SAMPLE, Path::new("acme_k1.lua")).unwrap();
        assert!(m.match_spec_for(&hid(0x1234, 0x5678, None)).is_some());
        assert!(
            m.match_spec_for(&hid(0x1234, 0x9999, None)).is_none(),
            "pid differs"
        );
        assert!(
            m.match_spec_for(&hid(0x9999, 0x5678, None)).is_none(),
            "vid differs"
        );
    }

    #[test]
    fn pids_list_matches_any_listed_product() {
        let src = r#"return {
            match = { transport = "hid", vid = 0x1E71, pids = { 0x3008, 0x300C } },
            identity = { vendor = "NZXT", model = "Kraken" },
        }"#;
        let m = parse_manifest(src, Path::new("k.lua")).unwrap();
        assert!(m.match_spec_for(&hid(0x1E71, 0x3008, None)).is_some());
        assert!(m.match_spec_for(&hid(0x1E71, 0x300C, None)).is_some());
        assert!(
            m.match_spec_for(&hid(0x1E71, 0x2007, None)).is_none(),
            "unlisted pid"
        );
    }

    #[test]
    fn non_table_return_is_error() {
        assert!(parse_manifest("return 42", Path::new("bad.lua")).is_err());
    }

    #[test]
    fn missing_identity_is_error() {
        let src = r#"return { match = { transport = "hid", vid = 1 } }"#;
        assert!(parse_manifest(src, Path::new("bad.lua")).is_err());
    }

    #[test]
    fn unknown_transport_kind_rejected() {
        let src = r#"return {
            match = { transport = "carrier_pigeon", vid = 1 },
            identity = { vendor = "x", model = "y" },
        }"#;
        assert!(parse_manifest(src, Path::new("bad.lua")).is_err());
    }

    #[test]
    fn smbus_requires_bus_and_addresses() {
        // Missing bus/addresses is rejected by the smbus backend's validate.
        let src = r#"return {
            match = { transport = "smbus" },
            identity = { vendor = "x", model = "y" },
        }"#;
        assert!(parse_manifest(src, Path::new("bad.lua")).is_err());
    }

    #[test]
    fn array_of_match_specs_parses() {
        let src = r#"return {
            match = {
              { transport = "smbus", bus = "chipset", addresses = { 0x70, 0x71 },
                device_type = "ram", name = "DRAM" },
              { transport = "smbus", bus = "gpu", addresses = { 0x67 },
                device_type = "gpu",
                pci_match = { { vendor = 0x10DE, sub_vendor = 0x1043, confirmed = true } } },
            },
            identity = { vendor = "ENE", model = "SMBus" },
          }"#;
        let m = parse_manifest(src, Path::new("ene.lua")).unwrap();
        assert_eq!(m.match_specs.len(), 2);
        assert_eq!(m.smbus_specs().count(), 2);
        assert_eq!(m.match_specs[0].device_type, Some(DeviceType::Ram));
    }

    #[test]
    fn device_type_aio_serializes_as_a_i_o_not_aio() {
        // Regression guard: serde's snake_case rename inserts an underscore
        // before every uppercase letter (not just word boundaries), so the
        // all-caps `AIO` variant becomes "a_i_o", not the more intuitive
        // "aio" — a real footgun for plugin authors declaring `device_type`.
        let src = r#"return {
            match = { transport = "hid", vid = 1, pid = 2, device_type = "a_i_o" },
            identity = { vendor = "x", model = "y" },
        }"#;
        let m = parse_manifest(src, Path::new("k.lua")).unwrap();
        assert_eq!(m.match_specs[0].device_type, Some(DeviceType::AIO));
    }

    #[test]
    fn gpu_spec_without_pci_match_is_rejected() {
        // The GPU I²C bus is shared with the display; a gate is mandatory.
        let src = r#"return {
            match = { transport = "smbus", bus = "gpu", addresses = { 0x67 } },
            identity = { vendor = "x", model = "y" },
        }"#;
        assert!(parse_manifest(src, Path::new("bad.lua")).is_err());
    }

    #[test]
    fn gpu_spec_with_pci_match_parses_and_round_trips() {
        let src = r#"return {
            match = { transport = "smbus", bus = "gpu", addresses = { 0x67 },
              pci_match = {
                { vendor = 0x10DE, device = 0x2684, sub_vendor = 0x1043,
                  sub_device = 0x88BF, confirmed = true },
              } },
            identity = { vendor = "ENE", model = "GPU" },
        }"#;
        let m = parse_manifest(src, Path::new("ene.lua")).unwrap();
        let gate = &m.match_specs[0].pci_match;
        assert_eq!(gate.len(), 1);
        assert_eq!(gate[0].vendor, Some(0x10DE));
        assert_eq!(gate[0].sub_device, Some(0x88BF));
        assert!(gate[0].confirmed);
    }

    #[test]
    fn chipset_spec_without_pci_match_still_parses() {
        let src = r#"return {
            match = { transport = "smbus", bus = "chipset", addresses = { 0x70 } },
            identity = { vendor = "x", model = "y" },
        }"#;
        assert!(parse_manifest(src, Path::new("ok.lua")).is_ok());
    }

    #[test]
    fn range_boolean_action_battery_connection_equalizer_sections_parse() {
        let src = r#"return {
            match = { transport = "hid", vid = 1, pid = 2 },
            identity = { vendor = "x", model = "y" },
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
    fn range_default_step_is_one() {
        let src = r#"return {
            match = { transport = "hid", vid = 1, pid = 2 },
            identity = { vendor = "x", model = "y" },
            range = { ranges = { { key = "hz", label = "Hz", min = 0, max = 10, default = 5 } } },
        }"#;
        let m = parse_manifest(src, Path::new("r.lua")).unwrap();
        assert_eq!(m.range.unwrap().ranges[0].step, 1);
    }

    #[test]
    fn pairing_onboard_profiles_key_remap_sections_parse() {
        let src = r#"return {
            match = { transport = "hid", vid = 1, pid = 2 },
            identity = { vendor = "x", model = "y" },
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
            match = { transport = "hid", vid = 1, pid = 2 },
            identity = { vendor = "x", model = "y" },
            permissions = { "network", "os" },
        }"#;
        let m = parse_manifest(src, Path::new("net.lua")).unwrap();
        assert_eq!(m.permissions, vec![Permission::Network, Permission::Os]);

        let no_perms = r#"return {
            match = { transport = "hid", vid = 1, pid = 2 },
            identity = { vendor = "x", model = "y" },
        }"#;
        let m = parse_manifest(no_perms, Path::new("no_perms.lua")).unwrap();
        assert!(m.permissions.is_empty());
    }

    #[test]
    fn effect_only_plugin_needs_no_match_spec() {
        let src = r#"return {
            identity = { vendor = "x", model = "Effects" },
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
        assert!(m.match_specs.is_empty());
        assert_eq!(m.plugin_type, PluginType::Effect);
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
    fn plugin_with_neither_match_nor_effects_is_rejected() {
        let src = r#"return {
            identity = { vendor = "x", model = "y" },
        }"#;
        assert!(parse_manifest(src, Path::new("bad.lua")).is_err());
    }

    #[test]
    fn effect_with_empty_id_is_rejected() {
        let src = r#"return {
            identity = { vendor = "x", model = "y" },
            effects = { { kind = "pixmap", id = "", name = "Nope" } },
        }"#;
        assert!(parse_manifest(src, Path::new("bad2.lua")).is_err());
    }

    #[test]
    fn plugin_type_defaults_to_device() {
        let m = parse_manifest(SAMPLE, Path::new("acme_k1.lua")).unwrap();
        assert_eq!(m.plugin_type, PluginType::Device);
    }

    #[test]
    fn device_plugin_can_also_bundle_effects() {
        let src = r#"return {
            match = { transport = "hid", vid = 1, pid = 2 },
            identity = { vendor = "x", model = "y" },
            effects = { { kind = "direct", id = "pulse", name = "Pulse" } },
        }"#;
        let m = parse_manifest(src, Path::new("bundled.lua")).unwrap();
        assert_eq!(m.match_specs.len(), 1);
        assert_eq!(m.effects.len(), 1);
    }
}
