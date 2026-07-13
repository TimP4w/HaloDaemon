// SPDX-License-Identifier: GPL-3.0-or-later
//! The register-bus access seam.
//!
//! Every register-bus device obtains its raw ops here rather than opening
//! `halod-hwaccess` directly, so *where* those ops run is decided in one place:
//!
//!   - **Linux** — the direct in-process `halod-hwaccess` implementation. Linux
//!     SMBus access is gated by `/dev/i2c-*` permissions, not process elevation,
//!     so no broker is needed.
//!   - **Windows** — an RPC client to the elevated `halod-broker` process over
//!     its named pipe. The client brings the broker up on first use: an
//!     installed on-demand `HalodBroker` service is started via the SCM (no
//!     UAC); a dev run with no service falls back to one `runas` UAC prompt.
//!
//! The trait boundaries (`SmBusSyncOps` for a bus, `PawnioOps` for a PawnIO
//! module) are unchanged, so every call site is agnostic to which backend it
//! got — that is the entire point of keeping the seam here.

use anyhow::Result;

use halod_hwaccess::smbus::{BusInfo, SmBusSyncOps};

#[cfg(target_os = "windows")]
use halod_hwaccess::pawnio::PawnioOps;

#[cfg(target_os = "windows")]
mod win_client;

/// Open the register bus described by `info`, returning ops the caller can
/// meter and lock behind a `SmBusDevice` exactly as before.
pub fn open_smbus(info: &BusInfo, addresses: &[u8]) -> Result<Box<dyn SmBusSyncOps + Send>> {
    #[cfg(target_os = "windows")]
    {
        win_client::open_bus(info, addresses)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = addresses;
        halod_hwaccess::smbus::open_bus(info)
    }
}

/// Open a PawnIO module into its own ops handle (LpcIO / AMD SMN). Windows-only
/// — PawnIO does not exist on other platforms.
#[cfg(target_os = "windows")]
pub fn open_pawnio(module: &str) -> Result<Box<dyn PawnioOps>> {
    win_client::open_pawnio(module)
}
