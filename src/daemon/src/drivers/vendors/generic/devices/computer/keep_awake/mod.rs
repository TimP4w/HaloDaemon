// SPDX-License-Identifier: GPL-3.0-or-later
//! Keep-awake toggle for the [`super::ComputerDevice`]: while on, the host is
//! prevented from idling/sleeping. Exposed as a `Boolean`. Linux holds a
//! systemd-logind inhibitor lock; Windows uses `SetThreadExecutionState`.

use anyhow::Result;
use async_trait::async_trait;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

/// The boolean `key` under which keep-awake is exposed to the UI.
pub const KEEP_AWAKE_KEY: &str = "keep_awake";

/// Holds (or releases) a system sleep/idle inhibitor.
#[async_trait]
pub trait KeepAwake: Send + Sync {
    /// Acquire the inhibitor when `on`, release it when `off`. Idempotent.
    async fn set(&self, on: bool) -> Result<()>;
    /// Whether the inhibitor is currently held.
    fn is_active(&self) -> bool;
}

/// No-op backend for platforms that have no keep-awake API.
struct NoOpKeepAwake;

#[async_trait]
impl KeepAwake for NoOpKeepAwake {
    async fn set(&self, _on: bool) -> Result<()> {
        Ok(())
    }
    fn is_active(&self) -> bool {
        false
    }
}

#[cfg(target_os = "linux")]
pub fn make_backend() -> Box<dyn KeepAwake> {
    Box::new(linux::LogindKeepAwake::default())
}

#[cfg(target_os = "windows")]
pub fn make_backend() -> Box<dyn KeepAwake> {
    Box::new(windows::WindowsKeepAwake::default())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn make_backend() -> Box<dyn KeepAwake> {
    Box::new(NoOpKeepAwake)
}
