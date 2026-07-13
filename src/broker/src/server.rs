// SPDX-License-Identifier: GPL-3.0-or-later
//! Authenticated, capability-scoped broker RPC server.

use std::collections::HashMap;
use std::sync::{mpsc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use halod_hwaccess::pawnio::{PawnioModule, PawnioOps};
use halod_hwaccess::proto::{
    self, CapabilityScope, Request, Response, CAPABILITY_TTL_MS, MAX_OPERATIONS_PER_CAPABILITY,
    MAX_OPERATIONS_PER_SECOND, MAX_PAWNIO_ARGS, MAX_PAWNIO_FUNCTIONS, MAX_SCOPE_ADDRESSES,
    PIPE_NAME,
};
use halod_hwaccess::smbus::{self, SmBusSyncOps, SMBUS_BLOCK_MAX};
use halod_hwaccess::winsec;
use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};

use crate::clientauth::{self, Admission, ClientIdentity, Gate};
use crate::pipe::{create_instance, wait_for_client, PipeSecurity, PipeStream};

const MAX_CLIENTS: usize = 32;
const MAX_HANDLES_PER_KIND: usize = 64;
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
        let stream = PipeStream::new(handle);
        if let Err(e) = wait_for_client(handle) {
            log::warn!("[broker] wait_for_client failed: {e}");
            drop(stream);
            continue;
        }

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
                        serve(stream);
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
    pawnio: HashMap<u32, PawnioModule>,
    next_pawnio: u32,
    capability: Option<Capability>,
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
        self.pawnio.clear();
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

fn serve(mut stream: PipeStream) {
    let mut conn = Conn::default();
    loop {
        match stream.wait_readable(CLIENT_IDLE_TIMEOUT) {
            Ok(true) => {}
            Ok(false) => {
                log::info!("[broker] closing idle client");
                break;
            }
            Err(e) => {
                log::debug!("[broker] connection wait failed: {e}");
                break;
            }
        }
        let req: Request = match proto::read_frame(&mut stream) {
            Ok(r) => r,
            Err(e) => {
                log::debug!("[broker] connection closed: {e}");
                break;
            }
        };
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
        Request::PawnioOpen { module } => ok_or_err((|| {
            let id = next_handle_id(&conn.pawnio, &mut conn.next_pawnio, "pawnio")?;
            let m = PawnioModule::open(&[module.as_str()])?;
            conn.pawnio.insert(id, m);
            Ok(Response::Opened(id))
        })()),
        Request::PawnioExec {
            handle,
            function,
            args,
        } => ok_or_err((|| {
            let m = conn
                .pawnio
                .get(&handle)
                .ok_or_else(|| anyhow!("unknown pawnio handle {handle}"))?;
            Ok(Response::Words(m.execute(&function, &args)?))
        })()),
        Request::Enumerate
        | Request::EnumerateGpu
        | Request::Authenticate { .. }
        | Request::Renew { .. } => Response::Error("operation not available in this state".into()),
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
        CapabilityScope::Pawnio {
            module,
            functions,
            max_operations_per_second,
            max_operations,
        } => {
            functions.sort();
            functions.dedup();
            if functions.is_empty() || functions.len() > MAX_PAWNIO_FUNCTIONS {
                bail!("PawnIO capability has an invalid function count");
            }
            if !functions.iter().all(|f| pawnio_function_allowed(module, f)) {
                bail!("PawnIO module/function is not broker-approved");
            }
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
        | CapabilityScope::Pawnio {
            max_operations_per_second,
            max_operations,
            ..
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
        (
            CapabilityScope::Pawnio {
                module, functions, ..
            },
            Request::PawnioOpen { module: requested },
        ) if requested == module => Ok(()),
        (
            CapabilityScope::Pawnio {
                module, functions, ..
            },
            Request::PawnioExec { function, args, .. },
        ) if functions.contains(function)
            && args.len() <= MAX_PAWNIO_ARGS
            && pawnio_signature_allowed(module, function, args.len()) =>
        {
            Ok(())
        }
        _ => bail!("operation is outside the capability"),
    }
}

fn pawnio_function_allowed(module: &str, function: &str) -> bool {
    matches!(
        (module, function),
        ("AMDFamily17.bin", "ioctl_read_smn")
            | ("LpcIO.bin", "ioctl_select_slot")
            | ("LpcIO.bin", "ioctl_find_bars")
            | ("LpcIO.bin", "ioctl_pio_inb")
            | ("LpcIO.bin", "ioctl_pio_outb")
            | ("LpcIO.bin", "ioctl_superio_inb")
            | ("LpcIO.bin", "ioctl_superio_outb")
    )
}

fn pawnio_signature_allowed(module: &str, function: &str, args: usize) -> bool {
    match (module, function) {
        ("AMDFamily17.bin", "ioctl_read_smn") => args == 1,
        ("LpcIO.bin", "ioctl_select_slot") => args == 1,
        ("LpcIO.bin", "ioctl_find_bars") => args == 0,
        ("LpcIO.bin", "ioctl_pio_inb") => args == 1,
        ("LpcIO.bin", "ioctl_pio_outb") => args == 2,
        ("LpcIO.bin", "ioctl_superio_inb") => args == 1,
        ("LpcIO.bin", "ioctl_superio_outb") => args == 2,
        _ => false,
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
    fn pawnio_scope_pins_module_function_and_arity() {
        let scope = CapabilityScope::Pawnio {
            module: "AMDFamily17.bin".into(),
            functions: vec!["ioctl_read_smn".into()],
            max_operations_per_second: 10,
            max_operations: 10,
        };
        assert!(request_allowed(
            &scope,
            &Request::PawnioExec {
                handle: 1,
                function: "ioctl_read_smn".into(),
                args: vec![0x1234],
            }
        )
        .is_ok());
        assert!(request_allowed(
            &scope,
            &Request::PawnioExec {
                handle: 1,
                function: "ioctl_read_smn".into(),
                args: vec![],
            }
        )
        .is_err());
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
