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

    /// Send a control message when the focus watcher is running.
    pub async fn notify(&self, msg: ControlMsg) {
        let tx = self.ctrl_tx.lock().await.clone();
        if let Some(tx) = tx {
            let _ = tx.send(msg).await;
        }
    }

    pub fn set_supported(&self, v: bool) {
        self.supported.store(v, Ordering::Relaxed);
    }

    pub fn supported(&self) -> bool {
        self.supported.load(Ordering::Relaxed)
    }
}
