#![cfg(target_os = "windows")]
//! Windows power-profile backend. Uses the built-in `powercfg` tool (available
//! on Windows 10 and later) to read and switch the active power plan.

use anyhow::Result;
use async_trait::async_trait;

use super::{
    parse_powercfg_active_guid, windows_guid_for, windows_profile_for_guid, PowerProfileBackend,
};

pub struct WindowsPowerProfile;

#[async_trait]
impl PowerProfileBackend for WindowsPowerProfile {
    async fn current(&self) -> Option<&'static str> {
        let out = tokio::process::Command::new("powercfg")
            .arg("/getactivescheme")
            .output()
            .await
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
        let status = tokio::process::Command::new("powercfg")
            .args(["/setactive", guid])
            .status()
            .await?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("powercfg /setactive {guid} failed with {status}")
        }
    }
}
