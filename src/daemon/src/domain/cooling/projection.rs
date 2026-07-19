// SPDX-License-Identifier: GPL-3.0-or-later
//! Projects cooling runtime state into its retained bus representation.

use std::sync::Arc;

use crate::application::state::AppState;
use crate::config::Config;
use crate::domain::cooling::model::FanCurveRecord;

pub async fn project(
    app: &Arc<AppState>,
    cfg: &Config,
    fan_curves: Vec<(String, String, FanCurveRecord)>,
) -> halod_shared::types::CoolingState {
    let mut cooling = app.cooling.snapshot(fan_curves).await;
    cooling.config = cfg.cooling.clone();
    cooling
}
