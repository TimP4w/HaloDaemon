//! Windows SMBus backend.
//!
//! Two transports sit behind this facade:
//!   - chipset SMBus controllers (DRAM RGB, …) via the PawnIO kernel driver
//!     (`chipset.rs`) — the same approach OpenRGB uses;
//!   - NVIDIA GPU i2c buses via `nvapi64.dll` (`nvapi.rs`).
//!
//! `SmBusInner` dispatches each operation to whichever backend opened the bus.
//!
//! Runtime requirements:
//!   - PawnIO installed, with `SmbusI801.bin` (Intel) / `SmbusPIIX4.bin` (AMD)
//!     placed next to the executable.
//!   - The process running with Administrator privileges.

use super::*;

mod chipset;
mod nvapi;

// Shared SMBus protocol constants (match Linux i2c-dev / OpenRGB).
pub(super) const SMBUS_WRITE: u64 = 0;
pub(super) const SMBUS_READ: u64 = 1;
pub(super) const SMBUS_QUICK: u64 = 0;
pub(super) const SMBUS_BYTE: u64 = 1;
pub(super) const SMBUS_BYTE_DATA: u64 = 2;
pub(super) const SMBUS_WORD_DATA: u64 = 3;
pub(super) const SMBUS_BLOCK_DATA: u64 = 5;
pub(super) use super::SMBUS_BLOCK_MAX;

macro_rules! delegate {
    ($method:ident, $($arg:ident: $ty:ty),+ => $ret:ty) => {
        fn $method(&mut self, $($arg: $ty),+) -> $ret {
            match self {
                Self::Chipset(b) => b.$method($($arg),+),
                Self::Gpu(b) => b.$method($($arg),+),
            }
        }
    };
}

pub(super) enum SmBusInner {
    Chipset(chipset::ChipsetBus),
    Gpu(nvapi::GpuBus),
}

impl SmBusSyncOps for SmBusInner {
    fn write_block_data(&mut self, addr: u8, cmd: u8, data: &[u8]) -> Result<()> {
        match self {
            Self::Chipset(b) => b.write_block_data(addr, cmd, data),
            Self::Gpu(_) => Err(anyhow!(
                "write_block_data is not supported on NVIDIA GPU SMBus (NvAPI)"
            )),
        }
    }
    fn supports_block_write(&self) -> bool {
        matches!(self, Self::Chipset(_))
    }

    delegate!(read_byte, addr: u8 => Result<u8>);
    delegate!(read_byte_data, addr: u8, cmd: u8 => Result<u8>);
    delegate!(write_quick, addr: u8 => Result<bool>);
    delegate!(write_byte_data, addr: u8, cmd: u8, val: u8 => Result<()>);
    delegate!(write_word_data, addr: u8, cmd: u8, val: u16 => Result<()>);
}

pub fn enumerate_buses() -> Vec<BusInfo> {
    chipset::enumerate_buses()
}

pub fn enumerate_gpu_buses() -> Vec<BusInfo> {
    nvapi::enumerate_gpu_buses()
}

pub fn open_device(info: &BusInfo) -> Result<SmBusInner> {
    if info.is_gpu_bus() {
        Ok(SmBusInner::Gpu(nvapi::GpuBus::open(info)?))
    } else {
        Ok(SmBusInner::Chipset(chipset::ChipsetBus::open(
            info.bus_number,
            info.pci_vendor,
        )?))
    }
}
