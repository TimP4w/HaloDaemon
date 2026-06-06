#![cfg(target_os = "windows")]

//! `NvidiaGpuSensorDevice` — one passive sensor-capability device per NVIDIA
//! GPU. Reads GPU Core (and Memory Junction on capable parts) every second
//! via [`nvapi_thermal`].

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::drivers::vendors::generic::devices::common::TaskHandle;
use crate::drivers::vendors::nvidia::devices::nvapi_thermal;
use crate::drivers::{CapabilityRef, Device, SensorCapability};
use crate::state::AppState;
use halod_protocol::types::{
    DeviceType, Sensor, SensorType, SensorUnit,
};

pub struct NvidiaGpuSensorDevice {
    handle: usize,
    name: String,
    stable_id: String,
    cached_sensors: Arc<Mutex<Vec<Sensor>>>,
    poll_task: Mutex<Option<TaskHandle>>,
}

impl NvidiaGpuSensorDevice {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

    pub fn new(handle: usize, name: String, index: usize) -> Self {
        let stable_id = format!("nvidia_gpu_{index}");
        Self {
            handle,
            name,
            stable_id,
            cached_sensors: Arc::new(Mutex::new(vec![])),
            poll_task: Mutex::new(None),
        }
    }

    fn poll_once(handle: usize, stable_id: &str) -> Vec<Sensor> {
        let readings = match nvapi_thermal::read_temperatures(handle) {
            Ok(r) => r,
            Err(e) => {
                log::trace!("[NvidiaGpuSensor] read_temperatures failed: {e}");
                return vec![];
            }
        };
        readings
            .into_iter()
            .enumerate()
            .map(|(i, r)| Sensor {
                id: format!("{stable_id}_temp{}", i + 1),
                name: r.label.to_string(),
                value: r.temperature_c,
                unit: SensorUnit::Celsius,
                sensor_type: SensorType::Temperature,
                visibility: Default::default(),
            })
            .collect()
    }
}

#[async_trait]
impl Device for NvidiaGpuSensorDevice {
    fn id(&self) -> String { self.stable_id.clone() }
    fn name(&self) -> &str { &self.name }
    fn vendor(&self) -> &str { "NVIDIA" }
    fn model(&self) -> &str { &self.name }

    async fn initialize(&self) -> Result<bool> {
        let handle = self.handle;
        let stable_id = self.stable_id.clone();
        let cached = Arc::clone(&self.cached_sensors);

        let task = tokio::task::spawn(async move {
            loop {
                tokio::time::sleep(NvidiaGpuSensorDevice::POLL_INTERVAL).await;
                let stable_id_call = stable_id.clone();
                let sensors =
                    tokio::task::spawn_blocking(move || {
                        NvidiaGpuSensorDevice::poll_once(handle, &stable_id_call)
                    })
                    .await
                    .unwrap_or_default();
                *cached.lock().await = sensors;
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(task));
        log::info!("[NvidiaGpuSensor] Initialized: {}", self.name);
        Ok(true)
    }

    async fn close(&self) {
        self.poll_task.lock().await.take();
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![CapabilityRef::Sensor(self)]
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Sensor
    }

    fn debug_transport(&self) -> Option<&'static str> {
        Some("nvapi")
    }
}

#[async_trait]
impl SensorCapability for NvidiaGpuSensorDevice {
    async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        Ok(self.cached_sensors.lock().await.clone())
    }
}

// ─── Discovery transport ─────────────────────────────────────────────────────

inventory::submit!(crate::discovery::TransportScanner {
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
            let device: Arc<dyn Device> =
                Arc::new(NvidiaGpuSensorDevice::new(gpu.handle, gpu.name, index));
            crate::usecases::registration::register_device(&app, device).await;
        }
        Ok(())
    }
}
