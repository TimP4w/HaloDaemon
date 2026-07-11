// SPDX-License-Identifier: GPL-3.0-or-later
//! PawnIO chipset SMBus transport.
//!
//! Chipset SMBus controllers (Intel i801, AMD FCH/PIIX4) are driven through the
//! PawnIO kernel driver (<https://pawnio.eu/>). PawnIO plumbing (DLL loading,
//! blob caching, ioctl dispatch) lives in [`crate::pawnio`].

use anyhow::{anyhow, Result};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

/// Scope guard that releases a Win32 mutex on drop, including panics/unwinds.
struct MutexGuard(HANDLE);

impl MutexGuard {
    fn acquire(mutex: HANDLE, timeout_ms: u32) -> Result<Self> {
        let wait = unsafe { WaitForSingleObject(mutex, timeout_ms) };
        // WAIT_ABANDONED: a previous owner died holding the mutex; treat as success.
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            return Err(anyhow!("SMBus mutex acquire failed/timed out"));
        }
        Ok(Self(mutex))
    }
}

impl Drop for MutexGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
        };
    }
}

use crate::pawnio::PawnioModule;
use crate::smbus::BusInfo;

use super::{
    SMBUS_BLOCK_DATA, SMBUS_BLOCK_MAX, SMBUS_BYTE, SMBUS_BYTE_DATA, SMBUS_QUICK, SMBUS_READ,
    SMBUS_WORD_DATA, SMBUS_WRITE,
};

