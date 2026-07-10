// SPDX-License-Identifier: GPL-3.0-or-later
//! The transport a plugin drives, abstracted over the two I/O shapes the daemon
//! exposes to scripts:
//!
//! - [`PluginIo::Stream`] — a byte-stream `Transport` (HID today). `write`/`read`.
//! - [`PluginIo::Register`] — an addressed register bus (SMBus today). Ops carry
//!   an `(addr, cmd)` and run inside an atomic, scope-checked batch.
//!
//! Which backend a plugin gets is decided by a [`PluginTransportDescriptor`]
//! registered next to the transport via `inventory::submit!` — the same
//! pattern native drivers use for `DeviceDescriptor`. Adding a bus is one
//! descriptor plus, if its I/O shape is new, a new `PluginIo` variant; the
//! plugin core (`manifest`/`worker`/`mod`) never grows a per-bus branch.

use std::sync::Arc;

use anyhow::Result;
use halod_shared::types::{WriteRateLimit, WriteRateStatus};

use crate::drivers::transports::smbus::{SmBusDevice, SmBusSyncOps};
use crate::drivers::transports::usb_bulk::UsbBulkTransport;
use crate::drivers::transports::Transport;
use crate::registry::discovery::DiscoveryHandle;

use super::manifest::{MatchSpec, PluginManifest};

/// A lazily-opened USB bulk-OUT endpoint paired with a HID stream transport, for
/// plugins that must push payloads larger than a HID report (LCD image frames).
/// The device is opened on first write, so plugins that never stream pay nothing.
pub enum BulkEndpoint {
    Usb {
        vid: u16,
        pid: u16,
        inner: std::sync::Mutex<Option<UsbBulkTransport>>,
    },
    /// Records every write instead of touching hardware (tests only).
    #[cfg(test)]
    Recording(std::sync::Mutex<Vec<Vec<u8>>>),
}

impl BulkEndpoint {
    pub fn new(vid: u16, pid: u16) -> Arc<Self> {
        Arc::new(Self::Usb {
            vid,
            pid,
            inner: std::sync::Mutex::new(None),
        })
    }

    #[cfg(test)]
    pub fn recording() -> Arc<Self> {
        Arc::new(Self::Recording(std::sync::Mutex::new(Vec::new())))
    }

    #[cfg(test)]
    pub fn recorded(&self) -> Vec<Vec<u8>> {
        match self {
            BulkEndpoint::Recording(m) => m.lock().unwrap().clone(),
            _ => Vec::new(),
        }
    }

    /// Write the whole payload to the bulk endpoint, opening the device on first
    /// use. `UsbBulkTransport::write` loops internally until every byte is sent.
    pub fn write(&self, data: &[u8]) -> Result<()> {
        match self {
            BulkEndpoint::Usb { vid, pid, inner } => {
                let mut guard = inner
                    .lock()
                    .map_err(|_| anyhow::anyhow!("plugin bulk endpoint mutex poisoned"))?;
                if guard.is_none() {
                    *guard = Some(UsbBulkTransport::open(*vid, *pid, None)?);
                }
                guard.as_ref().unwrap().write(data)?;
                Ok(())
            }
            #[cfg(test)]
            BulkEndpoint::Recording(m) => {
                m.lock().unwrap().push(data.to_vec());
                Ok(())
            }
        }
    }
}

/// The live transport handed to a plugin's worker (and to a `pre_scan`).
#[derive(Clone)]
pub enum PluginIo {
    Stream {
        transport: Arc<dyn Transport>,
        /// Companion bulk endpoint for HID devices that also expose one (e.g. an
        /// LCD panel). `None` when the device has no bulk endpoint.
        bulk: Option<Arc<BulkEndpoint>>,
    },
    Register(RegisterBus),
}

impl PluginIo {
    /// Live write-rate/throughput for the Info UI, regardless of backend.
    pub fn rate_status(&self) -> WriteRateStatus {
        match self {
            PluginIo::Stream { transport, .. } => transport.rate_status(),
            PluginIo::Register(r) => r.rate_status(),
        }
    }

