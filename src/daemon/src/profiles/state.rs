use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::{mpsc, watch, Mutex};

use crate::profiles::focus_watcher::{ControlMsg, FocusWatcherEngine};
use crate::run_loop::EngineRunConfig;

struct Engine {
    handle: Arc<FocusWatcherEngine>,
    cfg_tx: watch::Sender<EngineRunConfig>,
}

/// The focus-watcher engine handle and config channel, its control channel
/// (set separately and earlier, before the engine itself starts — see
/// `main.rs`), and whether a platform focus backend is available.
pub struct FocusState {
    ctrl_tx: Mutex<Option<mpsc::Sender<ControlMsg>>>,
    supported: AtomicBool,
    engine: OnceLock<Engine>,
}

impl FocusState {
    pub fn new() -> Self {
        Self {
            ctrl_tx: Mutex::new(None),
            supported: AtomicBool::new(false),
            engine: OnceLock::new(),
        }
    }

    pub fn set_engine(
        &self,
        handle: Arc<FocusWatcherEngine>,
        cfg_tx: watch::Sender<EngineRunConfig>,
    ) {
        let _ = self.engine.set(Engine { handle, cfg_tx });
    }

    pub fn engine(&self) -> Option<&Arc<FocusWatcherEngine>> {
        self.engine.get().map(|e| &e.handle)
    }

    pub fn cfg_tx(&self) -> Option<&watch::Sender<EngineRunConfig>> {
        self.engine.get().map(|e| &e.cfg_tx)
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
