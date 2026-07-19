// SPDX-License-Identifier: GPL-3.0-or-later
//! Registers the synthetic host-computer device during discovery.

use std::sync::Arc;

use crate::application::state::AppState;

async fn discover(app: Arc<AppState>) {
    let Some(device) =
        crate::infrastructure::drivers::vendors::generic::devices::computer::make_device().await
    else {
        return;
    };
    crate::domain::registry::usecases::registration::register_device(&app, device).await;
}

inventory::submit!(
    crate::domain::registry::observers::discovery::TransportScanner {
        name: "computer",
        detail: halod_shared::types::DiscoveryDetail::Computer,
        platform: None,
        scan: |app| Box::pin(discover(app)),
    }
);
