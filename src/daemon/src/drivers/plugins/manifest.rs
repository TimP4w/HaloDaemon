// SPDX-License-Identifier: GPL-3.0-or-later
//! Parsing a plugin script's `return`ed table into a `PluginManifest`.
//!
//! Parsing runs the script once in a throwaway Lua VM purely to read the
//! declarative `match`/`identity` tables; that VM is dropped immediately. The
//! per-device worker later builds its own VM from `script_source` when a device
//! actually matches.

use anyhow::{anyhow, bail, Result};
use halod_shared::types::{NativeEffect, RgbDescriptor, RgbZone};
use mlua::{DeserializeOptions, Lua, LuaSerdeExt};
use serde::Deserialize;
use std::path::Path;

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

/// Declarative device match — compiled to the `DeviceDescriptor::matches`
/// predicate shape. `None` fields mean "don't care".
#[derive(Debug, Clone, Deserialize)]
pub struct MatchSpec {
    pub transport: String,
    pub vid: u16,
    #[serde(default)]
    pub pid: Option<u16>,
    #[serde(default)]
    pub usage_page: Option<u16>,
    #[serde(default)]
    pub usage: Option<u16>,
    #[serde(default)]
    pub interface: Option<i32>,
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
    match_spec: MatchSpec,
    identity: Identity,
    #[serde(default)]
    transports: TransportsConfig,
    #[serde(default)]
    rgb: Option<RgbManifest>,
    #[serde(default)]
    fan: Option<FanManifest>,
    #[serde(default)]
    sensor: Option<SensorManifest>,
}

/// A parsed, validated plugin ready to be matched against discovery handles.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    /// Unique per plugin (the script file stem).
    pub plugin_id: String,
    pub source_path: std::path::PathBuf,
    /// Full script text, re-executed by the worker to build its own VM.
    pub script_source: String,
    pub match_spec: MatchSpec,
    pub identity: Identity,
    pub transports: TransportsConfig,
    pub rgb: Option<RgbManifest>,
    pub fan: Option<FanManifest>,
    pub sensor: Option<SensorManifest>,
}

impl MatchSpec {
    /// Does this spec accept the handle a bus scanner produced?
    pub fn matches(&self, handle: &DiscoveryHandle<'_>) -> bool {
        match (self.transport.as_str(), handle) {
            (
                "hid",
                DiscoveryHandle::Hid {
                    vid,
                    pid,
                    usage_page,
                    usage,
                    interface_number,
                    ..
                },
            ) => {
                *vid == self.vid
                    && self.pid.is_none_or(|p| p == *pid)
                    && self.usage_page.is_none_or(|u| u == *usage_page)
                    && self.usage.is_none_or(|u| u == *usage)
                    && self.interface.is_none_or(|i| Some(i) == *interface_number)
            }
            _ => false,
        }
    }
}

impl PluginManifest {
    /// Stable id prefix a matched device's id is built from.
    pub fn id_prefix(&self) -> &str {
        self.identity.id.as_deref().unwrap_or(&self.plugin_id)
    }

    /// Human-readable device name.
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
    /// + worker (RGB / fan / sensor). Device-only plugins skip the worker.
    pub fn needs_worker(&self) -> bool {
        self.rgb.is_some() || self.fan.is_some() || self.sensor.is_some()
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

    // v1 supports HID only; other transports land in later steps.
    if raw.match_spec.transport != "hid" {
        bail!(
            "unsupported match transport '{}' (only 'hid' in v1)",
            raw.match_spec.transport
        );
    }

    Ok(PluginManifest {
        plugin_id,
        source_path: path.to_path_buf(),
        script_source: source.to_owned(),
        match_spec: raw.match_spec,
        identity: raw.identity,
        transports: raw.transports,
        rgb: raw.rgb,
        fan: raw.fan,
        sensor: raw.sensor,
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
        assert_eq!(m.match_spec.vid, 0x1234);
        assert_eq!(m.match_spec.pid, Some(0x5678));
        assert_eq!(m.id_prefix(), "acme_k1");
    }

    #[test]
    fn match_predicate_respects_wildcards_and_specifics() {
        let m = parse_manifest(SAMPLE, Path::new("acme_k1.lua")).unwrap();
        assert!(m.match_spec.matches(&hid(0x1234, 0x5678, None)));
        assert!(
            !m.match_spec.matches(&hid(0x1234, 0x9999, None)),
            "pid differs"
        );
        assert!(
            !m.match_spec.matches(&hid(0x9999, 0x5678, None)),
            "vid differs"
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
    fn non_hid_transport_rejected() {
        let src = r#"return {
            match = { transport = "smbus", vid = 1 },
            identity = { vendor = "x", model = "y" },
        }"#;
        assert!(parse_manifest(src, Path::new("bad.lua")).is_err());
    }
}