    pub fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        match self {
            PluginIo::Stream { transport, .. } => transport.set_write_rate_limit(limit),
            PluginIo::Register(r) => r.set_write_rate_limit(limit),
        }
    }
}

/// The set of SMBus addresses a plugin is allowed to touch through a
/// [`RegisterBus`]. A register op naming any other address is a hard error, so
/// the declared address list is the security boundary — a plugin can never
/// free-roam the bus (unlike a raw `Scan(bus)` model).
#[derive(Clone)]
pub struct AddrScope {
    allowed: Arc<[u8]>,
}

impl AddrScope {
    pub fn new(addrs: impl IntoIterator<Item = u8>) -> Self {
        let mut v: Vec<u8> = addrs.into_iter().collect();
        v.sort_unstable();
        v.dedup();
        Self { allowed: v.into() }
    }

    pub fn single(addr: u8) -> Self {
        Self::new([addr])
    }

    pub fn permits(&self, addr: u8) -> bool {
        self.allowed.contains(&addr)
    }

    pub fn check(&self, addr: u8) -> Result<()> {
        if self.permits(addr) {
            Ok(())
        } else {
            anyhow::bail!(
                "plugin SMBus access to address 0x{addr:02x} is outside its declared scope"
            )
        }
    }
}

/// A register-addressed bus scoped to a plugin's declared addresses. Wraps the
/// metered [`SmBusDevice`]; every op is tallied and rate-limited through it.
#[derive(Clone)]
pub struct RegisterBus {
    bus: Arc<SmBusDevice>,
    scope: AddrScope,
}

impl RegisterBus {
    pub fn new(bus: Arc<SmBusDevice>, scope: AddrScope) -> Self {
        Self { bus, scope }
    }

    pub fn rate_status(&self) -> WriteRateStatus {
        self.bus.rate_status().unwrap_or_default()
    }

    pub fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.bus.set_write_rate_limit(limit);
    }

    /// Run `f` against the raw ops and the address scope in one atomic bus-lock
    /// hold, on the calling thread. `f` typically drives a plugin's Lua batch
    /// callback — hence the inline (non-`spawn_blocking`) primitive. The caller
    /// must be off the async runtime (the plugin worker / pre-scan `std::thread`).
    pub(crate) fn run_local<R>(
        &self,
        f: impl FnOnce(&mut dyn SmBusSyncOps, &AddrScope) -> Result<R>,
    ) -> Result<R> {
        let scope = &self.scope;
        self.bus.run_batch_local(move |ops| f(ops, scope))
    }
}

/// A plugin transport backend, registered next to the transport it wraps.
/// `descriptor_for(kind)` resolves the declared `match.transport` string to one
/// of these; the plugin core drives everything through it.
pub struct PluginTransportDescriptor {
    /// The `match.transport` discriminator (e.g. "hid", "smbus").
    pub kind: &'static str,
    /// Does this spec (of this kind) accept the discovered handle?
    pub matches: fn(&MatchSpec, &DiscoveryHandle<'_>) -> bool,
    /// Open the live transport for a matched handle.
    pub open: fn(&PluginManifest, &DiscoveryHandle<'_>) -> Result<PluginIo>,
    /// Stable per-device id suffix from the matched handle.
    pub id_suffix: fn(&DiscoveryHandle<'_>) -> String,
    /// Reject a manifest whose match spec omits a field this kind requires.
    pub validate: fn(&MatchSpec) -> Result<()>,
}
inventory::collect!(PluginTransportDescriptor);

/// Resolve a `match.transport` kind to its registered backend.
pub fn descriptor_for(kind: &str) -> Option<&'static PluginTransportDescriptor> {
    inventory::iter::<PluginTransportDescriptor>().find(|d| d.kind == kind)
}

/// Every registered backend kind, for error messages.
pub fn known_kinds() -> Vec<&'static str> {
    inventory::iter::<PluginTransportDescriptor>()
        .map(|d| d.kind)
        .collect()
}
