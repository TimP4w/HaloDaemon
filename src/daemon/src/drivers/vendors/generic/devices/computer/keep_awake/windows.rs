#![cfg(target_os = "windows")]
//! Windows keep-awake via `SetThreadExecutionState`. The execution state is
//! bound to the thread that sets it, so a dedicated thread holds it for the
//! lifetime of the "on" state and clears it when signalled to stop.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::thread::JoinHandle;
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};

use super::KeepAwake;

#[derive(Default)]
pub struct WindowsKeepAwake {
    /// Dropping this `Sender` signals the worker thread to clear the state.
    stop: Mutex<Option<Sender<()>>>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

#[async_trait]
impl KeepAwake for WindowsKeepAwake {
    fn is_active(&self) -> bool {
        self.stop.lock().unwrap().is_some()
    }

    async fn set(&self, on: bool) -> Result<()> {
        let mut guard = self.stop.lock().unwrap();
        if on {
            if guard.is_none() {
                let (tx, rx) = mpsc::channel::<()>();
                let handle = std::thread::spawn(move || {
                    unsafe {
                        SetThreadExecutionState(
                            ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED,
                        );
                    }
                    // Blocks until the Sender is dropped (the stop signal).
                    let _ = rx.recv();
                    unsafe {
                        SetThreadExecutionState(ES_CONTINUOUS);
                    }
                });
                *guard = Some(tx);
                *self.handle.lock().unwrap() = Some(handle);
            }
        } else {
            *guard = None;
            // Join the worker so ES_CONTINUOUS is cleared before we return.
            if let Some(h) = self.handle.lock().unwrap().take() {
                let _ = h.join();
            }
        }
        Ok(())
    }
}
