// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "linux")]

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{
    drivers::{
        vendors::generic::devices::hwmon_device::{HwmonDevice, HwmonFanDevice},
        Device, Metered,
    },
    state::AppState,
};
use halod_shared::types::{WriteRateLimit, WriteRateStatus};

/// Gated sysfs access for one hwmon chip directory: every attribute write
/// (`pwm*`, `pwm*_enable`) goes through the write-rate gate; reads are
/// unmetered, matching the read/write split on every other transport.
#[derive(Clone)]
pub struct HwmonIo {
    io: Metered<PathBuf>,
}

impl HwmonIo {
    pub fn new(chip_dir: PathBuf, limit: Option<WriteRateLimit>) -> Self {
        Self {
            io: Metered::new(chip_dir, limit),
        }
    }

    pub fn dir(&self) -> &Path {
        self.io.read_access()
    }

    /// Reads `dir/rel`; `None` if the file doesn't exist or isn't readable.
    pub fn read_attr(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.dir().join(rel)).ok()
    }

    /// Metered write of `value` to `dir/rel`.
    pub async fn write_attr(&self, rel: &str, value: &str) -> Result<()> {
        let dir = self.io.write_access(value.len()).await?;
        let path = dir.join(rel);
        tokio::fs::write(&path, value)
            .await
            .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", path.display()))
    }

    pub fn rate_status(&self) -> WriteRateStatus {
        self.io.status()
    }

    #[cfg(test)]
    pub fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.io.set_limit(limit);
    }
}

inventory::submit!(crate::registry::discovery::TransportScanner {
    name: "hwmon",
    platform: Some("linux"),
    scan: |app| Box::pin(async move {
        if let Err(e) = HwmonTransport::discover(app).await {
            log::error!("Hwmon discovery failed: {}", e);
        }
    }),
});

pub struct HwmonTransport;

impl HwmonTransport {
    pub async fn discover(app: Arc<AppState>) -> Result<()> {
        // sysfs reads are blocking kernel calls, so enumerate in a blocking thread.
        struct FanEntry {
            path: std::path::PathBuf,
            fan_index: u32,
            stable_id: String,
        }
        struct ChipEntry {
            path: std::path::PathBuf,
            fans: Vec<FanEntry>,
        }

        let chips: Vec<ChipEntry> = tokio::task::spawn_blocking(|| {
            let hwmon_path = std::path::Path::new("/sys/class/hwmon");
            if !hwmon_path.exists() {
                return Vec::new();
            }
            let entries = match std::fs::read_dir(hwmon_path) {
                Ok(e) => e,
                Err(err) => {
                    log::error!("[HwmonTransport] Failed to read /sys/class/hwmon: {}", err);
                    return Vec::new();
                }
            };
            let mut chips = Vec::new();
            for entry in entries.flatten() {
                let dir_name = entry.file_name();
                let dir_name = dir_name.to_string_lossy();
                if !dir_name.starts_with("hwmon") {
                    continue;
                }
                let stable_id = HwmonDevice::new(entry.path()).stable_id().to_string();
                let mut fans = Vec::new();
                for fan_index in 1u32..=16 {
                    let fan_input = entry.path().join(format!("fan{}_input", fan_index));
                    let pwm = entry.path().join(format!("pwm{}", fan_index));
                    if fan_input.exists() && pwm.exists() {
                        fans.push(FanEntry {
                            path: entry.path(),
                            fan_index,
                            stable_id: stable_id.clone(),
                        });
                    }
                }
                chips.push(ChipEntry {
                    path: entry.path(),
                    fans,
                });
            }
            chips
        })
        .await
        .unwrap_or_else(|e| {
            log::error!("[HwmonTransport] spawn_blocking panicked: {e}");
            Vec::new()
        });

        for chip in chips {
            let chip_device: Arc<dyn Device> = Arc::new(HwmonDevice::new(chip.path));
            crate::registry::usecases::registration::register_device(&app, chip_device).await;
            for fan in chip.fans {
                let fan_device: Arc<dyn Device> =
                    Arc::new(HwmonFanDevice::new(fan.path, fan.fan_index, fan.stable_id));
                crate::registry::usecases::registration::register_device(&app, fan_device).await;
            }
        }

        Ok(())
    }
}