/// Build the PawnIO `ioctl_smbus_xfer` 9-word input array for a block write:
/// `in[0..4]` = addr/read_write/command/size, `in[4..9]` = the
/// `i2c_smbus_data.block` buffer LE-packed (`block[0]` = length, rest = data).
fn pack_block_xfer(addr: u8, command: u64, data: &[u8]) -> Result<[u64; 9]> {
    if data.is_empty() || data.len() > SMBUS_BLOCK_MAX {
        return Err(anyhow!(
            "SMBus block length {} out of range 1..={}",
            data.len(),
            SMBUS_BLOCK_MAX
        ));
    }
    let mut args = [0u64; 9];
    args[0] = addr as u64;
    args[1] = SMBUS_WRITE;
    args[2] = command;
    args[3] = SMBUS_BLOCK_DATA;
    let mut block = [0u8; 40];
    block[0] = data.len() as u8;
    block[1..1 + data.len()].copy_from_slice(data);
    for (i, slot) in args[4..9].iter_mut().enumerate() {
        *slot = u64::from_le_bytes(block[i * 8..i * 8 + 8].try_into().expect("8-byte slice"));
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_block_xfer_layout() {
        let args = pack_block_xfer(0x70, 0x03, &[0xAA, 0xBB, 0xCC]).unwrap();
        assert_eq!(args.len(), 9);
        assert_eq!(args[0..4], [0x70, SMBUS_WRITE, 0x03, SMBUS_BLOCK_DATA]);
        // in[4] = block buffer bytes [len, d0, d1, d2, 0, 0, 0, 0] little-endian.
        assert_eq!(
            args[4],
            u64::from_le_bytes([3, 0xAA, 0xBB, 0xCC, 0, 0, 0, 0])
        );
        assert_eq!(args[5..9], [0, 0, 0, 0]);
    }

    #[test]
    fn pack_block_xfer_max_len() {
        let data = [0u8; 32];
        let args = pack_block_xfer(0x71, 0x03, &data).unwrap();
        assert_eq!(args.len(), 9);
        // Length byte sits in the low byte of in[4].
        assert_eq!(args[4] & 0xFF, 32);
    }

    #[test]
    fn pack_block_xfer_rejects_empty() {
        assert!(pack_block_xfer(0x70, 0x03, &[]).is_err());
    }

    #[test]
    fn pack_block_xfer_rejects_oversize() {
        assert!(pack_block_xfer(0x70, 0x03, &[0u8; 33]).is_err());
    }

    #[test]
    fn hex_run_extracts_ven_dev_subsys_rev() {
        let id = r"PCI\VEN_8086&DEV_06A3&SUBSYS_86941043&REV_00";
        let ven = hex_run(id, "VEN_");
        let dev = hex_run(id, "DEV_");
        let subsys = hex_run(id, "SUBSYS_");
        let rev = hex_run(id, "REV_");
        assert_eq!(ven, Some("8086"));
        assert_eq!(dev, Some("06A3"));
        assert_eq!(subsys, Some("86941043"));
        assert_eq!(rev, Some("00"));
        assert_eq!(parse_hex(ven, 0, 4), 0x8086);
        assert_eq!(parse_hex(dev, 0, 4), 0x06A3);
    }

    #[test]
    fn hex_run_returns_none_for_missing_prefix() {
        assert!(hex_run("PCI\\VEN_8086", "SUBSYS_").is_none());
    }

    #[test]
    fn hex_run_handles_suffix_non_hex_chars() {
        let id = "VEN_8086&DEV_06A3";
        let ven = hex_run(id, "VEN_");
        assert_eq!(ven, Some("8086"));
    }

    #[test]
    fn parse_hex_returns_zero_when_absent() {
        assert_eq!(parse_hex(None, 0, 4), 0);
    }
}

pub(crate) struct ChipsetBus {
    module: PawnioModule,
    mutex: HANDLE,
    // PIIX4 port to select before each transfer; `None` for single-port controllers like Intel i801.
    port: Option<u64>,
    // Latched once a block transfer is rejected, so callers fall back to byte-at-a-time thereafter.
    block_unsupported: std::sync::atomic::AtomicBool,
}

// SAFETY: the Win32 mutex handle is only touched while the owning
// `Mutex<SmBusInner>` is held; `PawnioModule` already declares the equivalent
// for its own handle.
unsafe impl Send for ChipsetBus {}

impl ChipsetBus {
    pub(super) fn open(bus_number: u8, pci_vendor: u16) -> Result<Self> {
        // Intel uses SmbusI801, AMD uses SmbusPIIX4; if the vendor is unknown, try both.
        const VENDOR_INTEL: u16 = 0x8086;
        const VENDOR_AMD: u16 = 0x1022;
        let blob_names: &[&str] = match pci_vendor {
            VENDOR_AMD => &["SmbusPIIX4.bin"],
            VENDOR_INTEL => &["SmbusI801.bin"],
            _ => &["SmbusI801.bin", "SmbusPIIX4.bin"],
        };
        let module = PawnioModule::open(blob_names).map_err(|e| {
            anyhow!(
                "no matching PawnIO SMBus module loaded, place SmbusI801.bin \
                 (Intel) or SmbusPIIX4.bin (AMD) next to the executable ({e})"
            )
        })?;
        log::info!("[SmBus] loaded PawnIO module {}", module.blob_name());

        let name: Vec<u16> = "Global\\Access_SMBUS.HTP.Method\0".encode_utf16().collect();
        let mutex = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
            .map_err(|e| anyhow!("CreateMutexW failed: {e}"))?;

        // AMD FCH is multi-port and needs the port re-selected before every transfer; Intel i801 is single-port.
        let port = (pci_vendor == VENDOR_AMD).then_some(bus_number as u64);

        Ok(Self {
            module,
            mutex,
            port,
            block_unsupported: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Run one `ioctl_smbus_xfer` through PawnIO, serialised on the shared
    /// SMBus mutex. The output array is always 5 words (`out[0]` carries
    /// the result byte/word on reads); the input layout is caller-supplied
    /// — 9 words for scalar ops, `5 + block_len` for block transfers.
    fn run_xfer(&self, input: &[u64]) -> Result<[u64; 5]> {
        let _guard = MutexGuard::acquire(self.mutex, 2000)?;

        // Select the PIIX4 port (AMD multi-port) under the same mutex so it
        // cannot race a transfer targeting the other port. A failed port
        // select makes the xfer unsafe — abort before proceeding.
        if let Some(port) = self.port {
            self.module
                .exec(c"ioctl_piix4_port_sel", &[port], &mut [0u64; 1])
                .map_err(|e| anyhow!("PIIX4 port select failed: {e}"))?;
        }

        let mut out = [0u64; 5];
        self.module
            .exec(c"ioctl_smbus_xfer", input, &mut out)
            .map_err(|e| anyhow!("ioctl_smbus_xfer failed: {e}"))?;
        Ok(out)
    }

    /// Scalar SMBus transfer — 9 input words, matching OpenRGB's
    /// `i2c_smbus_pawnio.cpp` layout (`data` packed into `in[4]`).
    fn xfer(&self, addr: u8, read_write: u64, command: u64, size: u64, data: u64) -> Result<u64> {
        let args: [u64; 9] = [addr as u64, read_write, command, size, data, 0, 0, 0, 0];
        Ok(self.run_xfer(&args)?[0])
    }

    /// Block SMBus write — sends up to 32 bytes in a single transfer.
    fn xfer_block(&self, addr: u8, command: u64, data: &[u8]) -> Result<()> {
        let args = pack_block_xfer(addr, command, data)?;
        self.run_xfer(&args)?;
        Ok(())
    }

    pub(super) fn read_byte(&mut self, addr: u8) -> Result<u8> {
        Ok((self.xfer(addr, SMBUS_READ, 0, SMBUS_BYTE, 0)? & 0xFF) as u8)
    }
    pub(super) fn read_byte_data(&mut self, addr: u8, cmd: u8) -> Result<u8> {
        Ok((self.xfer(addr, SMBUS_READ, cmd as u64, SMBUS_BYTE_DATA, 0)? & 0xFF) as u8)
    }
    pub(super) fn write_quick(&mut self, addr: u8) -> Result<bool> {
        Ok(self.xfer(addr, SMBUS_WRITE, 0, SMBUS_QUICK, 0).is_ok())
    }
    pub(super) fn write_byte_data(&mut self, addr: u8, cmd: u8, val: u8) -> Result<()> {
        self.xfer(addr, SMBUS_WRITE, cmd as u64, SMBUS_BYTE_DATA, val as u64)?;
        Ok(())
    }
    pub(super) fn write_word_data(&mut self, addr: u8, cmd: u8, val: u16) -> Result<()> {
        self.xfer(addr, SMBUS_WRITE, cmd as u64, SMBUS_WORD_DATA, val as u64)?;
        Ok(())
    }
    pub(super) fn write_block_data(&mut self, addr: u8, cmd: u8, data: &[u8]) -> Result<()> {
        use std::sync::atomic::Ordering;
        if self.block_unsupported.load(Ordering::Relaxed) {
            return Err(anyhow!("SMBus block write disabled (previously rejected)"));
        }
        match self.xfer_block(addr, cmd as u64, data) {
            Ok(()) => Ok(()),
            Err(e) => {
                // ENE only ever passes 1..=32-byte chunks, so a real error
                // here means the loaded PawnIO module declined block data.
                // Latching disables only the optimisation — callers fall
                // back to byte-at-a-time, so correctness is unaffected.
                self.block_unsupported.store(true, Ordering::Relaxed);
                log::warn!("[SmBus] block write rejected, disabling fast path: {e}");
                Err(e)
            }
        }
    }
}

impl Drop for ChipsetBus {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.mutex);
        }
    }
}

// ── Enumeration ───────────────────────────────────────────────────────────────

/// Enumerate chipset SMBus controllers via WMI `Win32_PnPEntity`, parsing
/// the PCI VEN_/DEV_/SUBSYS_ identifiers out of each `DeviceID`.
pub(super) fn enumerate_buses() -> Vec<BusInfo> {
    use std::collections::HashMap;
    use wmi::{COMLibrary, Variant, WMIConnection};

    let com = match COMLibrary::new() {
        Ok(c) => c,
        Err(e) => {
            log::debug!("[SmBus] COM init failed: {e}");
            return vec![];
        }
    };
    let conn = match WMIConnection::new(com) {
        Ok(c) => c,
        Err(e) => {
            log::debug!("[SmBus] WMI connect failed: {e}");
            return vec![];
        }
    };

    let rows: Vec<HashMap<String, Variant>> =
        match conn.raw_query("SELECT DeviceID, Name FROM Win32_PnPEntity") {
            Ok(r) => r,
            Err(e) => {
                log::debug!("[SmBus] WMI query failed: {e}");
                return vec![];
            }
        };

    let str_of = |v: Option<&Variant>| -> String {
        match v {
            Some(Variant::String(s)) => s.clone(),
            _ => String::new(),
        }
    };

    let mut buses = Vec::new();
    let mut bus_number: u16 = 0;
    for row in &rows {
        let device_id = str_of(row.get("DeviceID")).to_uppercase();
        if !device_id.contains("VEN_") {
            continue;
        }
        let name = str_of(row.get("Name"));
        let name_lc = name.to_lowercase();
        if !["smbus", "sm bus", "i2c", "smu"]
            .iter()
            .any(|kw| name_lc.contains(kw))
        {
            continue;
        }

        // Windows formats SUBSYS_ as <subdevice><subvendor>, 8 hex digits.
        let subsys = hex_run(&device_id, "SUBSYS_");
        let pci_vendor = parse_hex(hex_run(&device_id, "VEN_"), 0, 4);
        let pci_device = parse_hex(hex_run(&device_id, "DEV_"), 0, 4);
        let pci_sub_device = parse_hex(subsys, 0, 4);
        let pci_sub_vendor = parse_hex(subsys, 4, 8);
        let label = if name.is_empty() {
            device_id.clone()
        } else {
            name
        };

        // AMD FCH exposes two ports per PCI device; emit one bus per port so discovery scans each.
        const VENDOR_AMD: u16 = 0x1022;
        let port_count: u8 = if pci_vendor == VENDOR_AMD { 2 } else { 1 };
        for port in 0..port_count {
            buses.push(BusInfo {
                bus_number: u8::try_from(bus_number).unwrap_or_else(|_| {
                    log::warn!(
                        "[chipset] SMBus bus_number {bus_number} exceeds u8, clamping to 255"
                    );
                    255
                }),
                adapter_name: if port_count > 1 {
                    format!("{label} (port {port})")
                } else {
                    label.clone()
                },
                pci_vendor,
                pci_device,
                pci_sub_device,
                pci_sub_vendor,
            });
            bus_number += 1;
        }
    }

    if buses.is_empty() {
        log::warn!(
            "[SmBus] no chipset SMBus controller found via WMI, DRAM RGB \
             will be unavailable"
        );
    } else {
        for b in &buses {
            log::info!(
                "[SmBus] chipset bus {} = {} (VEN_{:04X}&DEV_{:04X})",
                b.bus_number,
                b.adapter_name,
                b.pci_vendor,
                b.pci_device
            );
        }
    }
    buses
}

/// Return the run of hex digits immediately following `prefix` in `id`.
fn hex_run<'a>(id: &'a str, prefix: &str) -> Option<&'a str> {
    let start = id.find(prefix)? + prefix.len();
    let rest = &id[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Parse `hex[from..to]` as a `u16`, returning 0 when absent or malformed.
fn parse_hex(hex: Option<&str>, from: usize, to: usize) -> u16 {
    hex.and_then(|h| h.get(from..to))
        .and_then(|s| u16::from_str_radix(s, 16).ok())
        .unwrap_or(0)
}
