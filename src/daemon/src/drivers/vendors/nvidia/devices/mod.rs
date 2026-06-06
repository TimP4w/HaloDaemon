//! NVIDIA GPU support — currently just temperature sensors via NvAPI.

#[cfg(target_os = "windows")]
pub mod nvapi_thermal;

#[cfg(target_os = "windows")]
pub mod gpu_sensor;

#[cfg(target_os = "windows")]
pub use gpu_sensor::NvidiaGpuTransport;
