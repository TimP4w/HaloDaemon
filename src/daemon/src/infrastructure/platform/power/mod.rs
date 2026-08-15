// SPDX-License-Identifier: GPL-3.0-or-later
//! System resume notifications. Linux listens for logind's `PrepareForSleep`;
//! Windows registers a callback-mode suspend/resume notification.

use anyhow::Result;
use tokio::sync::mpsc::UnboundedReceiver;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

/// One message per system resume, or `None` on a platform with no power
/// notification API. `Err` is a transient subscription failure worth retrying.
pub async fn resume_events() -> Result<Option<UnboundedReceiver<()>>> {
    #[cfg(target_os = "linux")]
    {
        linux::resume_events().await.map(Some)
    }
    #[cfg(target_os = "windows")]
    {
        windows::resume_events().map(Some)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Ok(None)
    }
}
