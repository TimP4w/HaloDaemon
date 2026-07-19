// SPDX-License-Identifier: GPL-3.0-or-later
//! Application boundary for observed cooling-engine state changes.

use std::sync::Arc;

use crate::application::state::AppState;

pub async fn status_changed(app: &Arc<AppState>) {
    app.record_change(crate::application::bus::coordinator::Change::Cooling)
        .await;
}

/// Record a duty command only after the device accepted it, then publish the
/// affected device projection immediately instead of waiting for telemetry.
pub async fn duty_applied(app: &Arc<AppState>, device_id: &str, channel_id: &str, duty: u8) {
    if app
        .cooling
        .record_applied_duty(device_id, channel_id, duty)
        .await
    {
        app.record_change(crate::application::bus::coordinator::Change::Device(
            device_id.to_owned(),
        ))
        .await;
    }
}
