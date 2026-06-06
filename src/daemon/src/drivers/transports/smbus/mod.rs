// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project
// Reference: OpenRGB ENE SMBus implementation
// https://gitlab.com/CalcProgrammer1/OpenRGB/-/blob/master/Controllers/ENESMBusController/ENESMBusInterface/ENESMBusInterface_i2c_smbus.cpp
// SPD reference: https://gitlab.com/CalcProgrammer1/OpenRGB/-/blob/master/Controllers/ENESMBusController/ENESMBusController.cpp
//
// Platform backends live in sibling files, each selected by `cfg`:
//   - `linux.rs`          — i2c-dev ioctl interface
//   - `windows/chipset.rs` — PawnIO chipset SMBus
//   - `windows/nvapi.rs`   — NvAPI GPU i2c
//   - `fallback.rs`       — unsupported platforms
// Every backend exposes the same four items consumed below: `SmBusInner`,
// `enumerate_buses`, `enumerate_gpu_buses`, `open_device`.

use anyhow::{anyhow, Result};
use std::sync::{Arc, Mutex};

use crate::{
    discovery::{DiscoveryHandle, SmBusScanEntry, TransportScanner},
    state::AppState,
};

// ── SMBus inventory descriptor ────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub enum SmbusBusKind {
    Chipset,
    Gpu,
}

// ── Bus metadata ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct BusInfo {
    pub bus_number: u8,
    pub adapter_name: String,
    pub pci_vendor: u16,
    pub pci_device: u16,
    pub pci_sub_vendor: u16,
    pub pci_sub_device: u16,
}

impl BusInfo {
    pub fn is_gpu_bus(&self) -> bool {
        is_gpu_adapter_name(&self.adapter_name)
    }
}

fn is_gpu_adapter_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("nvidia") || lower.contains("amd radeon") || lower.contains("radeon")
}

// ── SmBusDevice ───────────────────────────────────────────────────────────────

pub struct SmBusDevice {
    pub bus_number: u8,
    inner: Arc<Mutex<SmBusInner>>,
}

impl SmBusDevice {
    pub fn open(info: &BusInfo) -> Result<Arc<Self>> {
        let inner = platform::open_device(info)?;
        Ok(Arc::new(Self {
            bus_number: info.bus_number,
            inner: Arc::new(Mutex::new(inner)),
        }))
    }

    /// Run a batch of synchronous SMBus operations in a single `spawn_blocking` call.
    /// Use this instead of multiple individual async calls whenever you need to send
    /// several register writes or reads in sequence — it avoids N round-trips through
    /// the Tokio executor and is critical for maintaining animation frame rates.
    pub async fn run_batch<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut dyn SmBusSyncOps) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut g = inner.lock().map_err(|_| anyhow!("smbus lock poisoned"))?;
            f(&mut *g)
        })
        .await?
    }
}

// ── Platform implementation ───────────────────────────────────────────────────

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;

#[cfg(target_os = "windows")]
#[path = "windows/mod.rs"]
mod platform;

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
#[path = "fallback.rs"]
mod platform;

use platform::SmBusInner;

// ── Shared SMBus constants ────────────────────────────────────────────────────

/// Maximum payload length for a single SMBus block transfer (I2C_SMBUS_BLOCK_MAX).
/// Both the Linux i2c-dev and Windows PawnIO/NvAPI backends enforce this limit.
pub(super) const SMBUS_BLOCK_MAX: usize = 32;

// ── Sync batch ops trait ──────────────────────────────────────────────────────

/// Synchronous SMBus operations, available inside a `SmBusDevice::run_batch` closure.
/// Running multiple ops through this interface within one `run_batch` call avoids
/// the per-call `spawn_blocking` overhead that would otherwise cap animation FPS.
pub trait SmBusSyncOps {
    fn read_byte(&mut self, addr: u8) -> Result<u8>;
    fn read_byte_data(&mut self, addr: u8, cmd: u8) -> Result<u8>;
    fn write_quick(&mut self, addr: u8) -> Result<bool>;
    fn write_byte_data(&mut self, addr: u8, cmd: u8, val: u8) -> Result<()>;
    fn write_word_data(&mut self, addr: u8, cmd: u8, val: u16) -> Result<()>;
    fn write_block_data(&mut self, addr: u8, cmd: u8, data: &[u8]) -> Result<()>;
    /// Returns `false` if this backend does not support block writes.
    /// Callers can check this statically to avoid attempting a block write
    /// that will always return a runtime error (e.g. NvAPI GPU buses).
    fn supports_block_write(&self) -> bool {
        true
    }
}

// ── Discovery ─────────────────────────────────────────────────────────────────

pub struct SmBusTransport;

impl SmBusTransport {
    /// Enumerate every chipset and GPU SMBus controller the platform exposes,
    /// without opening any of them. Returns `(chipset_buses, gpu_buses)`. Used
    /// by the debug usecase to surface the "PawnIO loaded but no DRAM RGB"
    /// failure mode in the settings panel.
    pub async fn enumerate_for_debug() -> (Vec<BusInfo>, Vec<BusInfo>) {
        tokio::task::spawn_blocking(|| {
            (platform::enumerate_buses(), platform::enumerate_gpu_buses())
        })
        .await
        .unwrap_or_default()
    }
}

async fn discover(app: Arc<AppState>) -> Result<()> {
    let (chipset_buses, gpu_buses) = tokio::task::spawn_blocking(|| {
        (platform::enumerate_buses(), platform::enumerate_gpu_buses())
    })
    .await?;

    // A failure to open a bus is non-fatal, but the first one is worth a
    // visible warning — it usually means a missing PawnIO SMBus module or
    // a lack of privileges, which silently hides DRAM / chipset RGB.
    let mut open_warned = false;

    for entry in inventory::iter::<SmBusScanEntry>() {
        let buses: Vec<&BusInfo> = match entry.bus_kind {
            SmbusBusKind::Chipset => chipset_buses.iter().filter(|b| !b.is_gpu_bus()).collect(),
            SmbusBusKind::Gpu => gpu_buses.iter().collect(),
        };
        log::debug!(
            "[SmBusTransport] {:?}: {} bus(es) to scan",
            entry.bus_kind,
            buses.len()
        );

        for bus_info in buses {
            let bus = match SmBusDevice::open(bus_info) {
                Ok(b) => b,
                Err(e) => {
                    if open_warned {
                        log::debug!(
                            "[SmBusTransport] Cannot open bus {}: {}",
                            bus_info.bus_number,
                            e
                        );
                    } else {
                        log::warn!(
                            "[SmBusTransport] cannot open SMBus bus {}: {} \
                             — SMBus RGB devices (e.g. DRAM) on this bus \
                             will be unavailable",
                            bus_info.bus_number,
                            e
                        );
                        open_warned = true;
                    }
                    continue;
                }
            };

            if let Some(pre_scan) = entry.pre_scan {
                if let Err(e) = pre_scan(Arc::clone(&bus)).await {
                    log::debug!(
                        "[SmBusTransport] pre_scan failed on bus {}: {}",
                        bus_info.bus_number,
                        e
                    );
                }
            }

            for &addr in entry.addresses {
                crate::discovery::discover_handle(
                    &app,
                    DiscoveryHandle::Smbus {
                        bus: &bus,
                        addr,
                        bus_kind: entry.bus_kind,
                    },
                )
                .await;
            }
        }
    }

    Ok(())
}

inventory::submit!(TransportScanner {
    name: "SMBus",
    platform: None,
    scan: |app| Box::pin(async move {
        if let Err(e) = discover(app).await {
            log::error!("SMBus discovery failed: {e}");
        }
    }),
});
