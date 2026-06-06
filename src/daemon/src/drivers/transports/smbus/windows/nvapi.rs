//! NvAPI GPU i2c transport.
//!
//! NVIDIA GPU i2c buses are driven through `nvapi64.dll`. NvAPI is a process
//! singleton: the library is loaded once, the I2C function pointers resolved by
//! ID, and the enumerated GPUs cached for the process lifetime.

use anyhow::{anyhow, Result};
use std::sync::OnceLock;

use crate::drivers::transports::smbus::BusInfo;

use super::{SMBUS_BYTE, SMBUS_BYTE_DATA, SMBUS_QUICK, SMBUS_READ, SMBUS_WORD_DATA, SMBUS_WRITE};

const NVAPI_OK: i32 = 0;
const NVAPI_MAX_GPUS: usize = 64;

// GPU buses are numbered from this offset so they stay disjoint from the
// chipset buses, which the chipset backend numbers from 0.
const GPU_BUS_OFFSET: u8 = 128;

// Documented NvAPI interface IDs (stable across driver versions).
const FN_INITIALIZE: u32 = 0x0150_E828;
const FN_ENUM_PHYSICAL_GPUS: u32 = 0xE5AC_921F;
const FN_GPU_GET_PCI_IDENTIFIERS: u32 = 0x2DDF_B66E;
const FN_I2C_READ_EX: u32 = 0x4D7B_0709;
const FN_I2C_WRITE_EX: u32 = 0x283A_C65A;

/// Mirror of NvAPI's `NV_I2C_INFO_V3`. Explicit padding keeps the pointer
/// fields 8-byte aligned to match the NVIDIA SDK layout on x64.
#[repr(C)]
#[allow(dead_code)] // fields are written here and read by NvAPI across the FFI boundary
struct NvI2cInfoV3 {
    version: u32,
    display_mask: u32,
    is_ddc_port: u8,
    i2c_dev_address: u8,
    _pad0: [u8; 6],
    pb_i2c_reg_address: *mut u8,
    reg_addr_size: u32,
    _pad1: [u8; 4],
    pb_data: *mut u8,
    cb_size: u32,
    i2c_speed: u32,
    i2c_speed_khz: u32,
    port_id: u8,
    _pad2: [u8; 3],
    b_is_port_id_set: u32,
}

type NvI2cFn = unsafe extern "C" fn(usize, *mut NvI2cInfoV3, *mut u32) -> i32;

struct NvGpu {
    bus_number: u8,
    handle: usize,
    pci_vendor: u16,
    pci_device: u16,
    pci_sub_vendor: u16,
    pci_sub_device: u16,
}

/// Process-wide NvAPI state. `nvapi64.dll` stays loaded for the lifetime of
/// the process (NvAPI is a singleton), keeping the resolved I2C function
/// pointers valid.
struct NvApi {
    _lib: libloading::Library,
    i2c_read: NvI2cFn,
    i2c_write: NvI2cFn,
    gpus: Vec<NvGpu>,
}

// SAFETY: `NvApi` is initialised once and only read afterwards; the kept
// library handle and function pointers are immutable plain addresses.
unsafe impl Send for NvApi {}
unsafe impl Sync for NvApi {}

static NVAPI: OnceLock<Option<NvApi>> = OnceLock::new();

fn nvapi() -> Option<&'static NvApi> {
    NVAPI.get_or_init(init_nvapi).as_ref()
}

