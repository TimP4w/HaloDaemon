#![cfg(target_os = "windows")]

//! One PWM fan header on a detected SuperIO chip, exposing a `FanCapability` and `FanEngineSlot`.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as TokioMutex;

use crate::drivers::transports::lpcio::LpcIoBus;
use crate::drivers::vendors::generic::devices::common::TaskHandle;
use crate::drivers::vendors::generic::devices::superio::DetectedChip;
use crate::drivers::{CapabilityRef, Device, FanCapability, FanStateSlot};
use halod_shared::types::DeviceType;

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

pub struct SuperIoFanDevice {
    chip: DetectedChip,
    channel: u8,
    bus: Arc<Mutex<LpcIoBus>>,
    stable_id: String,
    id: String,
    label: String,
    cached_rpm: Arc<TokioMutex<u32>>,
    cached_duty: Arc<TokioMutex<u8>>,
    saved_mode: Mutex<Option<u8>>,
    fan: FanStateSlot,
    poll_task: TokioMutex<Option<TaskHandle>>,
}

impl SuperIoFanDevice {
    pub fn new(chip: DetectedChip, channel: u8, label: String, bus: Arc<Mutex<LpcIoBus>>) -> Self {
        let chip_id = format!(
            "{}_0x{:02x}",
            chip.name().to_lowercase().replace(' ', "_"),
            chip.probe_port()
        );
        let stable_id = format!("{}_fan{}", chip_id, channel + 1);
        let id = format!("superio_fan_{}", stable_id);
        Self {
            chip,
            channel,
            bus,
            stable_id,
            id,
            label,
            cached_rpm: Arc::new(TokioMutex::new(0)),
            cached_duty: Arc::new(TokioMutex::new(0)),
            saved_mode: Mutex::new(None),
            fan: FanStateSlot::default(),
            poll_task: TokioMutex::new(None),
        }
    }
}

#[async_trait]
impl Device for SuperIoFanDevice {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.label
    }
    fn vendor(&self) -> &str {
        "SuperIO"
    }
    fn model(&self) -> &str {
        self.chip.name()
    }

    async fn initialize(&self) -> Result<bool> {
        {
            let bus = self.bus.lock().unwrap();
            if let Err(e) = self.chip.keep_io_unlocked(&bus) {
                log::warn!("[SuperIoFanDevice] keep_io_unlocked failed at init: {e}");
            }
            *self.saved_mode.lock().unwrap() = self.chip.read_ctrl_mode(&bus, self.channel);
        }

        let chip = self.chip;
        let ch = self.channel;
        let bus = Arc::clone(&self.bus);
        let cached_rpm = Arc::clone(&self.cached_rpm);
        let cached_duty = Arc::clone(&self.cached_duty);

        let handle = tokio::task::spawn(async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                let bus2 = Arc::clone(&bus);
                let (rpm, duty) = tokio::task::spawn_blocking(move || {
                    let bus = bus2.lock().unwrap();
                    if let Err(e) = chip.keep_io_unlocked(&bus) {
                        log::trace!("[NCT677x] keep_io_unlocked failed: {e}");
                    }
                    (chip.read_rpm(&bus, ch), chip.read_duty(&bus, ch))
                })
                .await
                .unwrap_or((0, 0));
                *cached_rpm.lock().await = rpm;
                *cached_duty.lock().await = duty;
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(handle));
        log::info!(
            "[SuperIoFanDevice] Initialized: {} ch{} (base=0x{:04X})",
            self.chip.name(),
            self.channel,
            self.chip.hwm_base()
        );
        Ok(true)
    }

    async fn close(&self) {
        self.poll_task.lock().await.take();
        if let Some(mode) = *self.saved_mode.lock().unwrap() {
            let bus = self.bus.lock().unwrap();
            let _ = self.chip.keep_io_unlocked(&bus);
            self.chip.restore_ctrl_mode(&bus, self.channel, mode);
        }
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Fan
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        if self.chip.hwm_base() != 0 {
            vec![CapabilityRef::Fan(self)]
        } else {
            vec![]
        }
    }
    fn debug_transport(&self) -> Option<&'static str> {
        Some("superio")
    }

    fn write_rate_status(&self) -> Option<halod_shared::types::WriteRateStatus> {
        Some(self.bus.lock().unwrap().rate_status())
    }
}

#[async_trait]
impl FanCapability for SuperIoFanDevice {
    fn fan_channel_id(&self) -> u8 {
        self.channel
    }

    async fn fan_controllable(&self) -> bool {
        self.chip.hwm_base() != 0
    }

    async fn get_duty(&self) -> Result<u8> {
        Ok(*self.cached_duty.lock().await)
    }

    async fn set_duty(&self, duty: u8) -> Result<()> {
        {
            let bus = self.bus.lock().unwrap();
            self.chip.set_duty(&bus, self.channel, duty)?;
        }
        *self.cached_duty.lock().await = duty;
        Ok(())
    }

    async fn get_rpm(&self) -> Option<u32> {
        Some(*self.cached_rpm.lock().await)
    }

    fn fan_state(&self) -> &FanStateSlot {
        &self.fan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::vendors::generic::devices::superio::nct677x::{Detected, Nct677xVariant};

    #[test]
    fn stable_id_format() {
        let chip = DetectedChip::Nct677x(Detected {
            probe_port: 0x2E,
            variant: Nct677xVariant::Nct6796D,
            hwm_base: 0x0290,
        });
        let chip_id = format!("{}_0x{:02x}", chip.name().to_lowercase(), chip.probe_port());
        let stable_id = format!("{}_fan{}", chip_id, 1u8 + 1);
        assert!(stable_id.contains("nct6796d"));
        assert!(stable_id.contains("fan2"));
    }
}
