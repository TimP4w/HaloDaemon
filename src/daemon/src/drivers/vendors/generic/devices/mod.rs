// SPDX-License-Identifier: GPL-3.0-or-later
pub mod common;
pub mod computer;
// Chain construction still uses this small generic leaf; it is not a native
// hardware discovery driver.
pub mod generic_argb;
#[cfg(target_os = "linux")]
pub mod hwmon_device;
