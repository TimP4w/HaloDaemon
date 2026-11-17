// SPDX-License-Identifier: GPL-3.0-or-later
//! Application boundary for plugin runtime and monitor observations.

use crate::domain::events::ChangeSink as _;

use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::events::Change;
use halod_shared::ids::DeviceId;

pub async fn topology_changed(app: &Arc<AppState>) {
    app.record_change(Change::PluginTopology).await;
}

pub async fn data_changed(app: &Arc<AppState>) {
    app.record_change(Change::PluginData).await;
}

pub async fn device_changed(app: &Arc<AppState>, device_id: &DeviceId) {
    let mut ids = vec![DeviceId::from(device_id)];
    ids.extend(
        app.device_registry
            .read()
            .await
            .iter()
            .filter(|device| device.state_source_id() == Some(device_id))
            .map(|device| device.id().to_owned()),
    );
    app.record_change(Change::Devices(ids)).await;
}

pub async fn device_status_changed(app: &Arc<AppState>, device_id: &DeviceId) {
    app.record_change(Change::PluginDeviceStatus(device_id.into()))
        .await;
}
