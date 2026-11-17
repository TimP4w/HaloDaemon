// SPDX-License-Identifier: GPL-3.0-or-later
//! Registers the synthetic host-computer device during discovery.

use std::sync::Arc;

use crate::application::state::AppState;
use crate::application::usecases::registry::registration;
use crate::domain::registry::observers::discovery::TransportScanner;
use crate::infrastructure::drivers::vendors::generic::devices::computer;

async fn discover(app: Arc<AppState>) {
    let Some(device) = computer::make_device().await else {
        return;
    };
    registration::register_device(&app, device).await;
}

pub(crate) const SCANNER: TransportScanner = TransportScanner {
    name: "computer",
    detail: halod_shared::types::DiscoveryDetail::Computer,
    platform: None,
    scan: |app| Box::pin(discover(app)),
};
