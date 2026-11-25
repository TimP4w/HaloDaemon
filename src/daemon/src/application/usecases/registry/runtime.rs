// SPDX-License-Identifier: GPL-3.0-or-later
//! Application boundary for discovery and registry lifecycle observations.

use crate::domain::events::ChangeSink as _;

use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::events::Change;

/// Seed the persisted GUI config before IPC starts accepting clients, ensuring
/// the very first retained snapshot contains authoritative locale state.
pub async fn publish_gui(app: &Arc<AppState>) {
    app.record_change(Change::Gui).await;
}

pub async fn bootstrap(app: &Arc<AppState>) {
    app.record_change(Change::Bootstrap).await;
}

pub async fn topology_changed(app: &Arc<AppState>) {
    app.record_change(Change::DiscoveryTopology).await;
}

pub async fn topology_changed_since(app: &Arc<AppState>, revision: u64) {
    if app.device_registry.revision() != revision {
        topology_changed(app).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::test_support::{app_with, MockDevice};
    use halod_shared::bus::BusValue;

    #[tokio::test]
    async fn a_registry_change_publishes_arrivals_and_tombstones_departures() {
        let app = app_with(vec![Arc::new(MockDevice::new("old"))]);
        bootstrap(&app).await;
        let mut transactions = app.data_bus.subscribe_transactions();
        let revision = app.device_registry.revision();

        topology_changed_since(&app, revision).await;
        assert!(transactions.try_recv().is_err());

        {
            let mut devices = app.device_registry.write().await;
            devices.clear();
            devices.push(Arc::new(MockDevice::new("new")));
        }
        topology_changed_since(&app, revision).await;

        let transaction = transactions.recv().await.unwrap();
        assert!(transaction
            .upserts
            .iter()
            .any(|record| record.key == halod_shared::bus::topic::device("new")));
        assert!(transaction
            .tombstones
            .contains(&halod_shared::bus::topic::device("old")));
    }

    #[tokio::test]
    async fn publish_gui_seeds_persisted_language_for_the_first_snapshot() {
        let mut cfg = Config::default();
        cfg.gui.language = "it".into();
        let app = Arc::new(AppState::new(cfg));

        publish_gui(&app).await;

        let snapshot = app.data_bus.state_snapshot(&[]);
        assert!(snapshot
            .records
            .iter()
            .any(|record| { matches!(&record.value, BusValue::Gui(gui) if gui.language == "it") }));
    }
}
