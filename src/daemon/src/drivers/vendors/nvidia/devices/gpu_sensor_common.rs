// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026-present HaloDaemon contributors

//! Shared passive sensor device for NVIDIA GPUs; the platform reading source
//! (NvAPI on Windows, `nvidia-smi` on Linux) is supplied via [`SensorSource`].

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::drivers::vendors::generic::devices::common::TaskHandle;
use crate::drivers::{CapabilityRef, Device, SensorCapability};
use halod_shared::types::{DeviceType, Sensor, SensorType, SensorUnit};

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// A platform-specific temperature source for one GPU.
#[async_trait]
pub trait SensorSource: Send + Sync + 'static {
    /// `(label, temperature_c)` per reading. Empty on read failure.
    async fn read(&self) -> Vec<(&'static str, f64)>;
    /// True if the cache should be primed synchronously in `initialize`.
    fn prime_on_init(&self) -> bool {
        false
    }
    /// Value for [`Device::debug_transport`].
    fn transport_name(&self) -> &'static str;
}

/// Map raw `(label, temperature_c)` readings to wire [`Sensor`]s.
pub fn build_sensors(stable_id: &str, readings: Vec<(&'static str, f64)>) -> Vec<Sensor> {
    readings
        .into_iter()
        .enumerate()
        .map(|(i, (label, temperature_c))| Sensor {
            id: format!("{stable_id}_temp{}", i + 1),
            name: label.to_string(),
            value: temperature_c,
            unit: SensorUnit::Celsius,
            sensor_type: SensorType::Temperature,
            visibility: Default::default(),
        })
        .collect()
}

pub struct NvidiaSensorDevice<S: SensorSource> {
    name: String,
    stable_id: String,
    source: Arc<S>,
    cached_sensors: Arc<Mutex<Vec<Sensor>>>,
    poll_task: Mutex<Option<TaskHandle>>,
}

impl<S: SensorSource> NvidiaSensorDevice<S> {
    pub fn new(name: String, stable_id: String, source: S) -> Self {
        Self {
            name,
            stable_id,
            source: Arc::new(source),
            cached_sensors: Arc::new(Mutex::new(vec![])),
            poll_task: Mutex::new(None),
        }
    }

    async fn poll_once(source: &S, stable_id: &str) -> Vec<Sensor> {
        build_sensors(stable_id, source.read().await)
    }
}

#[async_trait]
impl<S: SensorSource> Device for NvidiaSensorDevice<S> {
    fn id(&self) -> &str {
        &self.stable_id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn vendor(&self) -> &str {
        "NVIDIA"
    }
    fn model(&self) -> &str {
        &self.name
    }

    async fn initialize(&self) -> Result<bool> {
        let source = Arc::clone(&self.source);
        let stable_id = self.stable_id.clone();
        let cached = Arc::clone(&self.cached_sensors);

        // Prime the cache once so the first sensor snapshot isn't empty.
        if self.source.prime_on_init() {
            *cached.lock().await = Self::poll_once(&source, &stable_id).await;
        }

        let task = tokio::task::spawn(async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                let sensors = Self::poll_once(&source, &stable_id).await;
                *cached.lock().await = sensors;
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(task));
        log::info!("[NvidiaSensor] Initialized: {}", self.name);
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
        Some(self.source.transport_name())
    }
}

#[async_trait]
impl<S: SensorSource> SensorCapability for NvidiaSensorDevice<S> {
    async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        Ok(self.cached_sensors.lock().await.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_sensors_maps_label_value_and_indexed_id() {
        let sensors = build_sensors("nvidia_gpu_0", vec![("GPU Core", 55.0), ("Memory", 60.5)]);
        assert_eq!(sensors.len(), 2);
        assert_eq!(sensors[0].id, "nvidia_gpu_0_temp1");
        assert_eq!(sensors[0].name, "GPU Core");
        assert_eq!(sensors[0].value, 55.0);
        assert!(matches!(sensors[0].unit, SensorUnit::Celsius));
        assert_eq!(sensors[0].sensor_type, SensorType::Temperature);
        assert_eq!(sensors[1].id, "nvidia_gpu_0_temp2");
        assert_eq!(sensors[1].name, "Memory");
        assert_eq!(sensors[1].value, 60.5);
    }

    #[test]
    fn build_sensors_empty_readings_yields_no_sensors() {
        assert!(build_sensors("nvidia_gpu_0", vec![]).is_empty());
    }
}
