// SPDX-License-Identifier: GPL-3.0-or-later
//! Coordinates authoritative effective/observed state commits.

#[cfg(test)]
use std::sync::Arc;
use tokio::sync::Mutex;

use super::projections::{self, Topic};
use crate::application::state::AppState;
use crate::domain::events::Change;

fn topics(change: &Change) -> Vec<Topic> {
    match change {
        Change::Bootstrap => projections::ALL_TOPICS.to_vec(),
        Change::DiscoveryTopology => vec![
            Topic::Discovery,
            Topic::Devices,
            Topic::Cooling,
            Topic::Lighting,
            Topic::Lcd,
            Topic::Plugins,
        ],
        // Persisted capability state is stored in the active profile. Some
        // device changes are observational only, but the unchanged-record
        // filter makes projecting Profiles cheap in that case and, crucially,
        // keeps the override badges in sync after a persisted mutation.
        Change::Device(id) => vec![Topic::Device(id.clone()), Topic::Profiles],
        Change::Devices(ids) => ids
            .iter()
            .cloned()
            .map(Topic::Device)
            .chain(std::iter::once(Topic::Profiles))
            .collect(),
        Change::SensorTelemetry(ids) => ids.iter().cloned().map(Topic::Device).collect(),
        Change::Lighting => vec![Topic::Lighting, Topic::Profiles],
        Change::LightingDevice(id) => {
            vec![Topic::Lighting, Topic::Device(id.clone()), Topic::Profiles]
        }
        Change::LightingCatalog => vec![Topic::Lighting],
        Change::Canvas => vec![Topic::Lighting, Topic::Profiles],
        Change::LightingTopology => vec![Topic::Lighting, Topic::Devices],
        Change::CanvasDevice(id) => {
            vec![Topic::Lighting, Topic::Device(id.clone()), Topic::Profiles]
        }
        Change::Cooling => vec![Topic::Cooling],
        Change::CoolingDevice(id) => {
            vec![Topic::Cooling, Topic::Device(id.clone()), Topic::Profiles]
        }
        Change::Lcd => vec![Topic::Lcd],
        Change::LcdDevice(id) => vec![Topic::Lcd, Topic::Device(id.clone()), Topic::Profiles],
        Change::LcdCatalog => vec![Topic::Lcd],
        Change::Gui => vec![Topic::Gui],
        Change::Profiles => vec![Topic::Profiles],
        Change::AppRules => vec![Topic::Profiles, Topic::ProcessIcons],
        Change::ProfileSwitch => vec![
            Topic::Profiles,
            Topic::Devices,
            Topic::Cooling,
            Topic::Lighting,
            Topic::Lcd,
        ],
        Change::PluginTopology => vec![Topic::Plugins, Topic::Devices],
        Change::PluginData => vec![Topic::Plugins],
        Change::PluginDeviceStatus(id) => vec![Topic::Plugins, Topic::Device(id.clone())],
    }
}

#[derive(Default)]
pub struct EffectiveStatePublisher {
    commit_lock: Mutex<()>,
}

impl EffectiveStatePublisher {
    pub async fn record(&self, app: &AppState, change: Change) {
        self.commit(app, &topics(&change)).await;
    }

    async fn commit(&self, app: &AppState, topics: &[Topic]) {
        let _commit = self.commit_lock.lock().await;
        let cfg = app.config.read().await.clone();
        let mut upserts = projections::produce(app, &cfg, topics).await;
        let mut tombstones = if topics.contains(&Topic::Devices) {
            let new_keys: std::collections::HashSet<_> =
                upserts.iter().map(|(key, _)| key.clone()).collect();
            app.data_bus
                .state_snapshot(&[halod_shared::bus::topic::DEVICE_PREFIX.into()])
                .records
                .into_iter()
                .map(|record| record.key)
                .filter(|key| !new_keys.contains(key))
                .collect()
        } else {
            Vec::new()
        };
        // Producers may deliberately check volatile projections on a cadence.
        // Keep that cheap on the wire: only a materially changed record creates
        // a revision and wakes IPC/GUI subscribers.
        let current = app.data_bus.state_values(
            upserts
                .iter()
                .map(|(key, _)| key.as_str())
                .chain(tombstones.iter().map(String::as_str)),
        );
        upserts.retain(|(key, value)| {
            current
                .get(key)
                .is_none_or(|old| !same_bus_value(old, value))
        });
        tombstones.retain(|key| current.contains_key(key));
        if upserts.is_empty() && tombstones.is_empty() {
            return;
        }
        if let Err(error) = app.data_bus.commit_state("host.state", upserts, tombstones) {
            log::error!("failed to publish state transaction: {error:#}");
        }
    }
}

