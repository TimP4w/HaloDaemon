#![cfg(target_os = "linux")]
//! Linux keep-awake via a systemd-logind inhibitor lock. Holding the file
//! descriptor returned by `Inhibit` keeps the lock; dropping it releases.

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;
use zbus::zvariant::OwnedFd;
use zbus::Connection;

use super::KeepAwake;

#[derive(Default)]
pub struct LogindKeepAwake {
    /// The held inhibitor fd; `Some` while keep-awake is on.
    inhibitor: Mutex<Option<OwnedFd>>,
}

#[async_trait]
impl KeepAwake for LogindKeepAwake {
    fn is_active(&self) -> bool {
        self.inhibitor.try_lock().map_or(true, |g| g.is_some())
    }

    async fn set(&self, on: bool) -> Result<()> {
        let mut guard = self.inhibitor.lock().await;
        if on {
            if guard.is_none() {
                *guard = Some(acquire().await.context("logind Inhibit failed")?);
            }
        } else {
            *guard = None;
        }
        Ok(())
    }
}

async fn acquire() -> Result<OwnedFd> {
    let conn = Connection::system().await?;
    let reply = conn
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Inhibit",
            &(
                "idle:sleep",
                halod_shared::app::APP_NAME,
                "Keep awake",
                "block",
            ),
        )
        .await?;
    let fd: OwnedFd = reply.body().deserialize()?;
    Ok(fd)
}
