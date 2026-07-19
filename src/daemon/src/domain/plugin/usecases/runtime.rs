// SPDX-License-Identifier: GPL-3.0-or-later
//! Application boundary for plugin runtime and monitor observations.

use std::sync::Arc;

use crate::application::state::AppState;

pub async fn topology_changed(app: &Arc<AppState>) {
    app.record_change(crate::application::bus::coordinator::Change::PluginTopology)
        .await;
}

pub async fn data_changed(app: &Arc<AppState>) {
    app.record_change(crate::application::bus::coordinator::Change::PluginData)
        .await;
}

pub async fn device_changed(app: &Arc<AppState>, device_id: &str) {
    let mut ids = vec![device_id.to_owned()];
    ids.extend(
        app.device_registry
            .read()
            .await
            .iter()
            .filter(|device| device.state_source_id() == Some(device_id))
            .map(|device| device.id().to_owned()),
    );
    app.record_change(crate::application::bus::coordinator::Change::Devices(ids))
        .await;
}

pub async fn device_status_changed(app: &Arc<AppState>, device_id: &str) {
    app.record_change(
        crate::application::bus::coordinator::Change::PluginDeviceStatus(device_id.to_owned()),
    )
    .await;
}
