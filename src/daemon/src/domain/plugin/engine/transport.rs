// SPDX-License-Identifier: GPL-3.0-or-later
//! The transport a plugin drives, abstracted over the I/O shapes the daemon
//! exposes to scripts. A backend's `open` may yield any [`PluginIo`] shape — a
//! byte stream, a datagram/register bus, or a request/response capability — not
//! only a stream. The two canonical shapes:
//!
//! - [`PluginIo::Stream`] — a byte-stream `Transport` (HID, TCP, …). `write`/`read`.
//! - [`PluginIo::Register`] — an addressed register bus (SMBus). Ops carry an
//!   `(addr, cmd)` and run inside an atomic, scope-checked batch.
//!
//! See [`PluginIo`] for the full set of shapes.
//!
//! Which backend a plugin gets is decided by a [`PluginTransportDescriptor`]
//! registered next to the transport via `inventory::submit!` — the same
//! pattern built-in discovery roots use for `DeviceDescriptor`. Adding a bus is one
//! descriptor plus, if its I/O shape is new, a new `PluginIo` variant; the
//! plugin core (`manifest`/`worker`/`mod`) never grows a per-bus branch.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use halod_shared::types::{Permission, WriteRateStatus};

use crate::domain::registry::observers::discovery::DiscoveryHandle;
use crate::infrastructure::drivers::transports::smbus::{SmBusDevice, SmBusSyncOps};
use crate::infrastructure::drivers::transports::usb::{UsbCollection, UsbLocation};
use crate::infrastructure::drivers::transports::Transport;

use super::manifest::{DeviceSpec, PluginManifest};

/// The live transport handed to a plugin's worker (and to a `pre_scan`).
#[derive(Clone)]
pub enum PluginIo {
    /// A headless integration root. It has no byte-stream transport; its Lua
    /// worker exposes only host-scoped capabilities such as `halod.http`.
    None,
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
    Hwmon(Arc<crate::infrastructure::drivers::transports::hwmon::HwmonTransport>),
    /// Read-only AMD System Management Network access.  This is available only
    /// on Windows, where the typed PawnIO broker service exists.
    #[cfg(target_os = "windows")]
    AmdSmn(Arc<crate::infrastructure::drivers::transports::amd_smn::AmdSmnBus>),
    /// Typed LPC/SuperIO access.  The mutex serializes every operation against
    /// one PawnIO handle, including configuration-mode transitions.
    #[cfg(target_os = "windows")]
    Lpcio(Arc<crate::infrastructure::drivers::transports::lpcio::LpcIoTransport>),
}

impl PluginIo {
    pub fn write_group_key(&self) -> Option<usize> {
        match self {
            Self::Register(bus) => Some(bus.write_group_key()),
            _ => None,
        }
    }

    /// A deterministic transport failure that cannot be fixed by retrying the
    /// same plugin instance. The first such failure latches for its lifetime.
    pub fn unrecoverable_error(&self) -> Option<String> {
        match self {
            PluginIo::None => None,
            PluginIo::Register(bus) => bus.unrecoverable_error(),
            PluginIo::Command(command) => command.unrecoverable_error(),
            #[cfg(target_os = "linux")]
            PluginIo::Hwmon(bus) => bus.unrecoverable_error(),
            _ => None,
        }
    }

    pub fn usb_location(&self) -> Option<UsbLocation> {
        match self {
            PluginIo::Usb(c) => c.primary_location(),
            _ => None,
        }
    }

