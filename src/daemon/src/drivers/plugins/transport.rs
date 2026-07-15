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
//! pattern built-in discovery roots use for `DeviceDescriptor`. Adding a bus is one
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
use crate::drivers::transports::usb::UsbCollection;
use crate::drivers::transports::Transport;
use crate::registry::discovery::DiscoveryHandle;

use super::manifest::{DeviceSpec, PluginManifest};

/// The live transport handed to a plugin's worker (and to a `pre_scan`).
#[derive(Clone)]
pub enum PluginIo {
    Stream {
        transport: Arc<dyn Transport>,
        /// General USB endpoint collection attached to a composite HID worker.
        usb: Option<Arc<dyn UsbCollection>>,
    },
    Register(RegisterBus),
    Usb(Arc<dyn UsbCollection>),
    Command(CommandExecutor),
    /// Scoped Linux hwmon collection used by the local hwmon integration.
    #[cfg(target_os = "linux")]
    Hwmon(Arc<crate::drivers::transports::hwmon::HwmonTransport>),
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
    /// A deterministic transport failure that cannot be fixed by retrying the
    /// same plugin instance. The first such failure latches for its lifetime.
    pub fn unrecoverable_error(&self) -> Option<String> {
        match self {
            PluginIo::Register(bus) => bus.unrecoverable_error(),
            PluginIo::Command(command) => command.unrecoverable_error(),
            #[cfg(target_os = "linux")]
            PluginIo::Hwmon(bus) => bus.unrecoverable_error(),
            _ => None,
        }
    }

    /// Live write-rate/throughput for the Info UI, regardless of backend.
    pub fn rate_status(&self) -> WriteRateStatus {
        match self {
            PluginIo::Stream { transport, .. } => transport.rate_status(),
            PluginIo::Register(r) => r.rate_status(),
            PluginIo::Usb(c) => c.rate_status(),
            PluginIo::Command(_) => WriteRateStatus::default(),
            #[cfg(target_os = "linux")]
            PluginIo::Hwmon(bus) => bus.rate_status(),
            #[cfg(target_os = "windows")]
            PluginIo::AmdSmn(_) => WriteRateStatus::default(),
            #[cfg(target_os = "windows")]
            PluginIo::Lpcio(bus) => bus.rate_status(),
        }
    }

    /// Restore host-managed safety-critical state. This is deliberately
    /// independent of the Lua worker so cleanup still works after a timeout.
    pub fn restore_safety_state(&self) {
        #[cfg(target_os = "linux")]
        if let PluginIo::Hwmon(transport) = self {
            if let Err(error) = transport.restore() {
                log::error!("restoring plugin hwmon state: {error:#}");
            }
        }
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
    unrecoverable: Arc<std::sync::Mutex<Option<String>>>,
}

impl CommandExecutor {
    pub fn new(commands: impl IntoIterator<Item = String>) -> Self {
        let mut allowed: Vec<String> = commands.into_iter().collect();
        allowed.sort();
        allowed.dedup();
        Self {
            allowed: allowed.into(),
            unrecoverable: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn reject<T>(&self, detail: String) -> Result<T> {
        self.unrecoverable
            .lock()
            .unwrap()
            .get_or_insert(detail.clone());
        anyhow::bail!(detail)
    }

    fn unrecoverable_error(&self) -> Option<String> {
        self.unrecoverable.lock().unwrap().clone()
    }

    pub fn run(&self, executable: &str, args: &[String]) -> Result<Vec<u8>> {
        if !self.allowed.iter().any(|name| name == executable) {
            return self.reject(format!(
                "command '{executable}' is outside the declared transport scope"
            ));
        }
        if super::manifest::is_disallowed_command(executable) {
            return self.reject(format!(
                "command '{executable}' is a shell, interpreter, or command launcher and cannot be run by a plugin"
            ));
        }
        if args.len() > MAX_COMMAND_ARGS
            || args
                .iter()
                .any(|arg| arg.len() > MAX_COMMAND_ARG_BYTES || arg.contains('\0'))
        {
            return self.reject("command arguments exceed the declared execution limits".into());
        }
        let mut child = Command::new(executable)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                let detail = format!("spawning '{executable}' failed: {error}");
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) {
                    self.unrecoverable
                        .lock()
                        .unwrap()
                        .get_or_insert(detail.clone());
                }
                anyhow::anyhow!(detail)
            })?;
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

/// The set of SMBus addresses a plugin is allowed to touch through a
/// [`RegisterBus`]. A register op naming any other address is a hard error, so
/// the declared address list is the security boundary — a plugin can never
/// free-roam the bus (unlike a raw `Scan(bus)` model).
#[derive(Clone)]
pub struct AddrScope {
    allowed: Arc<[u8]>,
    unrecoverable: Arc<std::sync::Mutex<Option<String>>>,
}

impl AddrScope {
    pub fn new(addrs: impl IntoIterator<Item = u8>) -> Self {
        let mut v: Vec<u8> = addrs.into_iter().collect();
        v.sort_unstable();
        v.dedup();
        Self {
            allowed: v.into(),
            unrecoverable: Arc::new(std::sync::Mutex::new(None)),
        }
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
            let detail = format!(
                "plugin SMBus access to address 0x{addr:02x} is outside its declared scope"
            );
            self.unrecoverable
                .lock()
                .unwrap()
                .get_or_insert(detail.clone());
            anyhow::bail!(detail)
        }
    }

    fn unrecoverable_error(&self) -> Option<String> {
        self.unrecoverable.lock().unwrap().clone()
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

    fn unrecoverable_error(&self) -> Option<String> {
        self.scope.unrecoverable_error()
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
        assert!(commands.unrecoverable_error().is_some());
    }

    #[test]
    fn command_executor_rejects_allowlisted_interpreter() {
        let commands = CommandExecutor::new(["python3".to_owned()]);
        let error = commands
            .run("python3", &["-c".to_owned(), "print('unsafe')".to_owned()])
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("shell, interpreter, or command launcher"));
        assert!(commands.unrecoverable_error().is_some());
    }

    #[test]
    fn smbus_scope_violation_is_unrecoverable() {
        let scope = super::AddrScope::single(0x50);
        assert!(scope.check(0x51).is_err());
        assert!(scope.unrecoverable_error().is_some());
    }
}
