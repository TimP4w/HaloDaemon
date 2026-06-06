#![cfg(target_os = "linux")]

use anyhow::Result;
use std::sync::Arc;

use crate::{
    drivers::{
        vendors::generic::devices::hwmon_device::{HwmonDevice, HwmonFanDevice},
        Device,
    },
    state::AppState,
};

inventory::submit!(crate::discovery::TransportScanner {
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
        let hwmon_path = std::path::Path::new("/sys/class/hwmon");
        if !hwmon_path.exists() {
            log::debug!("[HwmonTransport] /sys/class/hwmon not present, skipping");
            return Ok(());
        }

        let entries = match std::fs::read_dir(hwmon_path) {
            Ok(e) => e,
            Err(err) => {
                log::error!("[HwmonTransport] Failed to read /sys/class/hwmon: {}", err);
                return Ok(());
            }
        };

        for entry in entries.flatten() {
            let dir_name = entry.file_name();
            let dir_name = dir_name.to_string_lossy();
            if !dir_name.starts_with("hwmon") {
                continue;
            }

            // Chip-level temperature sensor device.
            let chip_device = HwmonDevice::new(entry.path());
            let stable_id = chip_device.stable_id().to_string();
            let chip: Arc<dyn Device> = Arc::new(chip_device);
            crate::usecases::registration::register_device(&app, chip).await;

            // Per-fan PWM devices — only register when both fan input AND PWM control exist.
            // Read-only RPM sensors (no pwm file) are skipped; they can't be used in fan curves
            // and the chip-level HwmonDevice already surfaces temperature data for the same chip.
            for fan_index in 1u32..=16 {
                let fan_input = entry.path().join(format!("fan{}_input", fan_index));
                let pwm = entry.path().join(format!("pwm{}", fan_index));
                if fan_input.exists() && pwm.exists() {
                    let fan: Arc<dyn Device> = Arc::new(HwmonFanDevice::new(
                        entry.path(),
                        fan_index,
                        stable_id.clone(),
                    ));
                    crate::usecases::registration::register_device(&app, fan).await;
                }
            }
        }

        Ok(())
    }
}
