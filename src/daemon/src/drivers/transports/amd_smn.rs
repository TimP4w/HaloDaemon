// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "windows")]

//! AMD SMN (System Management Network) access over PawnIO.
//!
//! Wraps PawnIO's `AMDFamily17.bin` module, which exposes `ioctl_read_smn` —
//! a single-register read of the on-die System Management Network used by the
//! Zen family (17h/19h/1Ah) for thermal and telemetry registers. The module is
//! loaded into its own [`PawnioModule`] handle; PawnIO serialises access to
//! that handle internally, so a single `AmdSmnBus` is safe to share via `Arc`.
//!
//! Only SMN reads are needed for CPU temperatures — MSR access (used by
//! LibreHardwareMonitor for power/clocks) is deliberately not exposed here.

use anyhow::{bail, Result};

use super::pawnio::PawnioModule;
use crate::drivers::Metered;
use halod_shared::types::{WriteRateLimit, WriteRateStatus};

pub struct AmdSmnBus {
    /// Gated like every other transport for a uniform write-rate surface,
    /// even though this bus only ever reads: `rate_status` legitimately
    /// reports zero writes, and if a write op is ever added here it's
    /// already behind the gate.
    io: Metered<PawnioModule>,
}

impl AmdSmnBus {
    /// Open a fresh PawnIO handle with the AMD Family 17h+ module loaded.
    pub fn open(limit: Option<WriteRateLimit>) -> Result<Self> {
        Ok(Self {
            io: Metered::new(PawnioModule::open(&["AMDFamily17.bin"])?, limit),
        })
    }

    /// Read one 32-bit SMN register at `offset`.
    pub fn read_smn(&self, offset: u32) -> Result<u32> {
        let mut out = [0u64; 1];
        let n = self
            .io
            .read_access()
            .exec(c"ioctl_read_smn", &[offset as u64], &mut out)?;
        if n == 0 {
            bail!("ioctl_read_smn(0x{offset:08X}) returned no data");
        }
        Ok((out[0] & 0xFFFF_FFFF) as u32)
    }

    pub fn rate_status(&self) -> WriteRateStatus {
        self.io.status()
    }

    pub fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.io.set_limit(limit);
    }
}
