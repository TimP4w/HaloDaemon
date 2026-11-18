// SPDX-License-Identifier: GPL-3.0-or-later
//! Identity-gated, capability-scoped broker RPC server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use halod_hwaccess::pawnio::PawnioModule;
use halod_hwaccess::proto::{
    self, BrokerError, CapabilityScope, Request, Response, MAX_OPERATIONS_PER_SECOND,
    MAX_SCOPE_ADDRESSES, PIPE_NAME,
};
use halod_hwaccess::smbus::{self, SmBusSyncOps, SMBUS_BLOCK_MAX};
use halod_hwaccess::winsec;

use crate::clientauth::{self, Admission, Gate};
use crate::pipe::{
    create_instance, disconnect_handle_value, wait_for_client, PipeSecurity, PipeStream,
};

const MAX_CLIENTS: usize = 32;
const MAX_HANDLES_PER_KIND: usize = 64;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_PENDING_REFUSALS: usize = 4;
const REFUSAL_LINGER: Duration = Duration::from_secs(2);

static PENDING_REFUSALS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokerConfig {
    pub coordinators: Vec<String>,
    /// Require clients to be `halod.exe` next to this broker (installed mode).
    pub pin_client_image: bool,
}

/// Foreground (dev/UAC) mode: the coordinator is the SID on the command line
/// the user just approved. No image pinning: a dev tree is user-writable.
pub fn foreground_config_from_args(args: &[String]) -> Result<BrokerConfig> {
    let sid = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--coordinator-sid="))
        .filter(|sid| !sid.is_empty())
        .ok_or_else(|| anyhow!("missing required broker argument --coordinator-sid"))?;
    Ok(BrokerConfig {
        coordinators: vec![sid.to_owned()],
        pin_client_image: false,
    })
}

static CONFIG: OnceLock<BrokerConfig> = OnceLock::new();
static GATE: Mutex<Gate> = Mutex::new(Gate::new());

/// Coordinators are stored canonically: token SIDs never arrive as aliases.
pub fn configure(mut config: BrokerConfig) -> Result<()> {
    config.coordinators = config
        .coordinators
        .iter()
        .map(|sid| winsec::canonical_sid(sid))
        .collect::<Result<_>>()?;
    winsec::coordinator_dacl_sddl(&config.coordinators)?;
    CONFIG
        .set(config)
        .map_err(|_| anyhow!("broker already configured"))
}

fn config() -> Result<&'static BrokerConfig> {
    CONFIG
        .get()
        .ok_or_else(|| anyhow!("broker was not configured"))
}

