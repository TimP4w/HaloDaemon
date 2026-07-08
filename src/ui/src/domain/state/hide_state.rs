// SPDX-License-Identifier: GPL-3.0-or-later
//! Cross-backend channel for tray → window-loop wakeups. The tray sets
//! `wants_show` (for "Open") from its own thread and calls [`HideState::wake`];
//! on Linux the custom loop installs a `waker` that nudges its winit event
//! loop, because while the window is destroyed egui's edge-triggered repaint
//! callback won't fire. On eframe backends no waker is installed and the tray
//! relies on `ViewportCommand::Visible(true)`, so this is a harmless no-op
//! there.

#[derive(Default)]
pub struct HideState {
    pub wants_show: std::sync::atomic::AtomicBool,
    waker: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl HideState {
    /// Install the loop wakeup (Linux custom loop only).
    pub fn set_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        if let Ok(mut w) = self.waker.lock() {
            *w = Some(Box::new(waker));
        }
    }

    /// Wake the window loop so it acts on `wants_show` / `force_quit`.
    pub fn wake(&self) {
        if let Ok(w) = self.waker.lock() {
            if let Some(wake) = w.as_ref() {
                wake();
            }
        }
    }
}
