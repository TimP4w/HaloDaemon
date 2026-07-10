// SPDX-License-Identifier: GPL-3.0-or-later
//! Parsing a plugin script's `return`ed table into a `PluginManifest`.
//!
//! Parsing runs the script once in a throwaway Lua VM purely to read the
//! declarative `match`/`identity` tables; that VM is dropped immediately. The
//! per-device worker later builds its own VM from `script_source` when a device
//! actually matches.

use anyhow::{anyhow, bail, Result};
use halod_shared::types::{DeviceType, NativeEffect, RgbDescriptor, RgbZone, ZoneTopology};
use mlua::{DeserializeOptions, Lua, LuaSerdeExt};
use serde::Deserialize;
use std::path::Path;

use super::transport::{descriptor_for, known_kinds};
use crate::drivers::transports::smbus::SmbusBusKind;
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
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    #[serde(rename = "match")]
    match_spec: MatchSpecs,
    identity: Identity,
    #[serde(default)]
    transports: TransportsConfig,
    #[serde(default)]
    rgb: Option<RgbManifest>,
    #[serde(default)]
    fan: Option<FanManifest>,
    #[serde(default)]
    sensor: Option<SensorManifest>,
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
    pub transports: TransportsConfig,
    pub rgb: Option<RgbManifest>,
    pub fan: Option<FanManifest>,
    pub sensor: Option<SensorManifest>,
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
        self.rgb.is_some() || self.fan.is_some() || self.sensor.is_some() || self.chain.is_some()
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

    let match_specs = raw.match_spec.into_vec();
    if match_specs.is_empty() {
        bail!("plugin declares no match spec");
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

    Ok(PluginManifest {
        plugin_id,
        source_path: path.to_path_buf(),
        script_source: source.to_owned(),
        match_specs,
        identity: raw.identity,
        transports: raw.transports,
        rgb: raw.rgb,
        fan: raw.fan,
        sensor: raw.sensor,
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
                device_type = "gpu" },
            },
            identity = { vendor = "ENE", model = "SMBus" },
          }"#;
        let m = parse_manifest(src, Path::new("ene.lua")).unwrap();
        assert_eq!(m.match_specs.len(), 2);
        assert_eq!(m.smbus_specs().count(), 2);
        assert_eq!(m.match_specs[0].device_type, Some(DeviceType::Ram));
    }
}
