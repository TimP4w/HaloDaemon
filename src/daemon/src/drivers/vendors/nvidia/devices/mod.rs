// SPDX-License-Identifier: GPL-3.0-or-later
//! NVIDIA GPU support — temperature sensors via NvAPI on Windows and the
//! `nvidia-smi` CLI on Linux.

#[cfg(any(target_os = "windows", target_os = "linux"))]
pub mod gpu_sensor_common;

#[cfg(target_os = "windows")]
pub mod nvapi_thermal;

#[cfg(target_os = "windows")]
pub mod gpu_sensor;

#[cfg(target_os = "linux")]
pub mod nvidia_smi;

#[cfg(target_os = "linux")]
pub mod gpu_sensor_linux;
