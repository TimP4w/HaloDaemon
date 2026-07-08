// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026-present HaloDaemon contributors

#![cfg(target_os = "linux")]

//! NVIDIA GPU temperature sensors on Linux, sourced by shelling out to
//! [`nvidia_smi`]. The device/poll/cache logic lives in [`gpu_sensor_common`].

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::drivers::vendors::nvidia::devices::gpu_sensor_common::{
    NvidiaSensorDevice, SensorSource,
};
use crate::drivers::vendors::nvidia::devices::nvidia_smi;
use crate::drivers::Device;
use crate::state::AppState;

struct SmiSource {
    uuid: String,
}

#[async_trait]
impl SensorSource for SmiSource {
    async fn read(&self) -> Vec<(&'static str, f64)> {
        match nvidia_smi::read_temperatures(&self.uuid).await {
            Ok(r) => r.into_iter().map(|r| (r.label, r.temperature_c)).collect(),
            Err(e) => {
                log::trace!("[NvidiaSmiSensor] read_temperatures failed: {e}");
                vec![]
            }
        }
    }

    fn prime_on_init(&self) -> bool {
        true
    }

    fn transport_name(&self) -> &'static str {
        "nvidia-smi"
    }
}

inventory::submit!(crate::registry::discovery::TransportScanner {
    name: "Nvidia GPU",
    platform: Some("linux"),
    scan: |app| Box::pin(async move {
        if let Err(e) = NvidiaSmiTransport::discover(app).await {
            log::error!("NVIDIA GPU sensor discovery failed: {}", e);
        }
    }),
});

pub struct NvidiaSmiTransport;

impl NvidiaSmiTransport {
    pub async fn discover(app: Arc<AppState>) -> Result<()> {
        let gpus = nvidia_smi::enumerate_gpus().await;
        if gpus.is_empty() {
            log::debug!("[NvidiaSmiTransport] no NVIDIA GPUs found");
            return Ok(());
        }

        for gpu in gpus {
            let stable_id = format!("nvidia_gpu_{}", gpu.uuid);
            let device: Arc<dyn Device> = Arc::new(NvidiaSensorDevice::new(
                gpu.name,
                stable_id,
                SmiSource { uuid: gpu.uuid },
            ));
            crate::registry::usecases::registration::register_device(&app, device).await;
        }
        Ok(())
    }
}
