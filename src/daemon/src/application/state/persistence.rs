// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::Arc;
use tokio::sync::watch;

/// Config persistence lifecycle. The monotonically increasing save version is
/// the authoritative dirty marker; `Failed` therefore remains retryable until
/// that version is successfully written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSaveState {
    Clean,
    Debouncing,
    Saving(u64),
    DirtyWhileSaving,
    Failed(String),
    Stopping,
}

/// Debounced config-save signaling and the device-state persist notify —
/// both drained by background workers started in `main` (see `workers.rs`),
/// keeping engines and usecases decoupled from the actual disk write.
pub struct Persistence {
    /// Send `()` to schedule a config save; the worker snapshots `config`
    /// itself when the debounce fires.
    pub save_tx: watch::Sender<u64>,
    pub save_state: watch::Sender<ConfigSaveState>,
    pub shutdown_tx: watch::Sender<bool>,
    /// Notified by engines to request that a device's state be persisted.
    pub notify: Arc<tokio::sync::Notify>,
}

impl Persistence {
    pub fn new() -> Self {
        let (save_tx, _) = watch::channel(0);
        Self {
            save_tx,
            save_state: watch::channel(ConfigSaveState::Clean).0,
            shutdown_tx: watch::channel(false).0,
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}
