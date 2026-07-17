// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use super::AppState;
use crate::drivers::Device;

/// Value stored in a [`HidTracking`] map.
pub enum HidTrackingEntry {
    /// Device(s) created for this HID key; closed and removed when it disappears.
    Primary(Vec<Arc<dyn Device>>),
}

impl Clone for HidTrackingEntry {
    fn clone(&self) -> Self {
        match self {
            Self::Primary(arcs) => Self::Primary(arcs.clone()),
        }
    }
}

/// True when every device an entry tracks is in `ids` — so untracking its key
/// can't strand a still-live device that shares the key. An empty `Primary`
/// (no devices) is never owned.
fn entry_owned_by(entry: &HidTrackingEntry, ids: &HashSet<String>) -> bool {
    match entry {
        HidTrackingEntry::Primary(arcs) => {
            !arcs.is_empty() && arcs.iter().all(|d| ids.contains(d.id()))
        }
    }
}

/// Derives the set of device ids reachable over HID from a tracking map.
fn compute_hid_ids(tracking: &HashMap<String, HidTrackingEntry>) -> HashSet<String> {
    let mut ids = HashSet::new();
    for entry in tracking.values() {
        let HidTrackingEntry::Primary(arcs) = entry;
        ids.extend(arcs.iter().map(|d| d.id().to_owned()));
    }
    ids
}

/// Maps HID key ("vid:pid:serial") → tracking entry, plus a cached set of
/// device ids reachable over HID derived from it. The cache is only ever
/// mutated through `track`/`untrack`, so it can never drift from the map —
/// that invariant used to be a doc-comment convention, now it's enforced by
/// this type having no other way to mutate the map.
pub struct HidTracking {
    tracking: Mutex<HashMap<String, HidTrackingEntry>>,
    /// Read on the broadcast hot path so it never contends with the
    /// discovery loop's lock on the tracking map itself.
    ids: RwLock<HashSet<String>>,
}

impl HidTracking {
    pub fn new() -> Self {
        Self {
            tracking: Mutex::new(HashMap::new()),
            ids: RwLock::new(HashSet::new()),
        }
    }

    /// Insert (or replace) a HID tracking entry and refresh the id cache.
    pub async fn track(&self, key: String, entry: HidTrackingEntry) {
        let ids = {
            let mut tracking = self.tracking.lock().await;
            tracking.insert(key, entry);
            compute_hid_ids(&tracking)
        }; // Mutex guard dropped before the RwLock write to avoid deadlock.
        *self.ids.write().await = ids;
    }

    /// Remove a HID tracking entry, returning it, and refresh the id cache.
    pub async fn untrack(&self, key: &str) -> Option<HidTrackingEntry> {
        let (removed, ids) = {
            let mut tracking = self.tracking.lock().await;
            let removed = tracking.remove(key);
            let ids = removed.as_ref().map(|_| compute_hid_ids(&tracking));
            (removed, ids)
        }; // Mutex guard dropped before the RwLock write to avoid deadlock.
        if let Some(ids) = ids {
            *self.ids.write().await = ids;
        }
        removed
    }

    /// Untrack every HID key whose tracked device id(s) all appear in `ids`, so a
    /// subsequent scoped re-probe re-registers that hardware instead of skipping
    /// it as already-open. Returns the number of keys removed.
    pub async fn untrack_devices(&self, ids: &HashSet<String>) -> usize {
        let keys: Vec<String> = {
            let tracking = self.tracking.lock().await;
            tracking
                .iter()
                .filter(|(_, entry)| entry_owned_by(entry, ids))
                .map(|(k, _)| k.clone())
                .collect()
        };
        for key in &keys {
            self.untrack(key).await;
        }
        keys.len()
    }

    /// All currently-tracked HID keys.
    pub async fn keys(&self) -> HashSet<String> {
        self.tracking.lock().await.keys().cloned().collect()
    }

