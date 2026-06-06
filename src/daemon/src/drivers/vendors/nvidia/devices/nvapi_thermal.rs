// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026-present HaloDaemon contributors

#![cfg(target_os = "windows")]

//! NvAPI thermal-sensor wrapper.
//!
//! NvAPI is a process singleton: `NvAPI_Initialize` is reference-counted
//! inside the driver, and `nvapi_QueryInterface` always returns the same
//! resolved function pointers regardless of how many times it's loaded.
//! That lets this module keep its own state alongside the SMBus
//! [`smbus::windows::nvapi`][crate::drivers::transports::smbus] one without
//! either interfering.

use anyhow::{anyhow, Result};
use std::sync::OnceLock;

const NVAPI_OK: i32 = 0;
const NVAPI_MAX_GPUS: usize = 64;
const NVAPI_MAX_THERMAL_SENSORS_PER_GPU: usize = 3;
const NVAPI_SHORT_STRING_MAX: usize = 64;

// NvAPI function IDs — stable across driver versions.
const FN_INITIALIZE: u32 = 0x0150_E828;
const FN_ENUM_PHYSICAL_GPUS: u32 = 0xE5AC_921F;
const FN_GPU_GET_FULL_NAME: u32 = 0xCEEE_8E9F;
const FN_GPU_GET_THERMAL_SETTINGS: u32 = 0xE3640A56;

/// `NV_GPU_THERMAL_SETTINGS_V2` from `NvApi.h`. Layout is documented in the
/// NVIDIA NvAPI SDK headers.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvThermalSensor {
    pub controller: u32,
    pub default_min_temp: i32,
    pub default_max_temp: i32,
    pub current_temp: i32,
    /// `NV_THERMAL_TARGET`: 1=GPU, 2=Memory, 4=Board, 5=VCD, 6=Inlet,
    /// 7=Outlet, 15=ALL (used as input).
    pub target: u32,
}

#[repr(C)]
pub struct NvThermalSettings {
    pub version: u32,
    pub count: u32,
    pub sensor: [NvThermalSensor; NVAPI_MAX_THERMAL_SENSORS_PER_GPU],
}

impl NvThermalSettings {
    fn zeroed() -> Self {
        Self {
            version: Self::nvapi_version(),
            count: 0,
            sensor: [NvThermalSensor {
                controller: 0,
                default_min_temp: 0,
                default_max_temp: 0,
                current_temp: 0,
                target: 0,
            }; NVAPI_MAX_THERMAL_SENSORS_PER_GPU],
        }
    }

    fn nvapi_version() -> u32 {
        // MAKE_NVAPI_VERSION: struct size in low 16 bits, version id in high.
        (std::mem::size_of::<Self>() as u32) | (2 << 16)
    }
}

type QueryFn = unsafe extern "C" fn(u32) -> usize;
type InitFn = unsafe extern "C" fn() -> i32;
type EnumGpusFn = unsafe extern "C" fn(*mut usize, *mut u32) -> i32;
type GetFullNameFn = unsafe extern "C" fn(usize, *mut u8) -> i32;
type GetThermalFn = unsafe extern "C" fn(usize, u32, *mut NvThermalSettings) -> i32;

/// Cached NvAPI handles for the lifetime of the process.
struct NvApi {
    _lib: libloading::Library,
    get_full_name: GetFullNameFn,
    get_thermal: GetThermalFn,
    gpus: Vec<NvGpu>,
}

// SAFETY: `NvApi` is initialised once and only read afterwards; raw pointers
// stay valid as long as `_lib` is held.
unsafe impl Send for NvApi {}
unsafe impl Sync for NvApi {}

pub struct NvGpu {
    pub handle: usize,
    pub name: String,
}

static NVAPI: OnceLock<Option<NvApi>> = OnceLock::new();

fn nvapi() -> Option<&'static NvApi> {
    NVAPI.get_or_init(init_nvapi).as_ref()
}

