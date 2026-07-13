// SPDX-License-Identifier: GPL-3.0-or-later
//! The register-bus access seam.
//!
//! Every register-bus device obtains its raw ops here rather than opening
//! `halod-hwaccess` directly, so *where* those ops run is decided in one place:
//!
//!   - **Linux, or a Windows build with the broker disabled** — the direct
//!     in-process `halod-hwaccess` impl (Linux SMBus access is gated by
//!     `/dev/i2c-*` permissions, not elevation, so no broker is needed).
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

/// Which implementation the register-bus seam resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Open `halod-hwaccess` in-process.
    Direct,
    /// Talk to the elevated broker over the named pipe.
    Broker,
}

/// Pure backend decision, factored out so it is unit-testable without actually
/// being on Windows: the broker is used only on Windows and only when it is not
/// explicitly disabled.
pub fn backend_for(is_windows: bool, force_direct: bool) -> Backend {
    if is_windows && !force_direct {
        Backend::Broker
    } else {
        Backend::Direct
    }
}

fn backend() -> Backend {
    backend_for(cfg!(windows), std::env::var_os("HALOD_NO_BROKER").is_some())
}

#[cfg(target_os = "windows")]
mod win_client;

/// Open the register bus described by `info`, returning ops the caller can
/// meter and lock behind a `SmBusDevice` exactly as before.
pub fn open_smbus(info: &BusInfo, addresses: &[u8]) -> Result<Box<dyn SmBusSyncOps + Send>> {
    match backend() {
        Backend::Direct => halod_hwaccess::smbus::open_bus(info),
        #[cfg(target_os = "windows")]
        Backend::Broker => win_client::open_bus(info, addresses),
        // On non-Windows `backend()` never returns `Broker`.
        #[cfg(not(target_os = "windows"))]
        Backend::Broker => unreachable!("broker backend is Windows-only"),
    }
}

/// Open a PawnIO module into its own ops handle (LpcIO / AMD SMN). Windows-only
/// — PawnIO does not exist on other platforms.
#[cfg(target_os = "windows")]
pub fn open_pawnio(module: &str) -> Result<Box<dyn PawnioOps>> {
    match backend() {
        Backend::Direct => Ok(Box::new(halod_hwaccess::pawnio::PawnioModule::open(&[
            module,
        ])?)),
        Backend::Broker => win_client::open_pawnio(module),
    }
}

#[cfg(test)]
mod tests {
    use super::{backend_for, Backend};

    #[test]
    fn linux_always_direct() {
        assert_eq!(backend_for(false, false), Backend::Direct);
        assert_eq!(backend_for(false, true), Backend::Direct);
    }

    #[test]
    fn windows_uses_broker_unless_forced_direct() {
        assert_eq!(backend_for(true, false), Backend::Broker);
        assert_eq!(backend_for(true, true), Backend::Direct);
    }
}