    /// `(key, entry)` snapshot of the whole tracking map.
    pub async fn snapshot(&self) -> Vec<(String, HidTrackingEntry)> {
        self.tracking
            .lock()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Cached set of device ids reachable over HID, derived from the
    /// tracking map. The cache may lag the map by one tick, the same
    /// trade-off `ipc/serializer.rs` accepts elsewhere.
    pub async fn tracked_ids(&self) -> HashSet<String> {
        self.ids.read().await.clone()
    }
}

impl AppState {
    pub async fn find_device_by_id(&self, id: &str) -> Option<Arc<dyn Device>> {
        self.devices
            .read()
            .await
            .iter()
            .find(|d| d.id() == id)
            .cloned()
    }

    /// Snapshot every sensor across all devices into a `sensor_id -> Sensor` map,
    /// built once per engine tick so per-fan lookups are O(1).
    pub async fn snapshot_sensors(&self) -> HashMap<String, halod_shared::types::Sensor> {
        let known = self.config.read().await.known_devices.clone();
        let devices = self.devices.read().await.clone();
        let mut map = HashMap::new();
        for device in &devices {
            let disabled = known
                .get(device.id())
                .is_some_and(|r| r.active_state == halod_shared::types::VisibilityState::Disabled);
            if disabled {
                continue;
            }
            if let Some(cap) = device.as_sensor_capability() {
                if let Ok(sensors) = cap.get_sensors().await {
                    for s in sensors {
                        map.insert(s.id.clone(), s);
                    }
                }
            }
            for s in crate::drivers::fan_sensors(device.as_ref()).await {
                map.insert(s.id.clone(), s);
            }
        }
        let policy = crate::services::data_bus::host_policy(std::time::Duration::from_secs(3));
        let mut catalog = Vec::with_capacity(map.len());
        let expected: HashSet<String> = map
            .values()
            .map(|sensor| crate::services::data_bus::sensor_key(&sensor.id))
            .collect();
        for (key, snapshot) in self.data_bus.statuses_for_owner("host") {
            if key.starts_with("host.sensors.")
                && key != "host.sensors.catalog"
                && !expected.contains(&key)
                && snapshot.status != crate::services::data_bus::SnapshotStatus::Unavailable
            {
                let _ = self.data_bus.invalidate("host", &key, "sensor_removed");
            }
        }
        for sensor in map.values() {
            let key = crate::services::data_bus::sensor_key(&sensor.id);
            let mut value = std::collections::BTreeMap::new();
            value.insert(
                "id".into(),
                crate::services::data_bus::DataValue::String(sensor.id.clone()),
            );
            value.insert(
                "label".into(),
                crate::services::data_bus::DataValue::String(sensor.name.clone()),
            );
            value.insert(
                "value".into(),
                crate::services::data_bus::DataValue::Number(sensor.value),
            );
            value.insert(
                "unit".into(),
                crate::services::data_bus::DataValue::String(
                    format!("{:?}", sensor.unit).to_lowercase(),
                ),
            );
            value.insert(
                "sensor_type".into(),
                crate::services::data_bus::DataValue::String(
                    format!("{:?}", sensor.sensor_type).to_lowercase(),
                ),
            );
            let _ = self.data_bus.publish(
                "host",
                &key,
                crate::services::data_bus::DataValue::Map(value),
                policy,
            );
            let mut item = std::collections::BTreeMap::new();
            item.insert(
                "id".into(),
                crate::services::data_bus::DataValue::String(sensor.id.clone()),
            );
            item.insert(
                "key".into(),
                crate::services::data_bus::DataValue::String(key),
            );
            catalog.push(crate::services::data_bus::DataValue::Map(item));
        }
        let _ = self.data_bus.publish(
            "host",
            "host.sensors.catalog",
            crate::services::data_bus::DataValue::Array(catalog),
            policy,
        );
        map
    }

    pub async fn get_active_devices(&self) -> Vec<Arc<dyn Device>> {
        let known = self.config.read().await.known_devices.clone();
        self.devices
            .read()
            .await
            .iter()
            .filter(|d| {
                known
                    .get(d.id())
                    .map(|r| r.active_state == halod_shared::types::VisibilityState::Visible)
                    .unwrap_or(true)
            })
            .cloned()
            .collect()
    }
}
