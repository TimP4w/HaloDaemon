// SPDX-License-Identifier: GPL-3.0-or-later
//! Downloads a signed HaloDaemon installer, verifies it and runs it elevated.

pub mod args;
pub mod download;
pub mod log_file;
#[cfg(windows)]
pub mod run;
pub mod verify;
