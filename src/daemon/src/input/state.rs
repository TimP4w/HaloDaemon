// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use crate::input::action_executor::ActionExecutor;

/// A button press or release event from a diverted HID++ button.
#[derive(Debug, Clone)]
pub struct ButtonEvent {
    /// ID of the device that emitted the event.
    pub device_id: String,
    /// CIDs that transitioned to pressed since the last event.
    pub pressed: Vec<u16>,
    /// CIDs that transitioned to released since the last event.
    pub released: Vec<u16>,
}

/// Key-remap input plumbing owned by `KeyRemapEngine`.
pub struct InputState {
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// Held Layer Shift tokens, keyed by (device_id, cid); active iff non-empty.
    layer_shift_held: Mutex<HashSet<(String, u16)>>,
    /// Injection backend; unset on headless Linux.
    executor: OnceLock<Arc<ActionExecutor>>,
}

impl InputState {
    pub fn new() -> Self {
        Self {
            shutdown_tx: tokio::sync::watch::channel(false).0,
            layer_shift_held: Mutex::new(HashSet::new()),
            executor: OnceLock::new(),
        }
    }

    pub fn set_executor(&self, exec: Arc<ActionExecutor>) {
        let _ = self.executor.set(exec);
    }

    pub fn executor(&self) -> Option<Arc<ActionExecutor>> {
        self.executor.get().cloned()
    }

    pub fn layer_shift_active(&self) -> bool {
        !self.layer_shift_held.lock().unwrap().is_empty()
    }

    /// Hold a Layer Shift token; idempotent if already held.
    pub fn layer_shift_press(&self, device_id: &str, cid: u16) {
        self.layer_shift_held
            .lock()
            .unwrap()
            .insert((device_id.to_owned(), cid));
    }

    /// Release a Layer Shift token; a no-op if it wasn't held.
    pub fn layer_shift_release(&self, device_id: &str, cid: u16) {
        self.layer_shift_held
            .lock()
            .unwrap()
            .remove(&(device_id.to_owned(), cid));
    }

    /// Direct disconnect cleanup, not just the synthetic-release broadcast.
    pub fn layer_shift_clear_device(&self, device_id: &str) {
        self.layer_shift_held
            .lock()
            .unwrap()
            .retain(|(d, _)| d != device_id);
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown_tx.send_replace(true);
    }

    pub(crate) fn shutdown_rx(&self) -> tokio::sync::watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }
}
