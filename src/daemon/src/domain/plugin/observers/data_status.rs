// SPDX-License-Identifier: GPL-3.0-or-later
//! Observes dynamic plugin-data freshness and refreshes the retained plugin summary.

use std::sync::Arc;
use tokio::sync::broadcast;

use crate::application::state::AppState;
use crate::application::usecases::plugin::runtime;

pub async fn run(app: Arc<AppState>) {
    let mut changes = app.data_bus.subscribe();
    loop {
        match changes.recv().await {
            Ok(change) => {
                if change.key.starts_with("host.") {
                    continue;
                }
                while let Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) =
                    changes.try_recv()
                {}
                runtime::data_changed(&app).await;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                runtime::data_changed(&app).await;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}
