// SPDX-License-Identifier: GPL-3.0-or-later
//! Linux SMBus backend — chipset and GPU i2c buses through the i2c-dev ioctl interface (`/dev/i2c-N`).

use super::*;
use std::os::unix::io::RawFd;

// i2c-dev ioctl constants
const I2C_SLAVE: libc::c_ulong = 0x0703;
const I2C_SMBUS: libc::c_ulong = 0x0720;
const I2C_TIMEOUT: libc::c_ulong = 0x0702;
const I2C_FUNCS: libc::c_ulong = 0x0705;
const I2C_FUNC_SMBUS_QUICK: libc::c_ulong = 0x00010000;

const I2C_SMBUS_READ: u8 = 1;
const I2C_SMBUS_WRITE: u8 = 0;
const I2C_SMBUS_QUICK: u32 = 0;
const I2C_SMBUS_BYTE: u32 = 1;
const I2C_SMBUS_BYTE_DATA: u32 = 2;
const I2C_SMBUS_WORD_DATA: u32 = 3;
const I2C_SMBUS_BLOCK_DATA: u32 = 5;
use super::SMBUS_BLOCK_MAX as I2C_SMBUS_BLOCK_MAX;

#[repr(C)]
union SmBusData {
    byte: u8,
    word: u16,
    block: [u8; 34],
}

#[repr(C)]
struct I2cSmbusIoctlData {
    read_write: u8,
    command: u8,
    size: u32,
    data: *mut SmBusData,
}

unsafe impl Send for I2cSmbusIoctlData {}

fn smbus_ioctl(
    fd: RawFd,
    read_write: u8,
    command: u8,
    size: u32,
    data: &mut SmBusData,
) -> libc::c_int {
    let mut args = I2cSmbusIoctlData {
        read_write,
        command,
        size,
        data: data as *mut SmBusData,
    };
    unsafe { libc::ioctl(fd, I2C_SMBUS, &mut args as *mut I2cSmbusIoctlData) }
}

pub(super) struct SmBusInner {
    fd: RawFd,
    current_addr: Option<u8>,
    supports_quick: bool,
}

impl Drop for SmBusInner {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

impl SmBusInner {
    pub fn open(bus_number: u8) -> Result<Self> {
        let path = format!("/dev/i2c-{}", bus_number);
        let cpath =
            std::ffi::CString::new(path.as_str()).map_err(|e| anyhow!("invalid path: {e}"))?;
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            return Err(anyhow!(
                "cannot open {}: {}",
                path,
                std::io::Error::last_os_error()
            ));
        }
        let ret = unsafe { libc::ioctl(fd, I2C_TIMEOUT, 5i64) };
        if ret < 0 {
            log::warn!(
                "[SmBus] I2C_TIMEOUT ioctl failed (bus will use kernel-default timeout): {}",
                std::io::Error::last_os_error()
            );
        }
        let mut funcs: libc::c_ulong = 0;
        let ret = unsafe { libc::ioctl(fd, I2C_FUNCS, &mut funcs as *mut libc::c_ulong) };
        if ret < 0 {
            log::debug!(
                "[SmBus] I2C_FUNCS ioctl failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let supports_quick = (funcs & I2C_FUNC_SMBUS_QUICK) != 0;
        Ok(Self {
            fd,
            current_addr: None,
            supports_quick,
        })
    }

    fn set_addr(&mut self, addr: u8) -> Result<()> {
        if self.current_addr != Some(addr) {
            let ret = unsafe { libc::ioctl(self.fd, I2C_SLAVE, addr as libc::c_ulong) };
            if ret < 0 {
                return Err(anyhow!(
                    "I2C_SLAVE ioctl failed for addr 0x{:02X}: {}",
                    addr,
                    std::io::Error::last_os_error()
                ));
            }
            self.current_addr = Some(addr);
        }
        Ok(())
    }
}

impl SmBusSyncOps for SmBusInner {
    fn read_byte(&mut self, addr: u8) -> Result<u8> {
        self.set_addr(addr)?;
        let mut data = SmBusData { byte: 0 };
        let ret = smbus_ioctl(self.fd, I2C_SMBUS_READ, 0, I2C_SMBUS_BYTE, &mut data);
        if ret < 0 {
            return Err(anyhow!(
                "read_byte 0x{:02X} failed: {}",
                addr,
                std::io::Error::last_os_error()
            ));
        }
        Ok(unsafe { data.byte })
    }

    fn read_byte_data(&mut self, addr: u8, cmd: u8) -> Result<u8> {
        self.set_addr(addr)?;
        let mut data = SmBusData { byte: 0 };
        let ret = smbus_ioctl(self.fd, I2C_SMBUS_READ, cmd, I2C_SMBUS_BYTE_DATA, &mut data);
        if ret < 0 {
            return Err(anyhow!(
                "read_byte_data 0x{:02X} cmd=0x{:02X} failed: {}",
                addr,
                cmd,
                std::io::Error::last_os_error()
            ));
        }
        Ok(unsafe { data.byte })
    }

    fn write_quick(&mut self, addr: u8) -> Result<bool> {
        if self.set_addr(addr).is_err() {
            return Ok(false);
        }
        let mut data = SmBusData { byte: 0 };
        if !self.supports_quick {
            let ret = smbus_ioctl(self.fd, I2C_SMBUS_READ, 0, I2C_SMBUS_BYTE, &mut data);
            return Ok(ret >= 0);
        }
        let ret = smbus_ioctl(self.fd, I2C_SMBUS_WRITE, 0, I2C_SMBUS_QUICK, &mut data);
        Ok(ret >= 0)
    }

