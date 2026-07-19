// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{mpsc, Mutex};

use crate::domain::profiles::observers::active_window::ControlMsg;

/// The focus-watcher control channel and whether a platform focus backend is
/// available.
pub struct FocusState {
    ctrl_tx: Mutex<Option<mpsc::Sender<ControlMsg>>>,
    supported: AtomicBool,
}

impl FocusState {
    pub fn new() -> Self {
        Self {
            ctrl_tx: Mutex::new(None),
            supported: AtomicBool::new(false),
        }
    }

    pub async fn set_ctrl_tx(&self, tx: mpsc::Sender<ControlMsg>) {
        *self.ctrl_tx.lock().await = Some(tx);
    }

    /// Best-effort control message to the running focus watcher; a no-op if
    /// the engine hasn't started yet or the channel is full.
    pub async fn notify(&self, msg: ControlMsg) {
        if let Some(tx) = &*self.ctrl_tx.lock().await {
            let _ = tx.try_send(msg);
        }
    }

    pub fn set_supported(&self, v: bool) {
        self.supported.store(v, Ordering::Relaxed);
    }

    pub fn supported(&self) -> bool {
        self.supported.load(Ordering::Relaxed)
    }
}
