// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "linux")]
//! Linux power-profile backend. Talks to power-profiles-daemon over the system
//! bus (the `ActiveProfile` property), falling back to the `powerprofilesctl`
//! CLI when the D-Bus call is unavailable.

use anyhow::Result;
use async_trait::async_trait;
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

use super::{index_of, PowerProfileBackend, PROFILES};

/// `(destination, object path, interface)` for power-profiles-daemon. The
/// project renamed its bus name; try the current one first, then the legacy one.
const PPD_TARGETS: &[(&str, &str, &str)] = &[
    (
        "org.freedesktop.UPower.PowerProfiles",
        "/org/freedesktop/UPower/PowerProfiles",
        "org.freedesktop.UPower.PowerProfiles",
    ),
    (
        "net.hadess.PowerProfiles",
        "/net/hadess/PowerProfiles",
        "net.hadess.PowerProfiles",
    ),
];

const PROPS: &str = "org.freedesktop.DBus.Properties";

pub struct LinuxPowerProfile;

impl LinuxPowerProfile {
    /// Register the device only when the host can actually switch profiles —
    /// either power-profiles-daemon answers on D-Bus, or `powerprofilesctl` is
    /// installed.
    pub async fn detect() -> Option<Self> {
        if dbus_get_active().await.is_some() || powerprofilesctl_available().await {
            Some(Self)
        } else {
            None
        }
    }
}

#[async_trait]
impl PowerProfileBackend for LinuxPowerProfile {
    async fn current(&self) -> Option<&'static str> {
        let raw = match dbus_get_active().await {
            Some(s) => s,
            None => ctl_get().await?,
        };
        // ppd reports the same strings we use as canonical ids.
        index_of(raw.trim()).map(|i| PROFILES[i].0)
    }

    async fn apply(&self, id: &str) -> Result<()> {
        match dbus_set_active(id).await {
            Ok(()) => Ok(()),
            Err(e) => {
                log::debug!("[ComputerDevice] D-Bus set failed ({e:#}); trying powerprofilesctl");
                ctl_set(id).await
            }
        }
    }
}

async fn dbus_get_active() -> Option<String> {
    let conn = Connection::system().await.ok()?;
    for (dest, path, iface) in PPD_TARGETS {
        if let Ok(reply) = conn
            .call_method(
                Some(*dest),
                *path,
                Some(PROPS),
                "Get",
                &(*iface, "ActiveProfile"),
            )
            .await
        {
            if let Ok((v,)) = reply.body().deserialize::<(OwnedValue,)>() {
                if let Ok(s) = String::try_from(v) {
                    return Some(s);
                }
            }
        }
    }
    None
}

async fn dbus_set_active(profile: &str) -> Result<()> {
    let conn = Connection::system().await?;
    let mut last: anyhow::Error = anyhow::anyhow!("power-profiles-daemon not reachable on D-Bus");
    for (dest, path, iface) in PPD_TARGETS {
        let value = Value::from(profile);
        match conn
            .call_method(
                Some(*dest),
                *path,
                Some(PROPS),
                "Set",
                &(*iface, "ActiveProfile", value),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => last = e.into(),
        }
    }
    Err(last)
}

const CTL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn powerprofilesctl_available() -> bool {
    let fut = tokio::process::Command::new("powerprofilesctl")
        .arg("list")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    tokio::time::timeout(CTL_TIMEOUT, fut)
        .await
        .ok()
        .and_then(|r| r.ok())
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn ctl_get() -> Option<String> {
    let out = tokio::time::timeout(
        CTL_TIMEOUT,
        tokio::process::Command::new("powerprofilesctl")
            .arg("get")
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

async fn ctl_set(profile: &str) -> Result<()> {
    let status = tokio::time::timeout(
        CTL_TIMEOUT,
        tokio::process::Command::new("powerprofilesctl")
            .args(["set", profile])
            .status(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("powerprofilesctl set timed out"))??;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("powerprofilesctl set {profile} failed with {status}")
    }
}