    /// Live write-rate/throughput for the Info UI, regardless of backend.
    pub fn rate_status(&self) -> WriteRateStatus {
        match self {
            PluginIo::None => WriteRateStatus::default(),
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
    #[cfg(feature = "plugin-test")]
    scripted: Option<Arc<std::sync::Mutex<std::collections::VecDeque<CommandRunResult>>>>,
}

/// The bounded outcome of one allowlisted command invocation. Failures before
/// a child exists (resolution/spawn) remain errors, so callers can distinguish
/// them from a child killed by the execution timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRunResult {
    pub success: bool,
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

impl CommandExecutor {
    pub fn new(commands: impl IntoIterator<Item = String>) -> Self {
        let mut allowed: Vec<String> = commands.into_iter().collect();
        allowed.sort();
        allowed.dedup();
        Self {
            allowed: allowed.into(),
            unrecoverable: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(feature = "plugin-test")]
            scripted: None,
        }
    }

    #[cfg(feature = "plugin-test")]
    pub fn scripted(
        commands: impl IntoIterator<Item = String>,
        results: impl IntoIterator<Item = CommandRunResult>,
    ) -> Self {
        let mut executor = Self::new(commands);
        executor.scripted = Some(Arc::new(std::sync::Mutex::new(
            results.into_iter().collect(),
        )));
        executor
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

    pub fn run(&self, executable: &str, args: &[String]) -> Result<CommandRunResult> {
        if !self.allowed.iter().any(|name| name == executable) {
            return self.reject(format!(
                "command '{executable}' is outside the declared transport scope"
            ));
        }
        if !super::manifest::is_allowed_command(executable) {
            return self.reject(format!(
                "command '{executable}' is outside HaloDaemon's executable allowlist"
            ));
        }
        if args.len() > MAX_COMMAND_ARGS
            || args
                .iter()
                .any(|arg| arg.len() > MAX_COMMAND_ARG_BYTES || arg.contains('\0'))
        {
            return self.reject("command arguments exceed the declared execution limits".into());
        }
        #[cfg(feature = "plugin-test")]
        if let Some(results) = &self.scripted {
            return results
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("no more scripted command results queued"));
        }
        self.run_system(executable, args, COMMAND_TIMEOUT)
    }

    fn run_system(
        &self,
        executable: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<CommandRunResult> {
        // Resolve once and execute that exact path. This keeps the runtime
        // behavior identical to the requirement probe and avoids a second,
        // potentially different PATH lookup between readiness and execution.
        let resolved = super::command_resolve::resolve(executable).ok_or_else(|| {
            let detail = format!("command '{executable}' is not executable on PATH");
            self.unrecoverable
                .lock()
                .unwrap()
                .get_or_insert(detail.clone());
            anyhow::anyhow!(detail)
        })?;
        let mut child = Command::new(&resolved)
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
        let mut timed_out = false;
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if started.elapsed() >= timeout {
                let _ = child.kill();
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = child.wait()?;
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
        Ok(CommandRunResult {
            success: status.success() && !timed_out,
            exit_code: status.code().unwrap_or(-1),
            stdout,
            stderr,
            timed_out,
        })
    }
}

/// Convert a command outcome without UTF-8 loss: Lua strings are byte strings,
/// which preserves command output exactly up to the existing output bound.
pub fn command_result_table(
    lua: &mlua::Lua,
    result: &CommandRunResult,
) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;
    table.set("success", result.success)?;
    table.set("exit_code", result.exit_code)?;
    table.set("stdout", lua.create_string(&result.stdout)?)?;
    table.set("stderr", lua.create_string(&result.stderr)?)?;
    table.set("timed_out", result.timed_out)?;
    Ok(table)
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

    pub fn write_group_key(&self) -> usize {
        Arc::as_ptr(&self.bus) as usize
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
    &crate::domain::plugin::ResolvedConfig,
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
    /// Open the live transport for a matched handle, returning any [`PluginIo`]
    /// shape (stream, datagram, or request/response). `config` is the plugin's
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
    use super::{command_result_table, CommandExecutor};

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
            .contains("outside HaloDaemon's executable allowlist"));
        assert!(commands.unrecoverable_error().is_some());
    }

    #[test]
    fn command_executor_returns_nonzero_exit_and_stderr() {
        let commands = CommandExecutor::new(["rustc".to_owned()]);
        let result = commands
            .run_system(
                "rustc",
                &["--definitely-not-a-rustc-option".to_owned()],
                super::COMMAND_TIMEOUT,
            )
            .unwrap();
        assert!(!result.success);
        assert_ne!(result.exit_code, 0);
        assert!(result.stdout.len() <= super::MAX_COMMAND_OUTPUT_BYTES);
        assert!(!result.stderr.is_empty());
        assert!(result.stderr.len() <= super::MAX_COMMAND_OUTPUT_BYTES);
        assert!(!result.timed_out);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_is_a_result_while_spawn_failure_is_an_error() {
        let commands = CommandExecutor::new(["definitely-missing-halod-command".to_owned()]);
        assert!(commands
            .run("definitely-missing-halod-command", &[])
            .is_err());

        let commands = CommandExecutor::new(["sleep".to_owned()]);
        let result = commands
            .run_system("sleep", &["1".to_owned()], std::time::Duration::ZERO)
            .expect("sleep should spawn");
        assert!(!result.success);
        assert!(result.timed_out);
    }

    #[test]
    fn command_result_lua_table_preserves_all_fields() {
        let lua = mlua::Lua::new();
        let result = super::CommandRunResult {
            success: false,
            exit_code: 7,
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
            timed_out: false,
        };
        let table = command_result_table(&lua, &result).unwrap();
        assert!(!table.get::<bool>("success").unwrap());
        assert_eq!(table.get::<i32>("exit_code").unwrap(), 7);
        assert_eq!(
            table.get::<mlua::LuaString>("stdout").unwrap().as_bytes(),
            b"out"
        );
        assert_eq!(
            table.get::<mlua::LuaString>("stderr").unwrap().as_bytes(),
            b"err"
        );
        assert!(!table.get::<bool>("timed_out").unwrap());
    }

    #[test]
    fn smbus_scope_violation_is_unrecoverable() {
        let scope = super::AddrScope::single(0x50);
        assert!(scope.check(0x51).is_err());
        assert!(scope.unrecoverable_error().is_some());
    }
}