fn same_bus_value(left: &halod_shared::bus::BusValue, right: &halod_shared::bus::BusValue) -> bool {
    match (serde_json::to_value(left), serde_json::to_value(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, domain::device::Device, test_support::MockDevice};

    #[test]
    fn semantic_changes_own_their_topic_dependency_graph() {
        assert_eq!(
            topics(&Change::LightingDevice("kbd".into())),
            vec![
                Topic::Lighting,
                Topic::Device("kbd".into()),
                Topic::Profiles
            ]
        );
        assert_eq!(
            topics(&Change::AppRules),
            vec![Topic::Profiles, Topic::ProcessIcons]
        );
        assert_eq!(topics(&Change::Gui), vec![Topic::Gui]);
        assert_eq!(topics(&Change::LightingCatalog), vec![Topic::Lighting]);
        assert_eq!(topics(&Change::LcdCatalog), vec![Topic::Lcd]);
        assert_eq!(
            topics(&Change::CanvasDevice("kbd".into())),
            vec![
                Topic::Lighting,
                Topic::Device("kbd".into()),
                Topic::Profiles
            ]
        );
        assert_eq!(
            topics(&Change::Device("mouse".into())),
            vec![Topic::Device("mouse".into()), Topic::Profiles]
        );
        assert_eq!(
            topics(&Change::Canvas),
            vec![Topic::Lighting, Topic::Profiles]
        );
    }

    #[test]
    fn persisted_device_state_changes_refresh_profile_overrides() {
        let changes = [
            Change::Device("mouse".into()),
            Change::Devices(vec!["mouse".into(), "keyboard".into()]),
            Change::Lighting,
            Change::LightingDevice("keyboard".into()),
            Change::Canvas,
            Change::CanvasDevice("keyboard".into()),
            Change::CoolingDevice("fan".into()),
            Change::LcdDevice("screen".into()),
        ];

        for change in changes {
            assert!(
                topics(&change).contains(&Topic::Profiles),
                "{change:?} must refresh the profile projection"
            );
        }
    }

    #[tokio::test]
    async fn device_change_does_not_upsert_unrelated_topics() {
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .write()
            .await
            .push(Arc::new(MockDevice::new("mouse")) as Arc<dyn Device>);
        // Bootstrap normally seeds this retained projection. A later
        // observational device change may request it for override tracking,
        // but must not publish it when it is unchanged.
        app.effective_state.record(&app, Change::Profiles).await;
        let mut transactions = app.data_bus.subscribe_transactions();

        app.effective_state
            .record(&app, Change::Device("mouse".into()))
            .await;

        let transaction = transactions.recv().await.unwrap();
        assert_eq!(transaction.upserts.len(), 1);
        assert_eq!(
            transaction.upserts[0].key,
            halod_shared::bus::topic::device("mouse")
        );
        assert!(transaction.tombstones.is_empty());
    }

    #[tokio::test]
    async fn unchanged_projection_does_not_emit_a_transaction() {
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .write()
            .await
            .push(Arc::new(MockDevice::new("mouse")) as Arc<dyn Device>);
        app.effective_state
            .record(&app, Change::Device("mouse".into()))
            .await;
        let mut transactions = app.data_bus.subscribe_transactions();

        app.effective_state
            .record(&app, Change::Device("mouse".into()))
            .await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), transactions.recv())
                .await
                .is_err()
        );
    }
}
