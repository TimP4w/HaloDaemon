#![cfg(target_os = "windows")]

//! AMD Ryzen (Zen, family 17h+) CPU temperature sensor.
//!
//! Polls the on-die SMN thermal registers once per second and exposes them as a
//! single sensor-capability device: package `Core (Tctl/Tdie)`, each populated
//! `CCDx (Tdie)`, and — on multi-CCD parts — `CCDs Max (Tdie)` /
//! `CCDs Average (Tdie)`.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

use crate::drivers::transports::amd_smn::AmdSmnBus;
use crate::drivers::vendors::amd::protocols::ryzen;
use crate::drivers::vendors::generic::devices::common::TaskHandle;
use crate::drivers::{CapabilityRef, Device, SensorCapability};
use halod_shared::types::{DeviceType, Sensor, SensorType, SensorUnit};

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Stable ID prefix — there is a single physical CPU package, so a fixed
/// prefix is sufficient and keeps sensor IDs constant across runs.
const STABLE_ID: &str = "amd_ryzen_cpu";

pub struct AmdCpuSensorDevice {
    bus: Arc<AmdSmnBus>,
    family: u8,
    model: u8,
    name: String,
    id: String,
    cached_sensors: Arc<TokioMutex<Vec<Sensor>>>,
    poll_task: TokioMutex<Option<TaskHandle>>,
}

impl AmdCpuSensorDevice {
    pub fn new(bus: Arc<AmdSmnBus>, family: u8, model: u8) -> Self {
        Self {
            bus,
            family,
            model,
            name: format!("AMD Ryzen ({})", ryzen::arch_label(family)),
            id: format!("amd_cpu_sensor_{STABLE_ID}"),
            cached_sensors: Arc::new(TokioMutex::new(vec![])),
            poll_task: TokioMutex::new(None),
        }
    }

    fn temp_sensor(id_suffix: &str, name: &str, value: f32) -> Sensor {
        Sensor {
            id: format!("{STABLE_ID}_{id_suffix}"),
            name: name.to_string(),
            value: value as f64,
            unit: SensorUnit::Celsius,
            sensor_type: SensorType::Temperature,
            visibility: Default::default(),
        }
    }

    /// Read every temperature register and build the sensor list.
    fn read_sensors(bus: &AmdSmnBus, model: u8) -> Vec<Sensor> {
        let mut sensors = Vec::new();

        // Package Tctl/Tdie.
        match bus.read_smn(ryzen::F17H_M01H_THM_TCON_CUR_TMP) {
            Ok(raw) => {
                let t = ryzen::decode_tctl_tdie(raw);
                if (-55.0..=155.0).contains(&t) {
                    sensors.push(Self::temp_sensor("tctl_tdie", "Core (Tctl/Tdie)", t));
                }
            }
            Err(e) => log::trace!("[AMD CPU] THM_TCON_CUR_TMP read failed: {e}"),
        }

        // Per-CCD Tdie.
        if ryzen::supports_per_ccd(model) {
            let base = ryzen::ccd_temp_base(model);
            let mut ccd_temps = Vec::new();
            for i in 0..ryzen::MAX_CCDS {
                let raw = match bus.read_smn(base + i * 4) {
                    Ok(v) => v,
                    Err(e) => {
                        log::trace!("[AMD CPU] CCD{} temp read failed: {e}", i + 1);
                        continue;
                    }
                };
                if let Some(t) = ryzen::decode_ccd_temp(raw) {
                    ccd_temps.push(t);
                    sensors.push(Self::temp_sensor(
                        &format!("ccd{}", i + 1),
                        &format!("CCD{} (Tdie)", i + 1),
                        t,
                    ));
                }
            }

            if let Some((max, avg)) = ryzen::ccd_aggregate(&ccd_temps) {
                sensors.push(Self::temp_sensor("ccds_max", "CCDs Max (Tdie)", max));
                sensors.push(Self::temp_sensor("ccds_avg", "CCDs Average (Tdie)", avg));
            }
        }

        sensors
    }
}

#[async_trait]
impl Device for AmdCpuSensorDevice {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn vendor(&self) -> &str {
        "AMD"
    }
    fn model(&self) -> &str {
        &self.name
    }

    async fn initialize(&self) -> Result<bool> {
        let bus = Arc::clone(&self.bus);
        let model = self.model;
        let cached = Arc::clone(&self.cached_sensors);

        // Prime the cache so sensors are available immediately.
        *cached.lock().await = {
            let bus = Arc::clone(&bus);
            tokio::task::spawn_blocking(move || AmdCpuSensorDevice::read_sensors(&bus, model))
                .await
                .unwrap_or_default()
        };

        let handle = tokio::task::spawn(async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                let bus2 = Arc::clone(&bus);
                let sensors = tokio::task::spawn_blocking(move || {
                    AmdCpuSensorDevice::read_sensors(&bus2, model)
                })
                .await
                .unwrap_or_default();
                *cached.lock().await = sensors;
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(handle));
        log::info!(
            "[AmdCpuSensorDevice] Initialized: {} (family=0x{:02X}, model=0x{:02X})",
            self.name,
            self.family,
            self.model
        );
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
        Some("amd_smn")
    }
}

#[async_trait]
impl SensorCapability for AmdCpuSensorDevice {
    async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        Ok(self.cached_sensors.lock().await.clone())
    }
}