fn init_nvapi() -> Option<NvApi> {
    let lib = unsafe { libloading::Library::new("nvapi64.dll") }.ok()?;

    type QueryFn = unsafe extern "C" fn(u32) -> usize;
    let query: libloading::Symbol<QueryFn> =
        unsafe { lib.get(b"nvapi_QueryInterface\0") }.ok()?;

    // NvAPI exposes nothing by name; every function is resolved by ID.
    let init_ptr = unsafe { query(FN_INITIALIZE) };
    let enum_ptr = unsafe { query(FN_ENUM_PHYSICAL_GPUS) };
    let pci_ptr = unsafe { query(FN_GPU_GET_PCI_IDENTIFIERS) };
    let read_ptr = unsafe { query(FN_I2C_READ_EX) };
    let write_ptr = unsafe { query(FN_I2C_WRITE_EX) };
    drop(query);
    if init_ptr == 0 || enum_ptr == 0 || pci_ptr == 0 || read_ptr == 0 || write_ptr == 0 {
        log::debug!("[SmBus] NvAPI present but a required function is missing");
        return None;
    }

    type InitFn = unsafe extern "C" fn() -> i32;
    type EnumFn = unsafe extern "C" fn(*mut usize, *mut u32) -> i32;
    type PciFn = unsafe extern "C" fn(usize, *mut u32, *mut u32, *mut u32, *mut u32) -> i32;
    let init: InitFn = unsafe { std::mem::transmute(init_ptr) };
    let enum_gpus: EnumFn = unsafe { std::mem::transmute(enum_ptr) };
    let get_pci: PciFn = unsafe { std::mem::transmute(pci_ptr) };
    let i2c_read: NvI2cFn = unsafe { std::mem::transmute(read_ptr) };
    let i2c_write: NvI2cFn = unsafe { std::mem::transmute(write_ptr) };

    if unsafe { init() } != NVAPI_OK {
        return None;
    }

    let mut handles = [0usize; NVAPI_MAX_GPUS];
    let mut count: u32 = 0;
    if unsafe { enum_gpus(handles.as_mut_ptr(), &mut count) } != NVAPI_OK {
        return None;
    }

    let mut gpus = Vec::new();
    for (i, &handle) in handles.iter().take(count as usize).enumerate() {
        if handle == 0 {
            continue;
        }
        let (mut device_id, mut sub_id, mut rev, mut ext) = (0u32, 0u32, 0u32, 0u32);
        if unsafe { get_pci(handle, &mut device_id, &mut sub_id, &mut rev, &mut ext) } != NVAPI_OK
        {
            continue;
        }
        gpus.push(NvGpu {
            bus_number: GPU_BUS_OFFSET.saturating_add(i as u8),
            handle,
            pci_vendor: (device_id & 0xFFFF) as u16,
            pci_device: ((device_id >> 16) & 0xFFFF) as u16,
            pci_sub_vendor: (sub_id & 0xFFFF) as u16,
            pci_sub_device: ((sub_id >> 16) & 0xFFFF) as u16,
        });
    }
    if gpus.is_empty() {
        return None;
    }
    log::info!("[SmBus] NvAPI discovered {} GPU i2c bus(es)", gpus.len());
    Some(NvApi {
        _lib: lib,
        i2c_read,
        i2c_write,
        gpus,
    })
}

pub(crate) struct GpuBus {
    handle: usize,
    i2c_read: NvI2cFn,
    i2c_write: NvI2cFn,
}

impl GpuBus {
    /// Open the GPU i2c bus identified by `info`, looking it up in the
    /// process-wide NvAPI GPU table.
    pub(super) fn open(info: &BusInfo) -> Result<GpuBus> {
        let api = nvapi().ok_or_else(|| anyhow!("NvAPI is not available"))?;
        let gpu = api
            .gpus
            .iter()
            .find(|g| g.bus_number == info.bus_number)
            .ok_or_else(|| anyhow!("no NvAPI GPU for bus {}", info.bus_number))?;
        Ok(GpuBus {
            handle: gpu.handle,
            i2c_read: api.i2c_read,
            i2c_write: api.i2c_write,
        })
    }

    fn nvapi_version() -> u32 {
        // MAKE_NVAPI_VERSION: struct size in the low 16 bits, version in the high.
        (std::mem::size_of::<NvI2cInfoV3>() as u32) | (3 << 16)
    }

