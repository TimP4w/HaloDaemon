// SPDX-License-Identifier: GPL-3.0-or-later
pub mod elevation;
#[cfg(target_os = "linux")]
pub mod env;
pub mod notify;
#[cfg(windows)]
pub mod service;
#[cfg(windows)]
pub mod win32;
