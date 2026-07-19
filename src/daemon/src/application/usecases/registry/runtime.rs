// SPDX-License-Identifier: GPL-3.0-or-later
//! Application boundary for discovery and registry lifecycle observations.

use crate::domain::events::ChangeSink as _;

use std::sync::Arc;

use crate::application::state::AppState;

pub async fn bootstrap(app: &Arc<AppState>) {
    // Publish the persisted GUI language before the full bootstrap projection.
    // The latter snapshots devices and plugin state and can take long enough for
    // the startup radar to render in the fallback locale in the meantime.
    app.record_change(crate::domain::events::Change::Gui).await;
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
