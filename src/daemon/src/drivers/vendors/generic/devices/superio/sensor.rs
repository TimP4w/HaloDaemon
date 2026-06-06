#![cfg(target_os = "windows")]

//! Temperature-sensor device for a detected SuperIO chip. Polls the chip's
//! basic temperature registers every second and exposes them as a single
//! sensor-capability device.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as TokioMutex;

use crate::drivers::vendors::generic::devices::common::TaskHandle;
use crate::drivers::vendors::generic::devices::superio::{nct677x, DetectedChip};
use crate::drivers::transports::lpcio::LpcIoBus;
use crate::drivers::{CapabilityRef, Device, SensorCapability};
use halod_protocol::types::{
    DeviceType, Sensor, SensorType, SensorUnit,
};

pub struct SuperIoSensorDevice {
    chip: DetectedChip,
    bus: Arc<Mutex<LpcIoBus>>,
    stable_id: String,
    cached_sensors: Arc<TokioMutex<Vec<Sensor>>>,
    poll_task: TokioMutex<Option<TaskHandle>>,
}

impl SuperIoSensorDevice {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

    pub fn new(chip: DetectedChip, bus: Arc<Mutex<LpcIoBus>>) -> Self {
        let stable_id = format!(
            "superio_{}_0x{:02x}",
            chip.name().to_lowercase().replace(' ', "_"),
            chip.probe_port()
        );
        Self {
            chip,
            bus,
            stable_id,
            cached_sensors: Arc::new(TokioMutex::new(vec![])),
            poll_task: TokioMutex::new(None),
        }
    }

    fn read_sensors(bus: &LpcIoBus, chip: DetectedChip, stable_id: &str) -> Vec<Sensor> {
        // Some Nuvoton chips re-engage their HWM I/O lock between polls;
        // re-clear it at the start of each cycle.
        let DetectedChip::Nct677x(d) = chip;
        if let Err(e) = nct677x::keep_io_unlocked(bus, d) {
            log::trace!("[SuperIO sensor] keep_io_unlocked failed: {e}");
        }
        Self::read_sensors_nct(bus, d, stable_id)
    }

    fn read_sensors_nct(
        bus: &LpcIoBus,
        chip: nct677x::Detected,
        stable_id: &str,
    ) -> Vec<Sensor> {
        nct677x::read_all_temperatures(bus, chip)
            .into_iter()
            .map(|r| Sensor {
                // Stable ID keyed by source byte — survives BIOS remapping
                // because the source is what's physically being measured.
                id: format!("{}_src{}", stable_id, r.source),
                name: r.label.to_string(),
                value: r.temperature_c as f64,
                unit: SensorUnit::Celsius,
                sensor_type: SensorType::Temperature,
                visibility: Default::default(),
            })
            .collect()
    }
}

#[async_trait]
impl Device for SuperIoSensorDevice {
    fn id(&self) -> String {
        format!("superio_sensor_{}", self.stable_id)
    }
    fn name(&self) -> &str {
        self.chip.name()
    }
    fn vendor(&self) -> &str {
        "SuperIO"
    }
    fn model(&self) -> &str {
        self.chip.name()
    }

    async fn initialize(&self) -> Result<bool> {
        let chip = self.chip;
        let bus = Arc::clone(&self.bus);
        let cached = Arc::clone(&self.cached_sensors);
        let stable_id = self.stable_id.clone();

        let handle = tokio::task::spawn(async move {
            loop {
                tokio::time::sleep(SuperIoSensorDevice::POLL_INTERVAL).await;
                let sensors = {
                    let bus = bus.lock().unwrap();
                    SuperIoSensorDevice::read_sensors(&bus, chip, &stable_id)
                };
                *cached.lock().await = sensors;
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(handle));
        log::info!(
            "[SuperIoSensorDevice] Initialized: {} (base=0x{:04X})",
            self.chip.name(),
            self.chip.hwm_base()
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
        Some("superio")
    }
}

#[async_trait]
impl SensorCapability for SuperIoSensorDevice {
    async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        Ok(self.cached_sensors.lock().await.clone())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn stable_id_format() {
        let id = format!(
            "superio_{}_0x{:02x}",
            "nct6796dr".to_lowercase(),
            0x2Eu16
        );
        assert_eq!(id, "superio_nct6796dr_0x2e");
    }
}