    fn write_byte_data(&mut self, addr: u8, cmd: u8, val: u8) -> Result<()> {
        self.set_addr(addr)?;
        let mut data = SmBusData { byte: val };
        let ret = smbus_ioctl(
            self.fd,
            I2C_SMBUS_WRITE,
            cmd,
            I2C_SMBUS_BYTE_DATA,
            &mut data,
        );
        if ret < 0 {
            return Err(anyhow!(
                "write_byte_data 0x{:02X} cmd=0x{:02X} val=0x{:02X} failed: {}",
                addr,
                cmd,
                val,
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn write_word_data(&mut self, addr: u8, cmd: u8, val: u16) -> Result<()> {
        self.set_addr(addr)?;
        let mut data = SmBusData { word: val };
        let ret = smbus_ioctl(
            self.fd,
            I2C_SMBUS_WRITE,
            cmd,
            I2C_SMBUS_WORD_DATA,
            &mut data,
        );
        if ret < 0 {
            return Err(anyhow!(
                "write_word_data 0x{:02X} cmd=0x{:02X} val=0x{:04X} failed: {}",
                addr,
                cmd,
                val,
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn write_block_data(&mut self, addr: u8, cmd: u8, data: &[u8]) -> Result<()> {
        if data.is_empty() || data.len() > I2C_SMBUS_BLOCK_MAX {
            return Err(anyhow!(
                "SMBus block length {} out of range 1..={}",
                data.len(),
                I2C_SMBUS_BLOCK_MAX
            ));
        }
        let sz = data.len();
        self.set_addr(addr)?;
        let mut smbus_data = SmBusData { block: [0u8; 34] };
        unsafe {
            smbus_data.block[0] = sz as u8;
            smbus_data.block[1..1 + sz].copy_from_slice(data);
        }
        let ret = smbus_ioctl(
            self.fd,
            I2C_SMBUS_WRITE,
            cmd,
            I2C_SMBUS_BLOCK_DATA,
            &mut smbus_data,
        );
        if ret < 0 {
            return Err(anyhow!(
                "write_block_data 0x{:02X} cmd=0x{:02X} sz={} failed: {}",
                addr,
                cmd,
                sz,
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

pub fn enumerate_buses() -> Vec<BusInfo> {
    let mut buses = Vec::new();
    let dev_entries = match std::fs::read_dir("/dev") {
        Ok(e) => e,
        Err(_) => return buses,
    };

    let mut nums: Vec<u8> = dev_entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.strip_prefix("i2c-").and_then(|n| n.parse::<u8>().ok())
        })
        .collect();
    nums.sort();

    for num in nums {
        let adapter_name = read_adapter_name(num);
        let (pci_vendor, pci_device, pci_sub_vendor, pci_sub_device) =
            read_pci_ids(num).unwrap_or((0, 0, 0, 0));
        buses.push(BusInfo {
            bus_number: num,
            adapter_name,
            pci_vendor,
            pci_device,
            pci_sub_vendor,
            pci_sub_device,
        });
    }
    buses
}

fn read_adapter_name(num: u8) -> String {
    for path in [
        format!("/sys/class/i2c-adapter/i2c-{}/name", num),
        format!("/sys/bus/i2c/devices/i2c-{}/name", num),
    ] {
        if let Ok(s) = std::fs::read_to_string(&path) {
            return s.trim().to_string();
        }
    }
    String::new()
}

fn read_sysfs_hex(path: std::path::PathBuf) -> Option<u16> {
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
}

fn read_pci_ids(num: u8) -> Option<(u16, u16, u16, u16)> {
    let link = format!("/sys/bus/i2c/devices/i2c-{}", num);
    let real = std::fs::canonicalize(&link)
        .inspect_err(|e| {
            log::debug!("[SmBusTransport] canonicalize({link}) failed: {e}");
        })
        .ok()?;

    // Walk up the device tree from the i2c adapter until we find a PCI device
    // with all four ID files. No depth limit — NVIDIA's DRM connector nesting
    // can place the PCI node several levels above the i2c adapter.
    let mut dir = real.parent().map(std::path::Path::to_path_buf);
    while let Some(d) = dir {
        let vendor = read_sysfs_hex(d.join("vendor"));
        let device = read_sysfs_hex(d.join("device"));
        let sub_vendor = read_sysfs_hex(d.join("subsystem_vendor"));
        let sub_device = read_sysfs_hex(d.join("subsystem_device"));
        if let (Some(v), Some(dev), Some(sv), Some(sd)) = (vendor, device, sub_vendor, sub_device) {
            return Some((v, dev, sv, sd));
        }
        dir = d.parent().map(std::path::Path::to_path_buf);
    }
    log::debug!(
        "[SmBusTransport] i2c-{num}: no PCI device found in ancestry; \
         canonical path was {real:?}"
    );
    None
}

pub fn open_device(info: &BusInfo) -> Result<SmBusInner> {
    SmBusInner::open(info.bus_number)
}

pub fn enumerate_gpu_buses() -> Vec<BusInfo> {
    enumerate_buses()
        .into_iter()
        .filter(|b| b.is_gpu_bus())
        .collect()
}
