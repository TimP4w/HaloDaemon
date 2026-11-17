// SPDX-License-Identifier: GPL-3.0-or-later
//! Application boundary for background LCD-engine state transitions.

use crate::domain::events::ChangeSink as _;

use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::events::Change;
use halod_shared::ids::DeviceId;

pub async fn device_changed(app: &Arc<AppState>, device_id: &DeviceId) {
    app.record_change(Change::LcdDevice(device_id.into())).await;
}
