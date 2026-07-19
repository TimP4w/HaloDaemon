// SPDX-License-Identifier: GPL-3.0-or-later
//! Projects lighting runtime state into its retained bus representation.

use std::sync::Arc;

use crate::application::state::AppState;
use crate::config::{Config, PlacedZone};

pub async fn project(
    app: &AppState,
    cfg: &Config,
    placed_zones: Vec<PlacedZone>,
) -> halod_shared::types::LightingOverviewState {
    let mut lighting = app
        .lighting
        .snapshot(&app.registry, cfg, placed_zones)
        .await;
    lighting.config = cfg.rgb.clone();
    lighting
}
