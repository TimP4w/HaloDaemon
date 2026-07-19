// SPDX-License-Identifier: GPL-3.0-or-later
//! Application boundary for input-engine changes to observable device state.

use std::sync::Arc;

use crate::state::AppState;

pub async fn device_changed(app: &Arc<AppState>, device_id: &str) {
    app.record_change(crate::services::effective_state::Change::Device(
        device_id.to_owned(),
    ))
    .await;
}
