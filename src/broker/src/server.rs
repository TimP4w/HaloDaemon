// SPDX-License-Identifier: GPL-3.0-or-later
//! Authenticated, capability-scoped broker RPC server.

use std::collections::HashMap;
use std::sync::{mpsc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use halod_hwaccess::pawnio::PawnioModule;
use halod_hwaccess::proto::{
    self, CapabilityScope, Request, Response, CAPABILITY_TTL_MS, MAX_OPERATIONS_PER_CAPABILITY,
    MAX_OPERATIONS_PER_SECOND, MAX_SCOPE_ADDRESSES, PIPE_NAME,
};
use halod_hwaccess::smbus::{self, SmBusSyncOps, SMBUS_BLOCK_MAX};
use halod_hwaccess::winsec;
use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};

use crate::clientauth::{self, Admission, ClientIdentity, Gate};
use crate::pipe::{
    create_instance, disconnect_handle_value, wait_for_client, PipeSecurity, PipeStream,
};

const MAX_CLIENTS: usize = 32;
const MAX_HANDLES_PER_KIND: usize = 64;
const AUTHENTICATION_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
pub struct AuthConfig {
    pub coordinator: ClientIdentity,
    pub bootstrap_token: String,
}

pub fn auth_config_from_args(args: &[String]) -> Result<AuthConfig> {
    fn value(args: &[String], name: &str) -> Result<String> {
        let prefix = format!("--{name}=");
        args.iter()
            .find_map(|arg| arg.strip_prefix(&prefix))
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("missing required broker argument --{name}"))
    }

    let session = value(args, "coordinator-session")?
        .parse::<u32>()
        .context("invalid --coordinator-session")?;
    Ok(AuthConfig {
        coordinator: ClientIdentity {
            sid: value(args, "coordinator-sid")?,
            session,
        },
        bootstrap_token: value(args, "bootstrap-token")?,
    })
}

static CONFIG: OnceLock<AuthConfig> = OnceLock::new();
static GATE: Mutex<Gate> = Mutex::new(Gate::new());

pub fn configure(config: AuthConfig) -> Result<()> {
    winsec::validate_sid_string(&config.coordinator.sid)?;
    if config.bootstrap_token.len() != 64
        || !config
            .bootstrap_token
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        bail!("broker bootstrap token must be 32 random bytes encoded as hex");
    }
    CONFIG
        .set(config)
        .map_err(|_| anyhow!("broker authentication already configured"))
}

fn config() -> Result<&'static AuthConfig> {
    CONFIG
        .get()
        .ok_or_else(|| anyhow!("broker authentication was not configured"))
}

