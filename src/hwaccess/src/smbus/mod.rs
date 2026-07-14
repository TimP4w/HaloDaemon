// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project
// Reference: OpenRGB ENE SMBus implementation
// https://gitlab.com/CalcProgrammer1/OpenRGB/-/blob/master/Controllers/ENESMBusController/ENESMBusInterface/ENESMBusInterface_i2c_smbus.cpp
//
//! Raw SMBus register primitives.
//!
//! [`SmBusSyncOps`] is the seam between the daemon (which owns `SmBusDevice`,
//! metering, discovery and every device driver) and the privileged backend
//! (in-process here, or served by the broker over RPC). Platform backends live
//! in sibling files, each selected by `cfg`:
//!   - `linux.rs`          — i2c-dev ioctl interface
//!   - `windows/chipset.rs` — PawnIO chipset SMBus
//!   - `windows/nvapi.rs`   — NvAPI GPU i2c
//!   - `fallback.rs`       — unsupported platforms
//! Every backend exposes the same four items consumed below: `SmBusInner`,
//! `enumerate_buses`, `enumerate_gpu_buses`, `open_device`.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Identity and PCI ids of one enumerated SMBus controller. `Serialize` so a
/// [`crate::proto`] RPC can carry it from the broker's enumeration to the
/// daemon; `PartialEq`/`Eq` so the proto round-trip is property-testable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

pub(crate) fn is_gpu_adapter_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("nvidia") || lower.contains("amd radeon") || lower.contains("radeon")
}

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;

#[cfg(target_os = "windows")]
#[path = "windows/mod.rs"]
mod platform;

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
#[path = "fallback.rs"]
mod platform;

/// Maximum payload length for a single SMBus block transfer (I2C_SMBUS_BLOCK_MAX).
pub const SMBUS_BLOCK_MAX: usize = 32;

/// Synchronous SMBus operations. The daemon's `SmBusDevice` runs a batch of
/// these against a `&mut dyn SmBusSyncOps` — whether that is a direct
/// in-process backend or an RPC client to the broker.
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

/// Enumerate every chipset SMBus controller without opening any.
pub fn enumerate_buses() -> Vec<BusInfo> {
    platform::enumerate_buses()
}

/// Enumerate every GPU SMBus/i2c controller without opening any.
pub fn enumerate_gpu_buses() -> Vec<BusInfo> {
    platform::enumerate_gpu_buses()
}

/// Open the register bus described by `info`, returning a boxed backend the
/// daemon can meter and lock behind its `SmBusDevice`.
pub fn open_bus(info: &BusInfo) -> Result<Box<dyn SmBusSyncOps + Send>> {
    Ok(Box::new(platform::open_device(info)?))
}

#[cfg(test)]
mod tests {
    use super::is_gpu_adapter_name;

    #[test]
    fn is_gpu_adapter_name_nvidia() {
        assert!(is_gpu_adapter_name("NVIDIA GeForce RTX 4090"));
        assert!(is_gpu_adapter_name("nvidia display"));
    }

    #[test]
    fn is_gpu_adapter_name_amd_radeon() {
        assert!(is_gpu_adapter_name("AMD Radeon RX 7900 XTX"));
        assert!(is_gpu_adapter_name("Radeon Graphics"));
        assert!(is_gpu_adapter_name("radeon rx 580"));
    }

    #[test]
    fn is_gpu_adapter_name_non_gpu() {
        assert!(!is_gpu_adapter_name("Intel SMBus"));
        assert!(!is_gpu_adapter_name("i801 SMBus"));
        assert!(!is_gpu_adapter_name(""));
        assert!(!is_gpu_adapter_name("Piix4 SMBus"));
    }
}
