#![cfg(target_os = "windows")]

//! NVIDIA GPU temperature sensors on Windows, sourced via NvAPI
//! ([`nvapi_thermal`]). The device/poll/cache logic lives in
//! [`gpu_sensor_common`].

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::drivers::vendors::nvidia::devices::gpu_sensor_common::{
    NvidiaSensorDevice, SensorSource,
};
use crate::drivers::vendors::nvidia::devices::nvapi_thermal;
use crate::drivers::Device;
use crate::state::AppState;

struct NvapiSource {
    handle: usize,
}

#[async_trait]
impl SensorSource for NvapiSource {
    async fn read(&self) -> Vec<(&'static str, f64)> {
        let handle = self.handle;
        tokio::task::spawn_blocking(move || match nvapi_thermal::read_temperatures(handle) {
            Ok(r) => r.into_iter().map(|r| (r.label, r.temperature_c)).collect(),
            Err(e) => {
                log::trace!("[NvidiaGpuSensor] read_temperatures failed: {e}");
                vec![]
            }
        })
        .await
        .unwrap_or_else(|e| {
            log::warn!("[NvAPI sensor] task panicked: {e}");
            vec![]
        })
    }

    fn transport_name(&self) -> &'static str {
        "nvapi"
    }
}

inventory::submit!(crate::registry::discovery::TransportScanner {
    name: "Nvidia GPU",
    platform: Some("windows"),
    scan: |app| Box::pin(async move {
        if let Err(e) = NvidiaGpuTransport::discover(app).await {
            log::error!("NVIDIA GPU sensor discovery failed: {}", e);
        }
    }),
});

pub struct NvidiaGpuTransport;

impl NvidiaGpuTransport {
    pub async fn discover(app: Arc<AppState>) -> Result<()> {
        let gpus = nvapi_thermal::enumerate_gpus();
        if gpus.is_empty() {
            log::debug!("[NvidiaGpuTransport] no NVIDIA GPUs found");
            return Ok(());
        }

        for (index, gpu) in gpus.into_iter().enumerate() {
            let stable_id = format!("nvidia_gpu_{index}");
            let device: Arc<dyn Device> = Arc::new(NvidiaSensorDevice::new(
                gpu.name,
                stable_id,
                NvapiSource { handle: gpu.handle },
            ));
            crate::registry::usecases::registration::register_device(&app, device).await;
        }
        Ok(())
    }
}
