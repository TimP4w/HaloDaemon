// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "windows")]
//! Windows power-profile backend. Uses the built-in `powercfg` tool (available
//! on Windows 10 and later) to read and switch the active power plan.

use anyhow::Result;
use async_trait::async_trait;

use super::{
    parse_powercfg_active_guid, windows_guid_for, windows_profile_for_guid, PowerProfileBackend,
};

const POWERCFG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub struct WindowsPowerProfile;

#[async_trait]
impl PowerProfileBackend for WindowsPowerProfile {
    async fn current(&self) -> Option<&'static str> {
        let out = tokio::time::timeout(
            POWERCFG_TIMEOUT,
            tokio::process::Command::new("powercfg")
                .arg("/getactivescheme")
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let guid = parse_powercfg_active_guid(&text)?;
        windows_profile_for_guid(&guid)
    }

    async fn apply(&self, id: &str) -> Result<()> {
        let guid =
            windows_guid_for(id).ok_or_else(|| anyhow::anyhow!("unknown power profile {id}"))?;
        let status = tokio::time::timeout(
            POWERCFG_TIMEOUT,
            tokio::process::Command::new("powercfg")
                .args(["/setactive", guid])
                .status(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("powercfg /setactive timed out"))??;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("powercfg /setactive {guid} failed with {status}")
        }
    }
}
