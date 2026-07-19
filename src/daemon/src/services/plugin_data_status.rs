// SPDX-License-Identifier: GPL-3.0-or-later
//! Projects dynamic plugin-data freshness into the retained plugin summary topic.

use std::sync::Arc;
use tokio::sync::broadcast;

use crate::state::AppState;

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
                crate::plugin::usecases::runtime::data_changed(&app).await;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                crate::plugin::usecases::runtime::data_changed(&app).await;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}
