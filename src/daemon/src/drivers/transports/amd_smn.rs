// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "windows")]

//! AMD SMN (System Management Network) access over PawnIO.
//!
//! The daemon uses a typed broker client; only the elevated broker knows that
//! this maps to PawnIO's `AMDFamily17.bin` and `ioctl_read_smn`.
//!
//! Only SMN reads are needed for CPU temperatures — MSR access (used by
//! LibreHardwareMonitor for power/clocks) is deliberately not exposed here.

use anyhow::Result;

use crate::drivers::transports::register_ops;

pub struct AmdSmnBus {
    io: register_ops::AmdSmnBrokerClient,
}

impl AmdSmnBus {
    /// Open a fresh typed AMD SMN handle in the broker.
    pub fn open() -> Result<Self> {
        Ok(Self {
            io: register_ops::open_amd_smn()?,
        })
    }

    /// Read one 32-bit SMN register at `offset`.
    pub fn read_smn(&self, offset: u32) -> Result<u32> {
        self.io.read_smn(offset)
    }
}