    /// Issue one SMBus operation over the GPU i2c bus. Returns the value
    /// read (byte/word); writes return 0.
    fn xfer(
        &self,
        addr: u8,
        read_write: u64,
        command: u8,
        size: u64,
        write_data: Option<u16>,
    ) -> Result<u16> {
        // NvAPI has no SMBus QUICK command; treat it as a no-op success so
        // device-presence probes don't get a false negative.
        if size == SMBUS_QUICK {
            return Ok(0);
        }

        let mut reg_byte = command;
        let mut data = [0u8; 32];
        let cb_size: u32;
        match size {
            SMBUS_BYTE => {
                data[0] = command;
                cb_size = 1;
            }
            SMBUS_BYTE_DATA => {
                cb_size = 1;
                if read_write == SMBUS_WRITE {
                    if let Some(v) = write_data {
                        data[0] = v as u8;
                    }
                }
            }
            SMBUS_WORD_DATA => {
                cb_size = 2;
                if read_write == SMBUS_WRITE {
                    if let Some(v) = write_data {
                        data[0] = (v & 0xFF) as u8;
                        data[1] = (v >> 8) as u8;
                    }
                }
            }
            _ => return Err(anyhow!("unsupported SMBus size {size} for NvAPI")),
        }

        let mut info = NvI2cInfoV3 {
            version: Self::nvapi_version(),
            display_mask: 0,
            is_ddc_port: 0,
            i2c_dev_address: (addr << 1) & 0xFF,
            _pad0: [0; 6],
            pb_i2c_reg_address: &mut reg_byte,
            reg_addr_size: if size == SMBUS_BYTE { 0 } else { 1 },
            _pad1: [0; 4],
            pb_data: data.as_mut_ptr(),
            cb_size,
            i2c_speed: 0xFFFF,
            i2c_speed_khz: 0,
            port_id: 1,
            _pad2: [0; 3],
            b_is_port_id_set: 1,
        };

        let mut unknown: u32 = 0;
        let func = if read_write == SMBUS_WRITE {
            self.i2c_write
        } else {
            self.i2c_read
        };
        let rc = unsafe { func(self.handle, &mut info, &mut unknown) };
        if rc != NVAPI_OK {
            return Err(anyhow!("NvAPI I2C transfer failed: rc={rc}"));
        }

        Ok(match size {
            SMBUS_WORD_DATA => (data[0] as u16) | ((data[1] as u16) << 8),
            _ => data[0] as u16,
        })
    }

    pub(super) fn read_byte(&mut self, addr: u8) -> Result<u8> {
        Ok(self.xfer(addr, SMBUS_READ, 0, SMBUS_BYTE, None)? as u8)
    }
    pub(super) fn read_byte_data(&mut self, addr: u8, cmd: u8) -> Result<u8> {
        Ok(self.xfer(addr, SMBUS_READ, cmd, SMBUS_BYTE_DATA, None)? as u8)
    }
    pub(super) fn write_quick(&mut self, _addr: u8) -> Result<bool> {
        // No QUICK command on NvAPI — report ACK.
        Ok(true)
    }
    pub(super) fn write_byte_data(&mut self, addr: u8, cmd: u8, val: u8) -> Result<()> {
        self.xfer(addr, SMBUS_WRITE, cmd, SMBUS_BYTE_DATA, Some(val as u16))?;
        Ok(())
    }
    pub(super) fn write_word_data(&mut self, addr: u8, cmd: u8, val: u16) -> Result<()> {
        self.xfer(addr, SMBUS_WRITE, cmd, SMBUS_WORD_DATA, Some(val))?;
        Ok(())
    }
}

pub(super) fn enumerate_gpu_buses() -> Vec<BusInfo> {
    let Some(api) = nvapi() else { return vec![] };
    api.gpus
        .iter()
        .enumerate()
        .map(|(i, g)| BusInfo {
            bus_number: g.bus_number,
            adapter_name: format!("NVIDIA NvAPI I2C GPU {i}"),
            pci_vendor: g.pci_vendor,
            pci_device: g.pci_device,
            pci_sub_vendor: g.pci_sub_vendor,
            pci_sub_device: g.pci_sub_device,
        })
        .collect()
}
