// SPDX-License-Identifier: GPL-3.0-or-later
//! Application boundary for discovery and registry lifecycle observations.

use crate::domain::events::ChangeSink as _;

use std::sync::Arc;

use crate::application::state::AppState;

/// Seed the persisted GUI config before IPC starts accepting clients, ensuring
/// the very first retained snapshot contains authoritative locale state.
pub async fn publish_gui(app: &Arc<AppState>) {
    app.record_change(crate::domain::events::Change::Gui).await;
}

pub async fn bootstrap(app: &Arc<AppState>) {
    app.record_change(crate::domain::events::Change::Bootstrap)
        .await;
}

pub async fn topology_changed(app: &Arc<AppState>) {
    app.record_change(crate::domain::events::Change::DiscoveryTopology)
        .await;
}

#[cfg(test)]
pub async fn gui_changed(app: &Arc<AppState>) {
    app.record_change(crate::domain::events::Change::Gui).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use halod_shared::bus::BusValue;

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
