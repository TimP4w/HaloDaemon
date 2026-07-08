pub mod common;
pub mod computer;
pub mod generic_argb;
#[cfg(target_os = "linux")]
pub mod hwmon_device;
#[cfg(target_os = "windows")]
pub mod superio;
