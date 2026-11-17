// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;
use std::sync::OnceLock;

use halod_shared::types::{Battery, Boolean, ConnectionStatus, CoolingChannel, Equalizer, Sensor};
use parking_lot::Mutex;

use super::super::worker::PluginHandle;
use super::StatusPollCaps;

/// The last status the poll sampled. Full IPC serialization reads these
/// instead of queueing callbacks on the worker used by interactive commands.
/// Shared with the poll and event tasks.
#[derive(Default)]
pub(super) struct StatusCache {
    pub(super) cooling_channels: OnceLock<Vec<CoolingChannel>>,
    pub(super) cooling: Mutex<HashMap<String, CoolingChannel>>,
    pub(super) sensors: Mutex<Vec<Sensor>>,
    pub(super) booleans: Mutex<Vec<Boolean>>,
    pub(super) batteries: Mutex<Vec<Battery>>,
    pub(super) connection: Mutex<Option<ConnectionStatus>>,
    pub(super) equalizer: Mutex<Option<Equalizer>>,
}

impl StatusCache {
    /// Sample dynamic status through the worker. A failed read keeps the last
    /// value. Returns whether anything changed.
    pub(super) async fn sample(&self, worker: &PluginHandle, caps: StatusPollCaps) -> bool {
        let mut changed = false;
        if caps.cooling {
            if let Some(channels) = self.cooling_channels.get() {
                let mut observed = HashMap::with_capacity(channels.len());
                for channel in channels {
                    if let Ok(status) = worker.cooling_status(&channel.id).await {
                        observed.insert(channel.id.clone(), status);
                    }
                }
                if !observed.is_empty() {
                    changed |= replace_if_changed(&self.cooling, observed);
                }
            }
        }
        if caps.sensor {
            if let Ok(sensors) = worker.get_sensors().await {
                changed |= replace_if_changed(&self.sensors, sensors);
            }
        }
        if caps.boolean {
            if let Ok(booleans) = worker.boolean_get().await {
                changed |= replace_if_changed(&self.booleans, booleans);
            }
        }
        if caps.battery {
            if let Ok(batteries) = worker.battery_get().await {
                changed |= replace_if_changed(&self.batteries, batteries);
            }
        }
        if caps.connection {
            if let Ok(connection) = worker.connection_get().await {
                changed |= replace_if_changed(&self.connection, connection);
            }
        }
        if caps.equalizer {
            if let Ok(equalizer) = worker.equalizer_get().await {
                changed |= replace_if_changed(&self.equalizer, Some(equalizer));
            }
        }
        changed
    }

    /// Replace the observed cooling channels with ones a poll or event reported.
    pub(super) fn observe_cooling(
        &self,
        channels: impl IntoIterator<Item = CoolingChannel>,
    ) -> bool {
        replace_if_changed(
            &self.cooling,
            channels
                .into_iter()
                .map(|channel| (channel.id.clone(), channel))
                .collect(),
        )
    }

    pub(super) fn observe_sensors(&self, sensors: Vec<Sensor>) -> bool {
        replace_if_changed(&self.sensors, sensors)
    }
}

fn replace_if_changed<T: PartialEq>(cache: &Mutex<T>, next: T) -> bool {
    let mut current = cache.lock();
    let changed = *current != next;
    if changed {
        *current = next;
    }
    changed
}