fn gate() -> MutexGuard<'static, Gate> {
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn serve_forever() -> Result<()> {
    let config = config()?.clone();
    let sec = PipeSecurity::from_sddl(&winsec::coordinator_dacl_sddl(&config.coordinator.sid)?)?;
    log::info!(
        "[broker] pipe secured to coordinator SID {} session {}",
        config.coordinator.sid,
        config.coordinator.session
    );

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

        let handle = create_instance(PIPE_NAME, &sec)?;
        let mut stream = PipeStream::new(handle);
        if let Err(e) = wait_for_client(handle) {
            log::warn!("[broker] wait_for_client failed: {e}");
            drop(stream);
            continue;
        }

        // Windows requires the server to consume data from a named-pipe client
        // before ImpersonateNamedPipeClient can derive that client's token.
        // Read exactly the mandatory first authentication frame here, then hand
        // it to the admitted worker so it is processed only once.
        match stream.wait_readable(AUTHENTICATION_TIMEOUT) {
            Ok(true) => {}
            Ok(false) => {
                log::warn!("[broker] client did not authenticate within the timeout; refusing");
                drop(stream);
                continue;
            }
            Err(e) => {
                log::debug!("[broker] authentication wait failed: {e}");
                drop(stream);
                continue;
            }
        }
        let first_request: Request = match proto::read_frame(&mut stream) {
            Ok(request @ Request::Authenticate { .. }) => request,
            Ok(_) => {
                log::warn!("[broker] first client frame was not Authenticate; refusing");
                drop(stream);
                continue;
            }
            Err(e) => {
                log::debug!("[broker] could not read authentication frame: {e}");
                drop(stream);
                continue;
            }
        };

        let identity = match clientauth::pipe_client_identity(handle) {
            Ok(id) => id,
            Err(e) => {
                log::warn!("[broker] could not identify pipe client ({e}); refusing");
                drop(stream);
                continue;
            }
        };
        match clientauth::decide(&mut gate(), &identity, &config.coordinator, MAX_CLIENTS) {
            Admission::Ok => {
                next_worker = next_worker.checked_add(1).unwrap_or(1);
                let id = next_worker;
                let done = done_tx.clone();
                let worker = std::thread::Builder::new()
                    .name(format!("halod-broker-{id}"))
                    .spawn(move || {
                        serve(stream, first_request);
                        clientauth::release(&mut gate());
                        let _ = done.send(id);
                    })?;
                workers.insert(id, worker);
            }
            Admission::WrongUser => {
                log::warn!(
                    "[broker] refusing client SID {} session {}: expected SID {} session {}",
                    identity.sid,
                    identity.session,
                    config.coordinator.sid,
                    config.coordinator.session
                );
                drop(stream);
            }
            Admission::TooMany => {
                log::warn!("[broker] refusing client: {MAX_CLIENTS} connections already active");
                drop(stream);
            }
        }
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

struct Capability {
    id: String,
    scope: CapabilityScope,
    expires: Instant,
    remaining: u32,
    window_started: Instant,
    window_operations: u32,
}

#[derive(Default)]
struct Conn {
    buses: HashMap<u32, Box<dyn SmBusSyncOps + Send>>,
    next_bus: u32,
    register_io: HashMap<u32, RegisterIoHandle>,
    next_pawnio: u32,
    capability: Option<Capability>,
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
    fn authenticate(&mut self, bootstrap_token: &str, scope: CapabilityScope) -> Result<Response> {
        if !constant_time_eq(
            bootstrap_token.as_bytes(),
            config()?.bootstrap_token.as_bytes(),
        ) {
            bail!("invalid broker bootstrap token");
        }
        let scope = validate_scope(scope)?;
        let id = random_token()?;
        let remaining = scope_limits(&scope).1;
        self.buses.clear();
        self.register_io.clear();
        self.capability = Some(Capability {
            id: id.clone(),
            scope,
            expires: Instant::now() + Duration::from_millis(CAPABILITY_TTL_MS),
            remaining,
            window_started: Instant::now(),
            window_operations: 0,
        });
        Ok(Response::Authorized {
            capability: id,
            expires_in_ms: CAPABILITY_TTL_MS,
        })
    }

    fn renew(&mut self, capability: &str) -> Result<Response> {
        let cap = self
            .capability
            .as_mut()
            .ok_or_else(|| anyhow!("connection is not authenticated"))?;
        if !constant_time_eq(capability.as_bytes(), cap.id.as_bytes()) {
            bail!("invalid capability");
        }
        if Instant::now() >= cap.expires {
            bail!("capability expired");
        }
        cap.id = random_token()?;
        cap.expires = Instant::now() + Duration::from_millis(CAPABILITY_TTL_MS);
        cap.remaining = scope_limits(&cap.scope).1;
        cap.window_started = Instant::now();
        cap.window_operations = 0;
        Ok(Response::Authorized {
            capability: cap.id.clone(),
            expires_in_ms: CAPABILITY_TTL_MS,
        })
    }

    fn authorize(&mut self, req: &Request) -> Result<CapabilityScope> {
        let cap = self
            .capability
            .as_mut()
            .ok_or_else(|| anyhow!("authenticate before requesting broker operations"))?;
        if Instant::now() >= cap.expires {
            bail!("capability expired");
        }
        request_allowed(&cap.scope, req)?;
        if cap.remaining == 0 {
            bail!("capability operation limit exhausted");
        }
        let (per_second, _) = scope_limits(&cap.scope);
        if cap.window_started.elapsed() >= Duration::from_secs(1) {
            cap.window_started = Instant::now();
            cap.window_operations = 0;
        }
        if cap.window_operations >= per_second {
            bail!("broker request-rate limit exceeded");
        }
        cap.window_operations += 1;
        cap.remaining -= 1;
        Ok(cap.scope.clone())
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

fn serve(mut stream: PipeStream, first_request: Request) {
    let mut conn = Conn::default();
    let mut pending = Some(first_request);
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
                    log::info!("[broker] disconnecting idle client");
                    disconnect_handle_value(pipe_handle);
                    return;
                }
            }
        });
    loop {
        let req = match pending.take() {
            Some(request) => request,
            None => match proto::read_frame(&mut stream) {
                Ok(request) => request,
                Err(e) => {
                    log::debug!("[broker] connection closed: {e}");
                    break;
                }
            },
        };
        let _ = activity_tx.send(Activity::Request);
        let resp = match req {
            Request::Authenticate {
                bootstrap_token,
                scope,
            } => ok_or_err(conn.authenticate(&bootstrap_token, scope)),
            Request::Renew { capability } => ok_or_err(conn.renew(&capability)),
            operation => match conn.authorize(&operation) {
                Ok(scope) => dispatch(&mut conn, &scope, operation),
                Err(e) => Response::Error(format!("{e:#}")),
            },
        };
        if let Err(e) = proto::write_frame(&mut stream, &resp) {
            log::debug!("[broker] reply write failed: {e}");
            break;
        }
    }
    let _ = activity_tx.send(Activity::Closed);
    if let Ok(watchdog) = idle_watchdog {
        let _ = watchdog.join();
    }
}

