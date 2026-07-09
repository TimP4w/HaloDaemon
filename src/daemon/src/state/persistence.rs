// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::Arc;
use tokio::sync::watch;

/// Debounced config-save signaling and the device-state persist notify —
/// both drained by background workers started in `main` (see `workers.rs`),
/// keeping engines and usecases decoupled from the actual disk write.
pub struct Persistence {
    /// Send `()` to schedule a config save; the worker snapshots `config`
    /// itself when the debounce fires.
    pub save_tx: watch::Sender<()>,
    /// Notified by engines to request that a device's state be persisted.
    pub notify: Arc<tokio::sync::Notify>,
}

impl Persistence {
    pub fn new() -> Self {
        let (save_tx, _) = watch::channel(());
        Self {
            save_tx,
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}
