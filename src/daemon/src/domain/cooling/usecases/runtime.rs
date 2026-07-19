// SPDX-License-Identifier: GPL-3.0-or-later
//! Application boundary for observed cooling-engine state changes.

use std::sync::Arc;

use crate::application::state::AppState;

pub async fn status_changed(app: &Arc<AppState>) {
    app.record_change(crate::application::bus::coordinator::Change::Cooling)
        .await;
}