fn ok_or_err(r: Result<Response>) -> Response {
    r.unwrap_or_else(|e| Response::Error(format!("{e:#}")))
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
        Request::Enumerate
        | Request::EnumerateGpu
        | Request::Authenticate { .. }
        | Request::Renew { .. } => Response::Error("operation not available in this state".into()),
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

fn with_amd_smn(
    conn: &Conn,
    handle: u32,
    f: impl FnOnce(&PawnioModule) -> Result<Response>,
) -> Response {
    match conn.register_io.get(&handle) {
        Some(entry) if !handle_kind_allows(entry.kind(), RegisterIoKind::AmdSmn) => {
            Response::Error(format!("register-I/O handle {handle} is LPC, not AMD SMN"))
        }
        Some(RegisterIoHandle::AmdSmn(module)) => ok_or_err(f(module)),
        Some(RegisterIoHandle::LpcIo(_)) => unreachable!("kind checked above"),
        None => Response::Error(format!("unknown register-I/O handle {handle}")),
    }
}

fn with_lpc_io(
    conn: &Conn,
    handle: u32,
    f: impl FnOnce(&PawnioModule) -> Result<Response>,
) -> Response {
    match conn.register_io.get(&handle) {
        Some(entry) if !handle_kind_allows(entry.kind(), RegisterIoKind::LpcIo) => {
            Response::Error(format!("register-I/O handle {handle} is AMD SMN, not LPC"))
        }
        Some(RegisterIoHandle::LpcIo(module)) => ok_or_err(f(module)),
        Some(RegisterIoHandle::AmdSmn(_)) => unreachable!("kind checked above"),
        None => Response::Error(format!("unknown register-I/O handle {handle}")),
    }
}

fn with_bus(
    conn: &mut Conn,
    bus: u32,
    f: impl FnOnce(&mut (dyn SmBusSyncOps + Send)) -> Result<Response>,
) -> Response {
    match conn.buses.get_mut(&bus) {
        Some(b) => ok_or_err(f(b.as_mut())),
        None => Response::Error(format!("unknown bus handle {bus}")),
    }
}

fn validate_scope(mut scope: CapabilityScope) -> Result<CapabilityScope> {
    match &mut scope {
        CapabilityScope::Smbus {
            bus,
            addresses,
            max_operations_per_second,
            max_operations,
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
            clamp_limits(max_operations_per_second, max_operations);
        }
        CapabilityScope::AmdSmn {
            max_operations_per_second,
            max_operations,
        }
        | CapabilityScope::LpcIo {
            max_operations_per_second,
            max_operations,
        } => {
            clamp_limits(max_operations_per_second, max_operations);
        }
    }
    Ok(scope)
}

fn clamp_limits(per_second: &mut u32, total: &mut u32) {
    *per_second = (*per_second).clamp(1, MAX_OPERATIONS_PER_SECOND);
    *total = (*total).clamp(1, MAX_OPERATIONS_PER_CAPABILITY);
}

fn scope_limits(scope: &CapabilityScope) -> (u32, u32) {
    match scope {
        CapabilityScope::Smbus {
            max_operations_per_second,
            max_operations,
            ..
        }
        | CapabilityScope::AmdSmn {
            max_operations_per_second,
            max_operations,
        }
        | CapabilityScope::LpcIo {
            max_operations_per_second,
            max_operations,
        } => (*max_operations_per_second, *max_operations),
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

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .map_err(|e| anyhow!("BCryptGenRandom: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
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
            max_operations: 100,
        }
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
            max_operations: 10,
        };
        let lpc = CapabilityScope::LpcIo {
            max_operations_per_second: 10,
            max_operations: 10,
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
    fn token_comparison_checks_every_byte() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn capability_enforces_rate_and_total_limits() {
        let mut conn = Conn {
            capability: Some(Capability {
                id: "cap".into(),
                scope: smbus_scope(),
                expires: Instant::now() + Duration::from_secs(1),
                remaining: 2,
                window_started: Instant::now(),
                window_operations: 0,
            }),
            ..Conn::default()
        };
        let request = Request::ReadByte { bus: 1, addr: 0x50 };
        assert!(conn.authorize(&request).is_ok());
        assert!(conn.authorize(&request).is_ok());
        assert!(conn.authorize(&request).is_err());

        let cap = conn.capability.as_mut().unwrap();
        cap.remaining = 100;
        cap.window_operations = 10;
        assert!(conn.authorize(&request).is_err());
    }

    #[test]
    fn expired_capability_is_rejected() {
        let mut conn = Conn {
            capability: Some(Capability {
                id: "cap".into(),
                scope: smbus_scope(),
                expires: Instant::now() - Duration::from_millis(1),
                remaining: 1,
                window_started: Instant::now(),
                window_operations: 0,
            }),
            ..Conn::default()
        };
        assert!(conn
            .authorize(&Request::ReadByte { bus: 1, addr: 0x50 })
            .is_err());
        assert!(conn.renew("cap").is_err());
    }

    #[test]
    fn renewal_rotates_the_connection_capability() {
        let mut conn = Conn {
            capability: Some(Capability {
                id: "cap".into(),
                scope: smbus_scope(),
                expires: Instant::now() + Duration::from_secs(1),
                remaining: 0,
                window_started: Instant::now(),
                window_operations: 10,
            }),
            ..Conn::default()
        };
        let Response::Authorized { capability, .. } = conn.renew("cap").unwrap() else {
            panic!("renewal did not return a capability");
        };
        assert_ne!(capability, "cap");
        assert_eq!(conn.capability.as_ref().unwrap().window_operations, 0);
    }
}