fn init_nvapi() -> Option<NvApi> {
    let lib = unsafe { libloading::Library::new("nvapi64.dll") }
        .map_err(|e| log::debug!("[NvAPI thermal] nvapi64.dll not loadable: {e}"))
        .ok()?;

    let query: libloading::Symbol<QueryFn> = unsafe { lib.get(b"nvapi_QueryInterface\0") }
        .map_err(|e| log::debug!("[NvAPI thermal] nvapi_QueryInterface missing: {e}"))
        .ok()?;

    let init_ptr = unsafe { query(FN_INITIALIZE) };
    let enum_ptr = unsafe { query(FN_ENUM_PHYSICAL_GPUS) };
    let name_ptr = unsafe { query(FN_GPU_GET_FULL_NAME) };
    let thermal_ptr = unsafe { query(FN_GPU_GET_THERMAL_SETTINGS) };
    drop(query);

    if init_ptr == 0 || enum_ptr == 0 || name_ptr == 0 || thermal_ptr == 0 {
        log::debug!("[NvAPI thermal] required NvAPI function missing");
        return None;
    }

    let init: InitFn = unsafe { std::mem::transmute(init_ptr) };
    let enum_gpus: EnumGpusFn = unsafe { std::mem::transmute(enum_ptr) };
    let get_full_name: GetFullNameFn = unsafe { std::mem::transmute(name_ptr) };
    let get_thermal: GetThermalFn = unsafe { std::mem::transmute(thermal_ptr) };

    // NvAPI_Initialize is ref-counted — fine to call alongside the SMBus
    // NvAPI module.
    if unsafe { init() } != NVAPI_OK {
        log::debug!("[NvAPI thermal] NvAPI_Initialize failed");
        return None;
    }

    let mut handles = [0usize; NVAPI_MAX_GPUS];
    let mut count: u32 = 0;
    if unsafe { enum_gpus(handles.as_mut_ptr(), &mut count) } != NVAPI_OK {
        log::debug!("[NvAPI thermal] NvAPI_EnumPhysicalGPUs failed");
        return None;
    }

    let mut gpus = Vec::new();
    for &handle in handles.iter().take(count as usize) {
        if handle == 0 {
            continue;
        }
        let mut name_buf = [0u8; NVAPI_SHORT_STRING_MAX];
        let name = if unsafe { get_full_name(handle, name_buf.as_mut_ptr()) } == NVAPI_OK {
            let end = name_buf
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_buf.len());
            String::from_utf8_lossy(&name_buf[..end]).into_owned()
        } else {
            format!("NVIDIA GPU 0x{handle:08X}")
        };
        gpus.push(NvGpu { handle, name });
    }

    if gpus.is_empty() {
        log::debug!("[NvAPI thermal] no GPUs enumerated");
        return None;
    }
    log::info!(
        "[NvAPI thermal] discovered {} GPU(s): {}",
        gpus.len(),
        gpus.iter()
            .map(|g| g.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    Some(NvApi {
        _lib: lib,
        get_full_name,
        get_thermal,
        gpus,
    })
}

/// All GPUs NvAPI is aware of, in enumeration order.
pub fn enumerate_gpus() -> Vec<NvGpu> {
    nvapi()
        .map(|api| {
            api.gpus
                .iter()
                .map(|g| NvGpu {
                    handle: g.handle,
                    name: g.name.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy)]
pub struct ThermalReading {
    /// Friendly sensor name (e.g. "GPU Core", "Memory Junction").
    pub label: &'static str,
    pub temperature_c: f64,
}

/// Query the thermal sensors on `handle`. Sensor labels map the NvAPI
/// `NV_THERMAL_TARGET` enum to friendly names.
pub fn read_temperatures(handle: usize) -> Result<Vec<ThermalReading>> {
    let api = nvapi().ok_or_else(|| anyhow!("NvAPI not available"))?;
    let mut settings = NvThermalSettings::zeroed();
    // Sensor index 15 = `NVAPI_THERMAL_TARGET_ALL` — return every sensor.
    let rc = unsafe { (api.get_thermal)(handle, 15, &mut settings) };
    if rc != NVAPI_OK {
        return Err(anyhow!("NvAPI_GPU_GetThermalSettings failed: rc={rc}"));
    }

    let mut out = Vec::with_capacity(settings.count as usize);
    let count = (settings.count as usize).min(NVAPI_MAX_THERMAL_SENSORS_PER_GPU);
    for sensor in &settings.sensor[..count] {
        let label = match sensor.target {
            1 => "GPU Core",
            2 => "GPU Memory Junction",
            3 => "Power Supply",
            4 => "GPU Board",
            5 => "Visual Computing Board",
            6 => "Visual Computing Inlet",
            7 => "Visual Computing Outlet",
            _ => continue,
        };
        out.push(ThermalReading {
            label,
            temperature_c: sensor.current_temp as f64,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv_thermal_settings_size_matches_layout() {
        // version + count = 8 bytes, plus 3 * sizeof(sensor)=20 = 60 bytes.
        // Total = 68 bytes.
        assert_eq!(std::mem::size_of::<NvThermalSettings>(), 68);
        assert_eq!(std::mem::size_of::<NvThermalSensor>(), 20);
    }

    #[test]
    fn version_encoding_includes_struct_size_and_v2() {
        let v = NvThermalSettings::nvapi_version();
        // Low 16 bits = struct size.
        assert_eq!(v & 0xFFFF, std::mem::size_of::<NvThermalSettings>() as u32);
        // High 16 bits = version (2).
        assert_eq!(v >> 16, 2);
    }
}
