//! Linux SMBus backend — chipset and GPU i2c buses through the i2c-dev ioctl
//! interface (`/dev/i2c-N`). GPU buses need no separate path: they are simply
//! chipset-enumerated i2c buses whose adapter name identifies them as an
//! NVIDIA/AMD GPU adapter.

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
        let fd = unsafe {
            libc::open(
                std::ffi::CString::new(path.clone()).unwrap().as_ptr(),
                libc::O_RDWR,
            )
        };
        if fd < 0 {
            return Err(anyhow!(
                "cannot open {}: {}",
                path,
                std::io::Error::last_os_error()
            ));
        }
        // 50 ms timeout per ioctl (5 × 10 ms)
        unsafe { libc::ioctl(fd, I2C_TIMEOUT, 5i64) };
        let mut funcs: libc::c_ulong = 0;
        unsafe { libc::ioctl(fd, I2C_FUNCS, &mut funcs as *mut libc::c_ulong) };
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
        if let Err(_) = self.set_addr(addr) {
            return Ok(false);
        }
        let mut data = SmBusData { byte: 0 };
        if !self.supports_quick {
            // Adapter has I2C_AQ_NO_ZERO_LEN quirk; use a BYTE read as presence probe instead.
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
        let sz = data.len().min(I2C_SMBUS_BLOCK_MAX);
        self.set_addr(addr)?;
        let mut smbus_data = SmBusData { block: [0u8; 34] };
        unsafe {
            smbus_data.block[0] = sz as u8;
            smbus_data.block[1..1 + sz].copy_from_slice(&data[..sz]);
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
            if s.starts_with("i2c-") {
                s[4..].parse::<u8>().ok()
            } else {
                None
            }
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

fn read_pci_ids(num: u8) -> Option<(u16, u16, u16, u16)> {
    let link = format!("/sys/bus/i2c/devices/i2c-{}", num);
    let real = std::fs::canonicalize(&link).ok()?;
    let mut dir = real.as_path();
    for _ in 0..4 {
        dir = dir.parent()?;
        let vendor = std::fs::read_to_string(dir.join("vendor"))
            .ok()
            .and_then(|s| u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok());
        let device = std::fs::read_to_string(dir.join("device"))
            .ok()
            .and_then(|s| u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok());
        let sub_vendor = std::fs::read_to_string(dir.join("subsystem_vendor"))
            .ok()
            .and_then(|s| u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok());
        let sub_device = std::fs::read_to_string(dir.join("subsystem_device"))
            .ok()
            .and_then(|s| u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok());
        if let (Some(v), Some(d), Some(sv), Some(sd)) = (vendor, device, sub_vendor, sub_device) {
            return Some((v, d, sv, sd));
        }
    }
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
