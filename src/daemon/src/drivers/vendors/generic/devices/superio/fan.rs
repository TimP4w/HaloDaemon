#![cfg(target_os = "windows")]

//! One PWM fan header on a detected SuperIO chip. Polls RPM + current duty
//! every second, exposes a `FanCapability` for direct duty writes, and a
//! `FanEngineSlot` for fan-curve engine integration. Saves the chip's
//! original control-mode byte at init and restores it on `close()`.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as TokioMutex;

use crate::drivers::vendors::generic::devices::common::TaskHandle;
use crate::drivers::vendors::generic::devices::superio::{nct677x, DetectedChip};
use crate::drivers::transports::lpcio::LpcIoBus;
use crate::drivers::{CapabilityRef, Device, FanCapability, FanStateSlot};
use halod_protocol::types::DeviceType;

pub struct SuperIoFanDevice {
    chip: DetectedChip,
    channel: u8,
    bus: Arc<Mutex<LpcIoBus>>,
    stable_id: String,
    label: String,
    cached_rpm: Arc<TokioMutex<u32>>,
    cached_duty: Arc<TokioMutex<u8>>,
    saved_mode: Mutex<Option<u8>>,
    fan: FanStateSlot,
    poll_task: TokioMutex<Option<TaskHandle>>,
}

impl SuperIoFanDevice {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

    pub fn new(
        chip: DetectedChip,
        channel: u8,
        label: String,
        bus: Arc<Mutex<LpcIoBus>>,
    ) -> Self {
        let chip_id = format!(
            "{}_0x{:02x}",
            chip.name().to_lowercase().replace(' ', "_"),
            chip.probe_port()
        );
        let stable_id = format!("{}_fan{}", chip_id, channel + 1);
        Self {
            chip,
            channel,
            bus,
            stable_id,
            label,
            cached_rpm: Arc::new(TokioMutex::new(0)),
            cached_duty: Arc::new(TokioMutex::new(0)),
            saved_mode: Mutex::new(None),
            fan: FanStateSlot::default(),
            poll_task: TokioMutex::new(None),
        }
    }

    fn read_rpm(bus: &LpcIoBus, chip: DetectedChip, ch: u8) -> u32 {
        let DetectedChip::Nct677x(d) = chip;
        Self::read_rpm_nct(bus, d, ch)
    }

    fn read_rpm_nct(bus: &LpcIoBus, chip: nct677x::Detected, ch: u8) -> u32 {
        if chip.hwm_base == 0 {
            return 0;
        }
        let regs = chip.variant.fan_count_regs();
        let Some(&reg) = regs.get(ch as usize) else {
            return 0;
        };
        // High byte at `reg`, low byte at `reg + 1`.
        let high = match nct677x::read_hwm(bus, chip.hwm_base, reg) {
            Ok(v) => v,
            Err(e) => {
                log::trace!("[NCT677x] read fan-count high (reg=0x{reg:03X}) failed: {e}");
                return 0;
            }
        };
        let low = match nct677x::read_hwm(bus, chip.hwm_base, reg + 1) {
            Ok(v) => v,
            Err(e) => {
                log::trace!("[NCT677x] read fan-count low (reg=0x{:03X}) failed: {e}", reg + 1);
                return 0;
            }
        };
        nct677x::rpm_from_count_bytes(high, low)
    }

    fn read_duty(bus: &LpcIoBus, chip: DetectedChip, ch: u8) -> u8 {
        let DetectedChip::Nct677x(d) = chip;
        Self::read_duty_nct(bus, d, ch)
    }

    fn read_duty_nct(bus: &LpcIoBus, chip: nct677x::Detected, ch: u8) -> u8 {
        if chip.hwm_base == 0 {
            return 0;
        }
        let regs = chip.variant.fan_pwm_out_regs();
        let Some(&reg) = regs.get(ch as usize) else {
            return 0;
        };
        let raw = nct677x::read_hwm(bus, chip.hwm_base, reg).unwrap_or(0) as u32;
        (raw * 100 / 255) as u8
    }
}

#[async_trait]
impl Device for SuperIoFanDevice {
    fn id(&self) -> String {
        format!("superio_fan_{}", self.stable_id)
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
            let DetectedChip::Nct677x(d) = self.chip;
            let _ = nct677x::keep_io_unlocked(&bus, d);
            let saved = if d.hwm_base != 0 {
                d.variant
                    .fan_ctrl_mode_regs()
                    .get(self.channel as usize)
                    .and_then(|&r| nct677x::read_hwm(&bus, d.hwm_base, r).ok())
            } else {
                None
            };
            *self.saved_mode.lock().unwrap() = saved;
        }

        let chip = self.chip;
        let ch = self.channel;
        let bus = Arc::clone(&self.bus);
        let cached_rpm = Arc::clone(&self.cached_rpm);
        let cached_duty = Arc::clone(&self.cached_duty);

        let handle = tokio::task::spawn(async move {
            loop {
                tokio::time::sleep(SuperIoFanDevice::POLL_INTERVAL).await;
                let (rpm, duty) = {
                    let bus = bus.lock().unwrap();
                    // Re-open the HWM space if the chip re-engaged its
                    // I/O lock since the last cycle.
                    let DetectedChip::Nct677x(d) = chip;
                    if let Err(e) = nct677x::keep_io_unlocked(&bus, d) {
                        log::trace!("[NCT677x] keep_io_unlocked failed: {e}");
                    }
                    (
                        SuperIoFanDevice::read_rpm(&bus, chip, ch),
                        SuperIoFanDevice::read_duty(&bus, chip, ch),
                    )
                };
                *cached_rpm.lock().await = rpm;
                *cached_duty.lock().await = duty;
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(handle));
        log::info!(
            "[SuperIoFanDevice] Initialized: {} ch{} (base=0x{:04X})",
            self.chip.name(), self.channel, self.chip.hwm_base()
        );
        Ok(true)
    }

    async fn close(&self) {
        self.poll_task.lock().await.take();
        if let Some(mode) = *self.saved_mode.lock().unwrap() {
            let bus = self.bus.lock().unwrap();
            let DetectedChip::Nct677x(d) = self.chip;
            let _ = nct677x::keep_io_unlocked(&bus, d);
            if d.hwm_base != 0 {
                if let Some(&reg) = d.variant.fan_ctrl_mode_regs().get(self.channel as usize) {
                    let _ = nct677x::write_hwm(&bus, d.hwm_base, reg, mode);
                }
            }
        }
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Fan
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        if self.chip.hwm_base() != 0 { vec![CapabilityRef::Fan(self)] } else { vec![] }
    }
    fn debug_transport(&self) -> Option<&'static str> {
        Some("superio")
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
            let DetectedChip::Nct677x(d) = self.chip;
            nct677x::keep_io_unlocked(&bus, d)?;
            if d.hwm_base != 0 {
                let mode_regs = d.variant.fan_ctrl_mode_regs();
                let cmd_regs = d.variant.fan_pwm_cmd_regs();
                let ch = self.channel as usize;
                if let (Some(&mode_reg), Some(&cmd_reg)) =
                    (mode_regs.get(ch), cmd_regs.get(ch))
                {
                    // 0 = manual control
                    nct677x::write_hwm(&bus, d.hwm_base, mode_reg, 0)?;
                    nct677x::write_hwm(
                        &bus,
                        d.hwm_base,
                        cmd_reg,
                        (duty as u16 * 255 / 100) as u8,
                    )?;
                }
            }
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
