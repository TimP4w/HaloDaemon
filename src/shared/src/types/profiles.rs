// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

pub const DEFAULT_PROFILE_NAME: &str = "default";

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
    pub device_ids: Vec<crate::ids::DeviceId>,
    #[serde(default)]
    pub channels: HashMap<crate::ids::DeviceId, Vec<String>>,
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
    pub device_capabilities: std::collections::BTreeMap<
        crate::ids::DeviceId,
        std::collections::BTreeMap<String, OverrideDiff>,
    >,
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
