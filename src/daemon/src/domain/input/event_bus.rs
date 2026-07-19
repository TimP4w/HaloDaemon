// SPDX-License-Identifier: GPL-3.0-or-later
//! Private transient input lane. Events are never retained or exposed over IPC.

use super::ButtonEvent;
use tokio::sync::broadcast;

pub struct InputEventBus {
    events: broadcast::Sender<ButtonEvent>,
}

impl Default for InputEventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl InputEventBus {
    pub fn new() -> Self {
        Self {
            events: broadcast::channel(256).0,
        }
    }
    pub fn publish(&self, event: ButtonEvent) {
        let _ = self.events.send(event);
    }
    pub fn subscribe(&self) -> broadcast::Receiver<ButtonEvent> {
        self.events.subscribe()
    }
}
