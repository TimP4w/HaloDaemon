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
    /// An existing device adopted this HID key's transport; reverts (not removed)
    /// when the key disappears.
    WiredOverride(Arc<dyn Device>),
}

impl Clone for HidTrackingEntry {
    fn clone(&self) -> Self {
        match self {
            Self::Primary(arcs) => Self::Primary(arcs.clone()),
            Self::WiredOverride(d) => Self::WiredOverride(Arc::clone(d)),
        }
    }
}

/// Derives the set of device ids reachable over HID from a tracking map.
fn compute_hid_ids(tracking: &HashMap<String, HidTrackingEntry>) -> HashSet<String> {
    let mut ids = HashSet::new();
    for entry in tracking.values() {
        match entry {
            HidTrackingEntry::Primary(arcs) => ids.extend(arcs.iter().map(|d| d.id().to_owned())),
            HidTrackingEntry::WiredOverride(d) => {
                ids.insert(d.id().to_owned());
            }
        }
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

    /// Clear all tracked HID entries and the cached id set.
    pub async fn clear(&self) {
        self.tracking.lock().await.clear();
        *self.ids.write().await = HashSet::new();
    }

    /// A clone of one tracked entry, if present.
    pub async fn get(&self, key: &str) -> Option<HidTrackingEntry> {
        self.tracking.lock().await.get(key).cloned()
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
    /// Re-add a device to the registry if it is not already present (by pointer equality).
    /// Returns `true` when the device was inserted. Used by reconnect paths that need to
    /// restore a device to `app.devices` without going through the full registration lifecycle.
    pub async fn add_device_if_absent(&self, device: Arc<dyn Device>) -> bool {
        let mut devs = self.devices.write().await;
        if devs.iter().any(|d| Arc::ptr_eq(d, &device)) {
            return false;
        }
        devs.push(device);
        true
    }

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
