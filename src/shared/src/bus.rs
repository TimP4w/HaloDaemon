// SPDX-License-Identifier: GPL-3.0-or-later
//! Typed state-bus wire contract shared by the daemon and GUI.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::types::{
    CoolingState, DiscoveryStatus, GuiConfig, HealthCheckState, LcdState, LightingOverviewState,
    Notification, PluginsState, ProfileState, WireDevice,
};

pub const EVENT_RING_CAPACITY: usize = 256;

pub mod topic {
    pub const DISCOVERY: &str = "runtime.discovery";
    pub const PROFILES: &str = "effective.profiles";
    pub const COOLING: &str = "effective.cooling";
    pub const LIGHTING: &str = "effective.lighting";
    pub const LCD: &str = "effective.lcd";
    pub const GUI: &str = "config.gui";
    pub const HEALTH: &str = "runtime.health";
    pub const PROCESS_ICONS: &str = "runtime.process_icons";
    pub const PLUGINS: &str = "runtime.plugins";
    pub const CONFIG_DIR: &str = "runtime.config_dir";
    pub const DEVICE_PREFIX: &str = "runtime.devices.";

    pub fn device(id: &str) -> String {
        format!("{DEVICE_PREFIX}{id}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum BusValue {
    Discovery(DiscoveryStatus),
    Device(WireDevice),
    Profiles(ProfileState),
    Cooling(CoolingState),
    Lighting(LightingOverviewState),
    Lcd(LcdState),
    Gui(GuiConfig),
    Health(HealthCheckState),
    ProcessIcons(HashMap<String, String>),
    Plugins(PluginsState),
    ConfigDir(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusRecordStatus {
    Fresh,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusRecord {
    pub key: String,
    pub value: BusValue,
    pub status: BusRecordStatus,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusTransaction {
    pub revision: u64,
    pub upserts: Vec<BusRecord>,
    pub tombstones: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusSnapshot {
    pub revision: u64,
    pub records: Vec<BusRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum BusEventPayload {
    Notification(Notification),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusEvent {
    pub id: u64,
    pub payload: BusEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusSubscribe {
    #[serde(default)]
    pub prefixes: Vec<String>,
    #[serde(default)]
    pub last_event_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusEventReplay {
    pub oldest_available_id: Option<u64>,
    pub events: Vec<BusEvent>,
}

pub fn matches_prefixes(key: &str, prefixes: &[String]) -> bool {
    prefixes.is_empty() || prefixes.iter().any(|prefix| key.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_subscription_matches_every_topic() {
        assert!(matches_prefixes(topic::GUI, &[]));
    }

    #[test]
    fn subscription_matches_by_prefix() {
        assert!(matches_prefixes(
            "runtime.devices.keyboard",
            &["runtime.devices.".into()]
        ));
        assert!(!matches_prefixes(
            "effective.cooling",
            &["runtime.devices.".into()]
        ));
    }
}
