// SPDX-License-Identifier: GPL-3.0-or-later
//! Application boundary for input-engine changes to observable device state.

use crate::domain::events::ChangeSink as _;

use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::events::Change;

pub async fn device_changed(app: &Arc<AppState>, device_id: &str) {
    app.record_change(Change::Device(device_id.to_owned()))
        .await;
}
