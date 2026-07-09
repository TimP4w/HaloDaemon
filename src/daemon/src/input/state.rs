// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, OnceLock};
use tokio::sync::broadcast;

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

/// Key-remap input plumbing: the button-event bus `KeyRemapEngine` consumes
/// and the Layer Shift modifier state it owns.
pub struct InputState {
    /// Button events emitted by diverted HID++ buttons; the sole input feed
    /// for `KeyRemapEngine` (which is spawned standalone, not held as a
    /// domain engine).
    pub button_event_tx: broadcast::Sender<ButtonEvent>,
    /// Reference count of held Layer Shift buttons across all devices.
    /// Layer Shift is active when this is > 0. Read/written only by
    /// `KeyRemapEngine`.
    pub layer_shift_active: AtomicU32,
    /// Injection backend, wired at startup when it initializes successfully
    /// (may fail on headless Linux); used by the `PlayMacro` usecase.
    executor: OnceLock<Arc<ActionExecutor>>,
}

impl InputState {
    pub fn new() -> Self {
        let (button_event_tx, _) = broadcast::channel(256);
        Self {
            button_event_tx,
            layer_shift_active: AtomicU32::new(0),
            executor: OnceLock::new(),
        }
    }

    pub fn set_executor(&self, exec: Arc<ActionExecutor>) {
        let _ = self.executor.set(exec);
    }

    pub fn executor(&self) -> Option<Arc<ActionExecutor>> {
        self.executor.get().cloned()
    }
}