fn gate() -> MutexGuard<'static, Gate> {
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn serve_forever() -> Result<()> {
    let config = config()?.clone();
    let sec = PipeSecurity::from_sddl(&winsec::coordinator_dacl_sddl(&config.coordinators)?)?;
    let expected_client: Option<PathBuf> = if config.pin_client_image {
        Some(winsec::installed_sibling("halod.exe")?)
    } else {
        None
    };
    log::info!("pipe secured to coordinators {:?}", config.coordinators);

    // Retain every worker JoinHandle and reap completed threads before the next
    // accept. The active cap bounds this vector even while accept is blocked.
    let (done_tx, done_rx) = mpsc::channel::<usize>();
    let mut workers: HashMap<usize, std::thread::JoinHandle<()>> = HashMap::new();
    let mut next_worker = 0usize;

    loop {
        while let Ok(id) = done_rx.try_recv() {
            if let Some(worker) = workers.remove(&id) {
                let _ = worker.join();
            }
        }
        if gate().active() >= MAX_CLIENTS {
            // Do not attempt to create a 33rd instance: CreateNamedPipeW would
            // fail at its explicit instance ceiling and tear down the accept
            // loop. Wait for one tracked worker to finish instead.
            if let Ok(id) = done_rx.recv() {
                if let Some(worker) = workers.remove(&id) {
                    let _ = worker.join();
                }
            }
            continue;
        }

        // Workers and refusals close their instance before releasing their
        // count, so with neither active no instance of ours exists and any
        // other owner of the name is a squatter.
        let first = gate().active() == 0 && PENDING_REFUSALS.load(Ordering::SeqCst) == 0;
        let handle = match create_instance(PIPE_NAME, &sec, first) {
            Ok(handle) => handle,
            Err(e) if first => {
                return Err(e.context(format!("{PIPE_NAME} is already owned by another process")))
            }
            Err(e) => {
                log::warn!("create pipe instance failed, retrying: {e}");
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        let stream = PipeStream::new(handle);
        if let Err(e) = wait_for_client(handle) {
            log::warn!("wait_for_client failed: {e}");
            continue;
        }

        let identity = match clientauth::client_identity(handle) {
            Ok(id) => id,
            Err(e) => {
                refuse(stream, format!("could not identify pipe client: {e}"));
                continue;
            }
        };
        let image = match &expected_client {
            Some(expected) => match clientauth::client_image_matches(identity.pid, expected) {
                Ok(image) => image,
                Err(actual) => {
                    refuse(
                        stream,
                        format!(
                            "client pid {} image {} is not {}",
                            identity.pid,
                            actual.display(),
                            expected.display()
                        ),
                    );
                    continue;
                }
            },
            None => winsec::process_image_path(identity.pid).unwrap_or_default(),
        };
        match clientauth::decide(
            &mut gate(),
            &identity.sid,
            &config.coordinators,
            MAX_CLIENTS,
        ) {
            Admission::Ok => {
                log::info!(
                    "accepted coordinator {} session {} image {}",
                    identity.sid,
                    identity.session,
                    image.display()
                );
                next_worker = next_worker.checked_add(1).unwrap_or(1);
                let id = next_worker;
                let done = done_tx.clone();
                let worker = std::thread::Builder::new()
                    .name(format!("halod-broker-{id}"))
                    .spawn(move || {
                        serve(stream);
                        clientauth::release(&mut gate());
                        let _ = done.send(id);
                    })?;
                workers.insert(id, worker);
            }
            Admission::WrongUser => refuse(
                stream,
                format!(
                    "client SID {} session {} is not a configured coordinator",
                    identity.sid, identity.session
                ),
            ),
            Admission::TooMany => refuse(
                stream,
                format!("{MAX_CLIENTS} broker connections already active"),
            ),
        }
    }
}

/// Tell a refused client why, so it fails fast instead of retrying. Closing
/// the instance discards unread data, so it lingers until the client hangs up,
/// bounded in time and in concurrent refusals; past the bound the reply is
/// best-effort.
fn refuse(mut stream: PipeStream, reason: String) {
    log::warn!("refusing client: {reason}");
    let reply = Response::Error(BrokerError::Failed(format!("broker refused: {reason}")));
    if PENDING_REFUSALS.fetch_add(1, Ordering::SeqCst) >= MAX_PENDING_REFUSALS {
        PENDING_REFUSALS.fetch_sub(1, Ordering::SeqCst);
        let _ = proto::write_frame(&mut stream, &reply);
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("halod-broker-refusal".into())
        .spawn(move || {
            if proto::write_frame(&mut stream, &reply).is_ok() {
                stream.wait_closed(REFUSAL_LINGER);
            }
            drop(stream);
            PENDING_REFUSALS.fetch_sub(1, Ordering::SeqCst);
        });
    if spawned.is_err() {
        PENDING_REFUSALS.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn wait_until_idle(grace: Duration) {
    let mut empty_since: Option<Instant> = Some(Instant::now());
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if gate().active() == 0 {
            let since = *empty_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= grace {
                return;
            }
        } else {
            empty_since = None;
        }
    }
}

struct Conn {
    buses: HashMap<u32, Box<dyn SmBusSyncOps + Send>>,
    next_bus: u32,
    register_io: HashMap<u32, RegisterIoHandle>,
    next_pawnio: u32,
    scope: CapabilityScope,
    window_started: Instant,
    window_operations: u32,
}

enum RegisterIoHandle {
    AmdSmn(PawnioModule),
    LpcIo(PawnioModule),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegisterIoKind {
    AmdSmn,
    LpcIo,
}

impl RegisterIoHandle {
    fn kind(&self) -> RegisterIoKind {
        match self {
            Self::AmdSmn(_) => RegisterIoKind::AmdSmn,
            Self::LpcIo(_) => RegisterIoKind::LpcIo,
        }
    }
}

fn handle_kind_allows(actual: RegisterIoKind, expected: RegisterIoKind) -> bool {
    actual == expected
}

impl Conn {
    fn new(scope: CapabilityScope) -> Self {
        Self {
            buses: HashMap::new(),
            next_bus: 0,
            register_io: HashMap::new(),
            next_pawnio: 0,
            scope,
            window_started: Instant::now(),
            window_operations: 0,
        }
    }

    fn authorize(&mut self, req: &Request) -> Result<CapabilityScope> {
        request_allowed(&self.scope, req)?;
        if self.window_started.elapsed() >= Duration::from_secs(1) {
            self.window_started = Instant::now();
            self.window_operations = 0;
        }
        if self.window_operations >= operations_per_second(&self.scope) {
            bail!("broker request-rate limit exceeded");
        }
        self.window_operations += 1;
        Ok(self.scope.clone())
    }
}

fn next_handle_id<V>(map: &HashMap<u32, V>, next: &mut u32, kind: &str) -> Result<u32> {
    if map.len() >= MAX_HANDLES_PER_KIND {
        bail!("too many open {kind} handles on this connection (max {MAX_HANDLES_PER_KIND})");
    }
    let id = next
        .checked_add(1)
        .ok_or_else(|| anyhow!("{kind} handle id space exhausted"))?;
    *next = id;
    Ok(id)
}

/// A read failure that just means the client closed its end of the pipe.
fn is_expected_disconnect(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::UnexpectedEof
    )
}

fn hello_scope(req: Request) -> Result<CapabilityScope> {
    match req {
        Request::Hello { scope } => validate_scope(scope),
        _ => bail!("first frame must be Hello"),
    }
}

/// Read the mandatory `Hello`, reply `Ready` or an error, and return the
/// connection bound to its scope.
fn handshake(stream: &mut PipeStream) -> Option<Conn> {
    match stream.wait_readable(HELLO_TIMEOUT) {
        Ok(true) => {}
        Ok(false) => {
            log::warn!("client sent no Hello within the timeout; disconnecting");
            return None;
        }
        Err(e) => {
            log::debug!("Hello wait failed: {e}");
            return None;
        }
    }
    let scope = match proto::read_frame(stream).map_err(anyhow::Error::from) {
        Ok(req) => hello_scope(req),
        Err(e) => Err(e),
    };
    let (reply, conn) = match scope {
        Ok(scope) => (Response::Ready, Some(Conn::new(scope))),
        Err(e) => {
            log::warn!("refusing handshake: {e:#}");
            (Response::Error(broker_error(e)), None)
        }
    };
    if let Err(e) = proto::write_frame(stream, &reply) {
        log::debug!("handshake reply failed: {e}");
        return None;
    }
    conn
}

fn serve(mut stream: PipeStream) {
    let Some(mut conn) = handshake(&mut stream) else {
        return;
    };
    enum Activity {
        Request,
        Closed,
    }
    let (activity_tx, activity_rx) = mpsc::channel();
    let pipe_handle = stream.handle_value();
    let idle_watchdog = std::thread::Builder::new()
        .name("halod-broker-idle-watchdog".into())
        .spawn(move || loop {
            match activity_rx.recv_timeout(CLIENT_IDLE_TIMEOUT) {
                Ok(Activity::Request) => {}
                Ok(Activity::Closed) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    log::info!("disconnecting idle client");
                    disconnect_handle_value(pipe_handle);
                    return;
                }
            }
        });
    loop {
        let req = match proto::read_frame(&mut stream) {
            Ok(request) => request,
            Err(e) => {
                if is_expected_disconnect(&e) {
                    log::trace!("connection closed: {e}");
                } else {
                    log::debug!("connection closed unexpectedly: {e}");
                }
                break;
            }
        };
        let _ = activity_tx.send(Activity::Request);
        let resp = match conn.authorize(&req) {
            Ok(scope) => dispatch(&mut conn, &scope, req),
            Err(e) => Response::Error(broker_error(e)),
        };
        if let Err(e) = proto::write_frame(&mut stream, &resp) {
            log::debug!("reply write failed: {e}");
            break;
        }
    }
    let _ = activity_tx.send(Activity::Closed);
    if let Ok(watchdog) = idle_watchdog {
        let _ = watchdog.join();
    }
}

fn ok_or_err(r: Result<Response>) -> Response {
    r.unwrap_or_else(|e| Response::Error(broker_error(e)))
}

fn broker_error(e: anyhow::Error) -> BrokerError {
    e.downcast()
        .unwrap_or_else(|e| BrokerError::Failed(format!("{e:#}")))
}

fn dispatch(conn: &mut Conn, scope: &CapabilityScope, req: Request) -> Response {
    match req {
        Request::OpenBus { info } => ok_or_err((|| {
            let CapabilityScope::Smbus { bus: scoped, .. } = scope else {
                bail!("SMBus open under non-SMBus capability");
            };
            if &info != scoped {
                bail!("bus does not match capability");
            }
            let id = next_handle_id(&conn.buses, &mut conn.next_bus, "bus")?;
            let bus = smbus::open_bus(&info)?;
            conn.buses.insert(id, bus);
            Ok(Response::Opened(id))
        })()),
        Request::ReadByte { bus, addr } => {
            with_bus(conn, bus, |b| Ok(Response::Byte(b.read_byte(addr)?)))
        }
        Request::ReadByteData { bus, addr, cmd } => with_bus(conn, bus, |b| {
            Ok(Response::Byte(b.read_byte_data(addr, cmd)?))
        }),
        Request::WriteQuick { bus, addr } => {
            with_bus(conn, bus, |b| Ok(Response::Bool(b.write_quick(addr)?)))
        }
        Request::WriteByteData {
            bus,
            addr,
            cmd,
            val,
        } => with_bus(conn, bus, |b| {
            b.write_byte_data(addr, cmd, val)?;
            Ok(Response::Unit)
        }),
        Request::WriteWordData {
            bus,
            addr,
            cmd,
            val,
        } => with_bus(conn, bus, |b| {
            b.write_word_data(addr, cmd, val)?;
            Ok(Response::Unit)
        }),
        Request::WriteBlockData {
            bus,
            addr,
            cmd,
            data,
        } => with_bus(conn, bus, |b| {
            b.write_block_data(addr, cmd, &data)?;
            Ok(Response::Unit)
        }),
        Request::SupportsBlockWrite { bus } => {
            with_bus(conn, bus, |b| Ok(Response::Bool(b.supports_block_write())))
        }
        Request::OpenAmdSmn => ok_or_err((|| {
            let id = next_handle_id(&conn.register_io, &mut conn.next_pawnio, "register-I/O")?;
            let module = PawnioModule::open(&["AMDFamily17.bin"])?;
            conn.register_io
                .insert(id, RegisterIoHandle::AmdSmn(module));
            Ok(Response::Opened(id))
        })()),
        Request::ReadSmn { handle, offset } => with_amd_smn(conn, handle, |module| {
            let value = exec_one(module, c"ioctl_read_smn", &[offset as u64])?;
            Ok(Response::Dword((value & 0xFFFF_FFFF) as u32))
        }),
        Request::OpenLpcIo => ok_or_err((|| {
            let id = next_handle_id(&conn.register_io, &mut conn.next_pawnio, "register-I/O")?;
            let module = PawnioModule::open(&["LpcIO.bin"])?;
            conn.register_io.insert(id, RegisterIoHandle::LpcIo(module));
            Ok(Response::Opened(id))
        })()),
        Request::LpcSelectSlot { handle, slot } => with_lpc_io(conn, handle, |module| {
            if slot > 1 {
                bail!("LPC slot {slot} is outside 0..=1");
            }
            exec_unit(module, c"ioctl_select_slot", &[slot as u64])?;
            Ok(Response::Unit)
        }),
        Request::LpcFindBars { handle } => with_lpc_io(conn, handle, |module| {
            exec_unit(module, c"ioctl_find_bars", &[])?;
            Ok(Response::Unit)
        }),
        Request::LpcReadPort { handle, port } => with_lpc_io(conn, handle, |module| {
            let value = exec_one(module, c"ioctl_pio_inb", &[port as u64])?;
            Ok(Response::Byte((value & 0xFF) as u8))
        }),
        Request::LpcWritePort {
            handle,
            port,
            value,
        } => with_lpc_io(conn, handle, |module| {
            exec_unit(module, c"ioctl_pio_outb", &[port as u64, value as u64])?;
            Ok(Response::Unit)
        }),
        Request::LpcSuperioInb { handle, register } => with_lpc_io(conn, handle, |module| {
            let value = exec_one(module, c"ioctl_superio_inb", &[register as u64])?;
            Ok(Response::Byte((value & 0xFF) as u8))
        }),
        Request::LpcSuperioOutb {
            handle,
            register,
            value,
        } => with_lpc_io(conn, handle, |module| {
            exec_unit(
                module,
                c"ioctl_superio_outb",
                &[register as u64, value as u64],
            )?;
            Ok(Response::Unit)
        }),
        Request::Hello { .. } => Response::Error(BrokerError::Failed(
            "Hello is only valid as the first frame".into(),
        )),
    }
}

fn exec_unit(module: &PawnioModule, function: &std::ffi::CStr, args: &[u64]) -> Result<()> {
    module.exec(function, args, &mut [])?;
    Ok(())
}

fn exec_one(module: &PawnioModule, function: &std::ffi::CStr, args: &[u64]) -> Result<u64> {
    let mut output = [0u64; 1];
    let count = module.exec(function, args, &mut output)?;
    require_one_word(function, &output[..count])
}

fn require_one_word(function: &std::ffi::CStr, output: &[u64]) -> Result<u64> {
    let [value] = output else {
        bail!(
            "pawnio_execute({}) returned {} words, expected 1",
            output.len(),
            function.to_string_lossy()
        );
    };
    Ok(*value)
}

fn with_register_io(
    conn: &Conn,
    handle: u32,
    want: RegisterIoKind,
    f: impl FnOnce(&PawnioModule) -> Result<Response>,
) -> Response {
    match conn.register_io.get(&handle) {
        Some(entry) if !handle_kind_allows(entry.kind(), want) => {
            Response::Error(BrokerError::Failed(format!(
                "register-I/O handle {handle} is {:?}, not {:?}",
                entry.kind(),
                want
            )))
        }
        Some(RegisterIoHandle::AmdSmn(module) | RegisterIoHandle::LpcIo(module)) => {
            ok_or_err(f(module))
        }
        None => Response::Error(BrokerError::Failed(format!(
            "unknown register-I/O handle {handle}"
        ))),
    }
}

fn with_amd_smn(
    conn: &Conn,
    handle: u32,
    f: impl FnOnce(&PawnioModule) -> Result<Response>,
) -> Response {
    with_register_io(conn, handle, RegisterIoKind::AmdSmn, f)
}

fn with_lpc_io(
    conn: &Conn,
    handle: u32,
    f: impl FnOnce(&PawnioModule) -> Result<Response>,
) -> Response {
    with_register_io(conn, handle, RegisterIoKind::LpcIo, f)
}

fn with_bus(
    conn: &mut Conn,
    bus: u32,
    f: impl FnOnce(&mut (dyn SmBusSyncOps + Send)) -> Result<Response>,
) -> Response {
    match conn.buses.get_mut(&bus) {
        Some(b) => ok_or_err(f(b.as_mut())),
        None => Response::Error(BrokerError::Failed(format!("unknown bus handle {bus}"))),
    }
}

fn validate_scope(mut scope: CapabilityScope) -> Result<CapabilityScope> {
    match &mut scope {
        CapabilityScope::Smbus {
            bus,
            addresses,
            max_operations_per_second,
        } => {
            addresses.sort_unstable();
            addresses.dedup();
            if addresses.is_empty() || addresses.len() > MAX_SCOPE_ADDRESSES {
                bail!("SMBus capability must contain 1..={MAX_SCOPE_ADDRESSES} addresses");
            }
            let exists = smbus::enumerate_buses()
                .into_iter()
                .chain(smbus::enumerate_gpu_buses())
                .any(|candidate| candidate == *bus);
            if !exists {
                bail!("requested bus was not enumerated by the broker");
            }
            clamp_rate(max_operations_per_second);
        }
        // AMD SMN offsets and LPC ports/registers are intentionally NOT bounded
        // to a numeric range the way SMBus addresses are: a granted raw-register
        // capability is inherently privileged (whole-capability trust), and any
        // per-chipset range would be arbitrary. Only the rate/op limits are
        // clamped. Deliberate — do not "fix" without adding chipset-aware ranges.
        CapabilityScope::AmdSmn {
            max_operations_per_second,
        }
        | CapabilityScope::LpcIo {
            max_operations_per_second,
        } => {
            clamp_rate(max_operations_per_second);
        }
    }
    Ok(scope)
}

fn clamp_rate(per_second: &mut u32) {
    *per_second = (*per_second).clamp(1, MAX_OPERATIONS_PER_SECOND);
}

fn operations_per_second(scope: &CapabilityScope) -> u32 {
    match scope {
        CapabilityScope::Smbus {
            max_operations_per_second,
            ..
        }
        | CapabilityScope::AmdSmn {
            max_operations_per_second,
        }
        | CapabilityScope::LpcIo {
            max_operations_per_second,
        } => *max_operations_per_second,
    }
}

fn request_allowed(scope: &CapabilityScope, req: &Request) -> Result<()> {
    match (scope, req) {
        (CapabilityScope::Smbus { bus, .. }, Request::OpenBus { info }) if info == bus => Ok(()),
        (CapabilityScope::Smbus { addresses, .. }, req) => {
            let address = match req {
                Request::ReadByte { addr, .. }
                | Request::ReadByteData { addr, .. }
                | Request::WriteQuick { addr, .. }
                | Request::WriteByteData { addr, .. }
                | Request::WriteWordData { addr, .. }
                | Request::WriteBlockData { addr, .. } => Some(*addr),
                Request::SupportsBlockWrite { .. } => None,
                _ => bail!("operation is outside the SMBus capability"),
            };
            if let Some(addr) = address {
                if !addresses.contains(&addr) {
                    bail!("SMBus address 0x{addr:02x} is outside the capability");
                }
                if let Request::WriteBlockData { data, .. } = req {
                    if data.is_empty() || data.len() > SMBUS_BLOCK_MAX {
                        bail!("SMBus block length is outside 1..={SMBUS_BLOCK_MAX}");
                    }
                }
            }
            Ok(())
        }
        (CapabilityScope::AmdSmn { .. }, Request::OpenAmdSmn | Request::ReadSmn { .. }) => Ok(()),
        (
            CapabilityScope::LpcIo { .. },
            Request::OpenLpcIo
            | Request::LpcFindBars { .. }
            | Request::LpcReadPort { .. }
            | Request::LpcWritePort { .. }
            | Request::LpcSuperioInb { .. }
            | Request::LpcSuperioOutb { .. },
        ) => Ok(()),
        (CapabilityScope::LpcIo { .. }, Request::LpcSelectSlot { slot: 0..=1, .. }) => Ok(()),
        (CapabilityScope::LpcIo { .. }, Request::LpcSelectSlot { slot, .. }) => {
            bail!("LPC slot {slot} is outside 0..=1")
        }
        _ => bail!("operation is outside the capability"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_hwaccess::smbus::BusInfo;

    fn smbus_scope() -> CapabilityScope {
        CapabilityScope::Smbus {
            bus: BusInfo {
                bus_number: 1,
                adapter_name: "test".into(),
                pci_vendor: 1,
                pci_device: 2,
                pci_sub_vendor: 3,
                pci_sub_device: 4,
            },
            addresses: vec![0x50, 0x51],
            max_operations_per_second: 10,
        }
    }

    #[test]
    fn expected_disconnects_are_classified() {
        use std::io::{Error, ErrorKind};
        for kind in [
            ErrorKind::BrokenPipe,
            ErrorKind::ConnectionReset,
            ErrorKind::UnexpectedEof,
        ] {
            assert!(is_expected_disconnect(&Error::new(kind, "x")), "{kind:?}");
        }
        assert!(!is_expected_disconnect(&Error::new(
            ErrorKind::InvalidData,
            "frame too long"
        )));
        assert!(!is_expected_disconnect(&Error::other("bad json")));
    }

    #[test]
    fn handle_ids_are_bounded_and_checked() {
        let map: HashMap<u32, ()> = HashMap::new();
        let mut next = 0;
        assert_eq!(next_handle_id(&map, &mut next, "bus").unwrap(), 1);
        next = u32::MAX;
        assert!(next_handle_id(&map, &mut next, "bus").is_err());
        let full: HashMap<u32, ()> = (0..MAX_HANDLES_PER_KIND as u32).map(|i| (i, ())).collect();
        assert!(next_handle_id(&full, &mut 0, "bus").is_err());
    }

    #[test]
    fn smbus_scope_rejects_other_addresses_and_oversize_blocks() {
        let scope = smbus_scope();
        assert!(request_allowed(&scope, &Request::ReadByte { bus: 1, addr: 0x50 }).is_ok());
        assert!(request_allowed(&scope, &Request::ReadByte { bus: 1, addr: 0x52 }).is_err());
        assert!(request_allowed(
            &scope,
            &Request::WriteBlockData {
                bus: 1,
                addr: 0x50,
                cmd: 0,
                data: vec![0; SMBUS_BLOCK_MAX + 1],
            }
        )
        .is_err());
    }

    #[test]
    fn typed_scopes_reject_cross_use_and_invalid_lpc_slots() {
        let amd = CapabilityScope::AmdSmn {
            max_operations_per_second: 10,
        };
        let lpc = CapabilityScope::LpcIo {
            max_operations_per_second: 10,
        };
        assert!(request_allowed(&amd, &Request::OpenAmdSmn).is_ok());
        assert!(request_allowed(
            &amd,
            &Request::ReadSmn {
                handle: 1,
                offset: 0x1234,
            }
        )
        .is_ok());
        assert!(request_allowed(&amd, &Request::OpenLpcIo).is_err());
        assert!(request_allowed(&lpc, &Request::OpenAmdSmn).is_err());
        assert!(request_allowed(&lpc, &Request::LpcSelectSlot { handle: 1, slot: 1 }).is_ok());
        assert!(request_allowed(&lpc, &Request::LpcSelectSlot { handle: 1, slot: 2 }).is_err());
    }

    #[test]
    fn typed_handle_kinds_reject_cross_use() {
        assert!(handle_kind_allows(
            RegisterIoKind::AmdSmn,
            RegisterIoKind::AmdSmn
        ));
        assert!(handle_kind_allows(
            RegisterIoKind::LpcIo,
            RegisterIoKind::LpcIo
        ));
        assert!(!handle_kind_allows(
            RegisterIoKind::AmdSmn,
            RegisterIoKind::LpcIo
        ));
        assert!(!handle_kind_allows(
            RegisterIoKind::LpcIo,
            RegisterIoKind::AmdSmn
        ));
    }

    #[test]
    fn malformed_empty_pawnio_read_fails_closed() {
        assert!(require_one_word(c"ioctl_read_smn", &[]).is_err());
        assert_eq!(require_one_word(c"ioctl_pio_inb", &[0x42]).unwrap(), 0x42);
    }

    #[test]
    fn connection_enforces_the_per_second_rate() {
        let mut conn = Conn::new(smbus_scope());
        let request = Request::ReadByte { bus: 1, addr: 0x50 };
        for _ in 0..10 {
            assert!(conn.authorize(&request).is_ok());
        }
        assert!(conn.authorize(&request).is_err());
        conn.window_started = Instant::now() - Duration::from_secs(1);
        assert!(conn.authorize(&request).is_ok());
    }

    #[test]
    fn out_of_scope_requests_do_not_consume_the_rate() {
        let mut conn = Conn::new(smbus_scope());
        assert!(conn
            .authorize(&Request::ReadByte { bus: 1, addr: 0x52 })
            .is_err());
        assert_eq!(conn.window_operations, 0);
    }

    #[test]
    fn hello_is_only_accepted_as_first_frame() {
        let lpc = CapabilityScope::LpcIo {
            max_operations_per_second: 10,
        };
        assert_eq!(
            hello_scope(Request::Hello { scope: lpc.clone() }).unwrap(),
            lpc
        );
        assert!(hello_scope(Request::OpenLpcIo).is_err());
        let mut conn = Conn::new(lpc.clone());
        assert!(conn.authorize(&Request::Hello { scope: lpc }).is_err());
        assert!(Conn::new(smbus_scope())
            .authorize(&Request::Hello {
                scope: smbus_scope()
            })
            .is_err());
    }

    #[test]
    fn scope_without_total_limit_round_trips() {
        let scope = validate_scope(CapabilityScope::AmdSmn {
            max_operations_per_second: u32::MAX,
        })
        .unwrap();
        assert_eq!(
            scope,
            CapabilityScope::AmdSmn {
                max_operations_per_second: MAX_OPERATIONS_PER_SECOND
            }
        );
        assert_eq!(
            validate_scope(CapabilityScope::LpcIo {
                max_operations_per_second: 0
            })
            .unwrap(),
            CapabilityScope::LpcIo {
                max_operations_per_second: 1
            }
        );
        let mut buf = Vec::new();
        proto::write_frame(
            &mut buf,
            &Request::Hello {
                scope: scope.clone(),
            },
        )
        .unwrap();
        assert!(!String::from_utf8_lossy(&buf).contains("max_operations\""));
        let decoded: Request = proto::read_frame(&mut std::io::Cursor::new(buf)).unwrap();
        assert_eq!(decoded, Request::Hello { scope });
    }

    #[test]
    fn a_refused_client_reads_the_reason_before_the_pipe_closes() {
        use crate::pipe::{create_instance, wait_for_client, PipeSecurity};

        let name = format!(r"\\.\pipe\halod-refuse-test-{}", std::process::id());
        let sec = PipeSecurity::from_sddl("D:P(A;;GA;;;WD)").unwrap();
        let handle = create_instance(&name, &sec, true).unwrap();
        let client_name = name.clone();
        let client = std::thread::spawn(move || {
            let mut pipe = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(client_name)
                .unwrap();
            proto::write_frame(
                &mut pipe,
                &Request::Hello {
                    scope: smbus_scope(),
                },
            )
            .unwrap();
            // Give the server time to close; the reply must survive it.
            std::thread::sleep(Duration::from_millis(100));
            proto::read_frame::<_, Response>(&mut pipe).unwrap()
        });
        wait_for_client(handle).unwrap();
        refuse(PipeStream::new(handle), "test reason".into());
        assert_eq!(
            client.join().unwrap(),
            Response::Error(BrokerError::Failed("broker refused: test reason".into()))
        );
    }

    #[test]
    fn foreground_config_takes_only_the_coordinator_sid() {
        let args = vec!["--coordinator-sid=S-1-5-21-1".to_owned()];
        assert_eq!(
            foreground_config_from_args(&args).unwrap(),
            BrokerConfig {
                coordinators: vec!["S-1-5-21-1".into()],
                pin_client_image: false,
            }
        );
        assert!(foreground_config_from_args(&[]).is_err());
        assert!(foreground_config_from_args(&["--coordinator-sid=".into()]).is_err());
    }
}
