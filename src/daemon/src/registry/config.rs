// SPDX-License-Identifier: GPL-3.0-or-later
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Metadata recorded the first time a device's state is saved.
/// Persists across disconnects so profiles can reference offline devices.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub name: String,
    pub vendor: String,
    pub model: String,
    #[serde(default)]
    pub active_state: halod_shared::types::VisibilityState,
}

/// Get or create the `DeviceRecord` for `device_id`. When the device is
/// currently registered we seed the record from its `name/vendor/model`; when
/// it isn't (offline / never-seen) we leave a `DeviceRecord::default()`.
pub fn ensure_record<'a>(
    known: &'a mut HashMap<String, DeviceRecord>,
    device_id: &str,
    device: Option<&dyn crate::drivers::Device>,
) -> &'a mut DeviceRecord {
    known
        .entry(device_id.to_string())
        .or_insert_with(|| match device {
            Some(d) => DeviceRecord {
                name: d.name().to_string(),
                vendor: d.vendor().to_string(),
                model: d.model().to_string(),
                active_state: Default::default(),
            },
            None => DeviceRecord::default(),
        })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceLayout {
    #[serde(default)]
    pub channels: HashMap<String, ChannelLayoutRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelLayoutRecord {
    /// Only user-added links. Hardware-detected accessories are re-probed each
    /// boot and never persisted here.
    #[serde(default)]
    pub chain_links: Vec<ChainLinkRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainLinkRecord {
    /// Stable across restarts — canvas placements, transforms, and saved RGB
    /// state are keyed by this child id.
    pub id: String,
    pub kind: String,
    pub name: String,
    pub topology: halod_shared::types::ZoneTopology,
    pub led_count: u32,
}
