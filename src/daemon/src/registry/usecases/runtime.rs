// SPDX-License-Identifier: GPL-3.0-or-later
//! Application boundary for discovery and registry lifecycle observations.

use std::sync::Arc;

use crate::state::AppState;

pub async fn bootstrap(app: &Arc<AppState>) {
    app.record_change(crate::services::effective_state::Change::Bootstrap)
        .await;
}

pub async fn topology_changed(app: &Arc<AppState>) {
    app.record_change(crate::services::effective_state::Change::DiscoveryTopology)
        .await;
}

#[cfg(test)]
pub async fn gui_changed(app: &Arc<AppState>) {
    app.record_change(crate::services::effective_state::Change::Gui)
        .await;
}
