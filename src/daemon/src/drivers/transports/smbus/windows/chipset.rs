// SPDX-License-Identifier: GPL-3.0-or-later
//! PawnIO chipset SMBus transport.
//!
//! Chipset SMBus controllers (Intel i801, AMD FCH/PIIX4) are driven through the
//! PawnIO kernel driver (<https://pawnio.eu/>). PawnIO plumbing (DLL loading,
//! blob caching, ioctl dispatch) lives in
//! [`crate::drivers::transports::pawnio`].

use anyhow::{anyhow, Result};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

use crate::drivers::transports::pawnio::PawnioModule;
use crate::drivers::transports::smbus::BusInfo;

use super::{
    SMBUS_BLOCK_DATA, SMBUS_BLOCK_MAX, SMBUS_BYTE, SMBUS_BYTE_DATA, SMBUS_QUICK, SMBUS_READ,
    SMBUS_WORD_DATA, SMBUS_WRITE,
};

/// Build the PawnIO `ioctl_smbus_xfer` input array for a block write.
///
/// Passes a 9-word input and `memcpy`s the whole `i2c_smbus_data` union into `in[4]`:
///   `in[0..4]` = addr, read_write, command, size
///   `in[4..9]` = the `i2c_smbus_data.block` buffer as raw little-endian
///                bytes — `block[0]` = length, `block[1..]` = data.
/// The block buffer is `I2C_SMBUS_BLOCK_MAX + 2` = 34 bytes; packed into 5
/// `u64` words (40 bytes) it occupies exactly `in[4]..in[9]`.
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
    // i2c_smbus_data.block: byte 0 = length, bytes 1.. = payload.
    let mut block = [0u8; 40];
    block[0] = data.len() as u8;
    block[1..1 + data.len()].copy_from_slice(data);
    for (i, slot) in args[4..9].iter_mut().enumerate() {
        *slot = u64::from_le_bytes(block[i * 8..i * 8 + 8].try_into().unwrap());
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
        assert_eq!(args[4], u64::from_le_bytes([3, 0xAA, 0xBB, 0xCC, 0, 0, 0, 0]));
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
}

pub(crate) struct ChipsetBus {
    module: PawnioModule,
    // Global mutex shared across apps that drive the SMBus.
    mutex: HANDLE,
    // PIIX4 port to select before each transfer (AMD multi-port chipsets);
    // `None` for single-port controllers such as Intel i801.
    port: Option<u64>,
    // Latched once a block transfer is rejected by the loaded PawnIO
    // module. Block support is a static property of the `.bin` module, so
    // after one rejection we stop attempting the fast path and let callers
    // fall back to byte-at-a-time — avoiding a wasted mutex+ioctl per frame.
    block_unsupported: std::sync::atomic::AtomicBool,
}

// SAFETY: the Win32 mutex handle is only touched while the owning
// `Mutex<SmBusInner>` is held; `PawnioModule` already declares the equivalent
// for its own handle.
unsafe impl Send for ChipsetBus {}

impl ChipsetBus {
    pub(super) fn open(bus_number: u8, pci_vendor: u16) -> Result<Self> {
        // Intel chipsets use SmbusI801, AMD use SmbusPIIX4; each module's
        // probe rejects the wrong vendor, so when the vendor is unknown we
        // hand both candidates to PawnIO and keep whichever loads.
        const VENDOR_INTEL: u16 = 0x8086;
        const VENDOR_AMD: u16 = 0x1022;
        let blob_names: &[&str] = match pci_vendor {
            VENDOR_AMD => &["SmbusPIIX4.bin"],
            VENDOR_INTEL => &["SmbusI801.bin"],
            _ => &["SmbusI801.bin", "SmbusPIIX4.bin"],
        };
        let module = PawnioModule::open(blob_names).map_err(|e| {
            anyhow!(
                "no matching PawnIO SMBus module loaded — place SmbusI801.bin \
                 (Intel) or SmbusPIIX4.bin (AMD) next to the executable ({e})"
            )
        })?;
        log::info!("[SmBus] loaded PawnIO module {}", module.blob_name());

        let name: Vec<u16> = "Global\\Access_SMBUS.HTP.Method\0".encode_utf16().collect();
        let mutex = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
            .map_err(|e| anyhow!("CreateMutexW failed: {e}"))?;

        // AMD FCH SMBus controllers are multi-port. The active port is
        // shared hardware state, so it must be re-selected before every
        // transfer (done in `xfer`). `bus_number` doubles as the port
        // index. Intel i801 is single-port — no selection needed.
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
        let wait = unsafe { WaitForSingleObject(self.mutex, 2000) };
        if wait != WAIT_OBJECT_0 {
            return Err(anyhow!("SMBus mutex acquire failed/timed out"));
        }

        // Select the PIIX4 port (AMD multi-port) under the same mutex so it
        // cannot race a transfer targeting the other port. Errors are
        // intentionally swallowed — the subsequent xfer will report any
        // real problem.
        if let Some(port) = self.port {
            let _ = self
                .module
                .exec(b"ioctl_piix4_port_sel\0", &[port], &mut [0u64; 1]);
        }

        let mut out = [0u64; 5];
        let res = self.module.exec(b"ioctl_smbus_xfer\0", input, &mut out);

        unsafe {
            let _ = ReleaseMutex(self.mutex);
        }

        res.map_err(|e| anyhow!("ioctl_smbus_xfer failed: {e}"))?;
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
    let conn = match WMIConnection::new(com.into()) {
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
    let mut bus_number: u8 = 0;
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
        let label = if name.is_empty() { device_id.clone() } else { name };

        // AMD FCH SMBus controllers expose two ports behind one PCI device,
        // and DRAM RGB usually sits on the secondary port. Intel i801 is
        // single-port. Emit one bus per port so discovery scans each.
        const VENDOR_AMD: u16 = 0x1022;
        let port_count: u8 = if pci_vendor == VENDOR_AMD { 2 } else { 1 };
        for port in 0..port_count {
            buses.push(BusInfo {
                bus_number,
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
            "[SmBus] no chipset SMBus controller found via WMI — DRAM RGB \
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
