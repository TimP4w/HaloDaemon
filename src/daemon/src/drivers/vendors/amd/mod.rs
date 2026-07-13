// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "windows")]

//! AMD CPU support — Ryzen (Zen, family 17h/19h/1Ah) on-die thermal sensors.
//!
//! Detects the CPU via CPUID; if it is a supported AMD Zen part, opens the
//! broker's typed SMN service and registers one temperature-sensor device.
//! Windows only; PawnIO and the elevated broker are required.

pub mod devices;
pub mod protocols;

use std::sync::Arc;

use crate::drivers::transports::amd_smn::AmdSmnBus;
use crate::drivers::vendors::amd::devices::cpu_sensor::AmdCpuSensorDevice;
use crate::drivers::vendors::amd::protocols::ryzen;
use crate::state::AppState;

inventory::submit!(crate::registry::discovery::TransportScanner {
    name: "AMD CPU",
    platform: Some("windows"),
    scan: |app| Box::pin(async move {
        if let Err(e) = discover(app).await {
            log::error!("AMD CPU discovery failed: {e}");
        }
    }),
});

async fn discover(app: Arc<AppState>) -> anyhow::Result<()> {
    // Broker/PawnIO calls are blocking Win32 I/O. Keep the scanner future
    // yieldable so the discovery driver's outer timeout can actually fire and
    // later transports (notably HID) still run if the kernel driver stalls.
    let detected = tokio::task::spawn_blocking(|| -> anyhow::Result<_> {
        let Some((family, model)) = ryzen::detect_amd_zen() else {
            log::debug!("[AMD CPU] no supported AMD Zen CPU detected");
            return Ok(None);
        };

        let bus = match AmdSmnBus::open(None) {
            Ok(bus) => Arc::new(bus),
            Err(e) => {
                log::debug!("[AMD CPU] AMDFamily17.bin not available: {e}");
                return Ok(None);
            }
        };

        // Confirm the SMN path actually responds before registering a device
        // that would otherwise report nothing.
        if let Err(e) = bus.read_smn(ryzen::F17H_M01H_THM_TCON_CUR_TMP) {
            log::debug!("[AMD CPU] SMN probe read failed, skipping: {e}");
            return Ok(None);
        }
        Ok(Some((bus, family, model)))
    })
    .await?;

    let Some((bus, family, model)) = detected? else {
        return Ok(());
    };

    log::info!(
        "[AMD CPU] detected AMD Zen CPU (family=0x{family:02X}, model=0x{model:02X}, {})",
        ryzen::arch_label(family)
    );

    let device: Arc<dyn crate::drivers::Device> =
        Arc::new(AmdCpuSensorDevice::new(bus, family, model));
    crate::registry::usecases::registration::register_device(&app, device).await;
    Ok(())
}
