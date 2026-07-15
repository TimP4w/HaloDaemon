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

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use halod_shared::types::{Permission, WriteRateStatus};

use crate::drivers::transports::smbus::{SmBusDevice, SmBusSyncOps};
use crate::drivers::transports::usb_bulk::UsbBulkTransport;
use crate::drivers::transports::{ControlTransport, Transport};
use crate::registry::discovery::DiscoveryHandle;

use super::manifest::{DeviceSpec, PluginManifest};

/// A lazily-opened USB bulk-OUT endpoint paired with a HID stream transport, for
/// plugins that must push payloads larger than a HID report (LCD image frames).
/// The device is opened on first write, so plugins that never stream pay nothing.
pub enum BulkEndpoint {
    Usb {
        vid: u16,
        pid: u16,
        limit: Option<halod_shared::types::WriteRateLimit>,
        inner: std::sync::Mutex<Option<UsbBulkTransport>>,
    },
}

impl BulkEndpoint {
    pub fn new(
        vid: u16,
        pid: u16,
        limit: Option<halod_shared::types::WriteRateLimit>,
    ) -> Arc<Self> {
        Arc::new(Self::Usb {
            vid,
            pid,
            limit,
            inner: std::sync::Mutex::new(None),
        })
    }

    /// Write the whole payload to the bulk endpoint, opening the device on first
    /// use. `UsbBulkTransport::write` loops internally until every byte is sent.
    pub fn write(&self, data: &[u8]) -> Result<()> {
        match self {
            BulkEndpoint::Usb {
                vid,
                pid,
                limit,
                inner,
            } => {
                let mut guard = inner
                    .lock()
                    .map_err(|_| anyhow::anyhow!("plugin bulk endpoint mutex poisoned"))?;
                let transport = match &mut *guard {
                    Some(t) => t,
                    none => none.insert(UsbBulkTransport::open(*vid, *pid, *limit)?),
                };
                transport.write(data)?;
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
    /// One or more USB vendor-control endpoints (DDC/CI, ENE RGB controllers).
    /// Unlike the byte-stream/register shapes, a control device can bundle
    /// *several* physical USB devices behind one plugin — a plugin that presents
    /// two chips as a single device (e.g. a monitor's DDC controller plus its
    /// Ambiglow LED controller) declares the extra ones in its manifest and
    /// reaches them by name.
    Control(ControlEndpoints),
    Command(CommandExecutor),
    /// Read-only AMD System Management Network access.  This is available only
    /// on Windows, where the typed PawnIO broker service exists.
    #[cfg(target_os = "windows")]
    AmdSmn(Arc<crate::drivers::transports::amd_smn::AmdSmnBus>),
    /// Typed LPC/SuperIO access.  The mutex serializes every operation against
    /// one PawnIO handle, including configuration-mode transitions.
    #[cfg(target_os = "windows")]
    Lpcio(Arc<crate::drivers::transports::lpcio::LpcIoTransport>),
}

impl PluginIo {
    /// Live write-rate/throughput for the Info UI, regardless of backend.
    pub fn rate_status(&self) -> WriteRateStatus {
        match self {
            PluginIo::Stream { transport, .. } => transport.rate_status(),
            PluginIo::Register(r) => r.rate_status(),
            PluginIo::Control(c) => c.rate_status(),
            PluginIo::Command(_) => WriteRateStatus::default(),
            #[cfg(target_os = "windows")]
            PluginIo::AmdSmn(_) => WriteRateStatus::default(),
            #[cfg(target_os = "windows")]
            PluginIo::Lpcio(bus) => bus.rate_status(),
        }
    }

    /// Restore host-managed safety-critical state. This is deliberately
    /// independent of the Lua worker so cleanup still works after a timeout.
    pub fn restore_safety_state(&self) {
        #[cfg(target_os = "windows")]
        if let PluginIo::Lpcio(transport) = self {
            if let Err(error) = transport.restore() {
                log::error!("restoring plugin LPCIO state: {error:#}");
            }
        }
    }
}

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_COMMAND_ARGS: usize = 64;
const MAX_COMMAND_ARG_BYTES: usize = 4096;
const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;

/// Direct, allowlisted process execution for command-backed plugins. No shell
/// is involved: the manifest grants only bare executable names and Lua supplies
/// a bounded argv vector.
#[derive(Clone)]
pub struct CommandExecutor {
    allowed: Arc<[String]>,
}

impl CommandExecutor {
    pub fn new(commands: impl IntoIterator<Item = String>) -> Self {
        let mut allowed: Vec<String> = commands.into_iter().collect();
        allowed.sort();
        allowed.dedup();
        Self {
            allowed: allowed.into(),
        }
    }

    pub fn run(&self, executable: &str, args: &[String]) -> Result<Vec<u8>> {
        if !self.allowed.iter().any(|name| name == executable) {
            anyhow::bail!("command '{executable}' is outside the declared transport scope");
        }
        if args.len() > MAX_COMMAND_ARGS
            || args
                .iter()
                .any(|arg| arg.len() > MAX_COMMAND_ARG_BYTES || arg.contains('\0'))
        {
            anyhow::bail!("command arguments exceed the declared execution limits");
        }
        let mut child = Command::new(executable)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawning '{executable}' failed: {e}"))?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let read = |pipe: std::process::ChildStdout| -> Result<Vec<u8>> {
            let mut bytes = Vec::new();
            pipe.take((MAX_COMMAND_OUTPUT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            Ok(bytes)
        };
        let out = std::thread::spawn(move || read(stdout));
        let err = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr
                .take((MAX_COMMAND_OUTPUT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            Result::<Vec<u8>>::Ok(bytes)
        });
        let started = Instant::now();
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if started.elapsed() >= COMMAND_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("command '{executable}' exceeded its execution timeout");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let stdout = out
            .join()
            .map_err(|_| anyhow::anyhow!("command stdout reader panicked"))??;
        let stderr = err
            .join()
            .map_err(|_| anyhow::anyhow!("command stderr reader panicked"))??;
        if stdout.len() > MAX_COMMAND_OUTPUT_BYTES || stderr.len() > MAX_COMMAND_OUTPUT_BYTES {
            anyhow::bail!("command '{executable}' exceeded its output limit");
        }
        if !stderr.is_empty() {
            log::debug!(
                "plugin command {executable} stderr: {}",
                String::from_utf8_lossy(&stderr)
            );
        }
        Ok(stdout)
    }
}

/// A set of named USB vendor-control endpoints. The device the discovery handle
/// matched lives under [`ControlEndpoints::PRIMARY`] (the empty string); any
/// secondary endpoints a plugin declares (`transports.usb_control.endpoints`)
/// are keyed by their declared id. A script reaches each through the `transport`
/// object's `control_write`/`control_read`, naming the endpoint (`""` = primary).
#[derive(Clone)]
pub struct ControlEndpoints {
    endpoints: Arc<HashMap<String, Arc<dyn ControlTransport>>>,
}

impl ControlEndpoints {
    /// Key under which the matched (primary) device is stored.
    pub const PRIMARY: &'static str = "";

    pub fn new(endpoints: HashMap<String, Arc<dyn ControlTransport>>) -> Self {
        Self {
            endpoints: Arc::new(endpoints),
        }
    }

    /// The endpoint registered under `name`, or `None` if the manifest never
    /// declared it (`""` is always the matched device).
    pub fn get(&self, name: &str) -> Option<&Arc<dyn ControlTransport>> {
        self.endpoints.get(name)
    }

    fn rate_status(&self) -> WriteRateStatus {
        self.get(Self::PRIMARY)
            .map(|t| t.rate_status())
            .unwrap_or_default()
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
type PluginOpenFn = fn(
    &PluginManifest,
    &DiscoveryHandle<'_>,
    &HashMap<String, String>,
    &[Permission],
    Option<halod_shared::types::WriteRateLimit>,
) -> Result<PluginIo>;

pub struct PluginTransportDescriptor {
    /// The `match.transport` discriminator (e.g. "hid", "smbus").
    pub kind: &'static str,
    /// Does this spec (of this kind) accept the discovered handle? `None` for a
    /// backend that is config-instantiated rather than discovery-matched (the
    /// `tcp` integration transport), which is reached via `open` directly and
    /// never through handle matching.
    pub matches: Option<fn(&DeviceSpec, &DiscoveryHandle<'_>) -> bool>,
    /// Open the live transport for a matched handle. `config` is the plugin's
    /// resolved non-secure config values (see `plugins::config_for`) — HID/
    /// SMBus ignore it; the `tcp` backend reads its host/port keys from it,
    /// since a config-instantiated integration has no real discovery handle.
    /// `granted` is the plugin's granted permissions — a backend that reaches
    /// off the matched device (the `tcp` backend) gates on `Permission::Network`.
    pub open: PluginOpenFn,
    /// Stable per-device id suffix from the matched handle. `None` for a
    /// config-instantiated backend, whose id is built from its config, not a handle.
    pub id_suffix: Option<fn(&DiscoveryHandle<'_>) -> String>,
    /// Reject a manifest whose match spec omits a field this kind requires. `None`
    /// for a config-instantiated backend (an integration declares no device specs).
    pub validate: Option<fn(&DeviceSpec) -> Result<()>>,
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

#[cfg(test)]
mod tests {
    use super::CommandExecutor;

    #[test]
    fn command_executor_rejects_undeclared_executable() {
        let commands = CommandExecutor::new(["nvidia-smi".to_owned()]);
        let error = commands.run("sh", &[]).unwrap_err();
        assert!(error
            .to_string()
            .contains("outside the declared transport scope"));
    }
}
