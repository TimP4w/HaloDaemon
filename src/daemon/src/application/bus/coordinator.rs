// SPDX-License-Identifier: GPL-3.0-or-later
//! Coordinates authoritative effective/observed state commits.

use std::sync::Arc;
use tokio::sync::Mutex;

use super::projections::{self, Topic};
use crate::application::state::AppState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Bootstrap,
    DiscoveryTopology,
    Device(String),
    SensorTelemetry(Vec<String>),
    Lighting,
    LightingDevice(String),
    LightingCatalog,
    LightingTopology,
    Canvas,
    CanvasDevice(String),
    Cooling,
    CoolingDevice(String),
    Lcd,
    LcdDevice(String),
    LcdCatalog,
    Gui,
    Profiles,
    AppRules,
    ProfileSwitch,
    PluginTopology,
    PluginData,
    PluginDeviceStatus(String),
}

impl Change {
    fn topics(&self) -> Vec<Topic> {
        match self {
            Self::Bootstrap => projections::ALL_TOPICS.to_vec(),
            Self::DiscoveryTopology => vec![
                Topic::Discovery,
                Topic::Devices,
                Topic::Cooling,
                Topic::Lighting,
                Topic::Lcd,
                Topic::Plugins,
            ],
            Self::Device(id) => vec![Topic::Device(id.clone())],
            Self::SensorTelemetry(ids) => ids.iter().cloned().map(Topic::Device).collect(),
            Self::Lighting => vec![Topic::Lighting],
            Self::LightingDevice(id) => vec![Topic::Lighting, Topic::Device(id.clone())],
            Self::LightingCatalog | Self::Canvas => vec![Topic::Lighting],
            Self::LightingTopology => vec![Topic::Lighting, Topic::Devices],
            Self::CanvasDevice(id) => vec![Topic::Lighting, Topic::Device(id.clone())],
            Self::Cooling => vec![Topic::Cooling],
            Self::CoolingDevice(id) => vec![Topic::Cooling, Topic::Device(id.clone())],
            Self::Lcd => vec![Topic::Lcd],
            Self::LcdDevice(id) => vec![Topic::Lcd, Topic::Device(id.clone())],
            Self::LcdCatalog => vec![Topic::Lcd],
            Self::Gui => vec![Topic::Gui],
            Self::Profiles => vec![Topic::Profiles],
            Self::AppRules => vec![Topic::Profiles, Topic::ProcessIcons],
            Self::ProfileSwitch => vec![
                Topic::Profiles,
                Topic::Devices,
                Topic::Cooling,
                Topic::Lighting,
                Topic::Lcd,
            ],
            Self::PluginTopology => vec![Topic::Plugins, Topic::Devices],
            Self::PluginData => vec![Topic::Plugins],
            Self::PluginDeviceStatus(id) => vec![Topic::Plugins, Topic::Device(id.clone())],
        }
    }
}

#[derive(Default)]
pub struct EffectiveStatePublisher {
    commit_lock: Mutex<()>,
}

impl EffectiveStatePublisher {
    pub async fn record(&self, app: &Arc<AppState>, change: Change) {
        self.commit(app, &change.topics()).await;
    }

    async fn commit(&self, app: &Arc<AppState>, topics: &[Topic]) {
        let _commit = self.commit_lock.lock().await;
        let cfg = app.config.read().await.clone();
        let upserts = projections::produce(app, &cfg, topics).await;
        let tombstones = if topics.contains(&Topic::Devices) {
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
        if let Err(error) = app.data_bus.commit_state("host.state", upserts, tombstones) {
            log::error!("failed to publish state transaction: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, infrastructure::drivers::Device, test_support::MockDevice};

    #[test]
    fn semantic_changes_own_their_topic_dependency_graph() {
        assert_eq!(
            Change::LightingDevice("kbd".into()).topics(),
            vec![Topic::Lighting, Topic::Device("kbd".into())]
        );
        assert_eq!(
            Change::AppRules.topics(),
            vec![Topic::Profiles, Topic::ProcessIcons]
        );
        assert_eq!(Change::Gui.topics(), vec![Topic::Gui]);
        assert_eq!(Change::LightingCatalog.topics(), vec![Topic::Lighting]);
        assert_eq!(Change::LcdCatalog.topics(), vec![Topic::Lcd]);
        assert_eq!(
            Change::CanvasDevice("kbd".into()).topics(),
            vec![Topic::Lighting, Topic::Device("kbd".into())]
        );
    }

    #[tokio::test]
    async fn device_change_does_not_upsert_unrelated_topics() {
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .write()
            .await
            .push(Arc::new(MockDevice::new("mouse")) as Arc<dyn Device>);
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
}
