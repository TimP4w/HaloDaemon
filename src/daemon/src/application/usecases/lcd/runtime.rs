// SPDX-License-Identifier: GPL-3.0-or-later
//! Application boundary for background LCD-engine state transitions.

use crate::domain::events::ChangeSink as _;

use std::sync::Arc;

use crate::application::state::AppState;

pub async fn device_changed(app: &Arc<AppState>, device_id: &str) {
    app.record_change(crate::domain::events::Change::LcdDevice(
        device_id.to_owned(),
    ))
    .await;
}
