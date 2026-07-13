// SPDX-License-Identifier: GPL-3.0-or-later
//! Named-pipe RPC client to the elevated `halod-broker`.
//!
//! One connection per opened bus / PawnIO module: a `SmBusDevice` already
//! serialises its ops behind a mutex, so a single blocking request/response
//! stream per handle needs no extra locking. [`connect_or_spawn`] brings the
//! broker up on first use: the installed on-demand `HalodBroker` service is
//! started via the SCM (no UAC); a dev run with no service installed falls back
//! to a single `runas` UAC prompt.

use std::fs::{File, OpenOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

use halod_hwaccess::pawnio::PawnioOps;
use halod_hwaccess::proto::{
    self, CapabilityScope, Request, Response, BROKER_SERVICE_NAME, MAX_OPERATIONS_PER_CAPABILITY,
    MAX_OPERATIONS_PER_SECOND, PIPE_NAME,
};
use halod_hwaccess::smbus::{BusInfo, SmBusSyncOps};

/// How many connect attempts before giving up, and how long to wait between
/// them — enough to cover the SCM starting the on-demand broker, or a dev-run
/// UAC prompt being accepted.
const CONNECT_ATTEMPTS: u32 = 160;
const CONNECT_BACKOFF: Duration = Duration::from_millis(250);

/// Whether a dev run has already fired a UAC prompt, so repeated register-bus
/// opens don't each re-prompt after a decline.
static BROKER_SPAWN_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct BrokerAuth {
    token: String,
    sid: String,
    session: u32,
}

static BROKER_AUTH: OnceLock<std::result::Result<BrokerAuth, String>> = OnceLock::new();

fn broker_auth() -> Result<&'static BrokerAuth> {
    let result = BROKER_AUTH.get_or_init(|| {
        let (sid, session) = halod_hwaccess::winsec::current_process_identity()
            .map_err(|e| format!("query coordinator identity: {e:#}"))?;
        let mut bytes = [0u8; 32];
        // SAFETY: BCrypt writes exactly the supplied mutable byte slice and
        // uses the OS system-preferred CSPRNG (no algorithm handle required).
        unsafe {
            windows::Win32::Security::Cryptography::BCryptGenRandom(
                None,
                &mut bytes,
                windows::Win32::Security::Cryptography::BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        }
        .ok()
        .map_err(|e| format!("generate broker bootstrap secret: {e}"))?;
        let token = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(BrokerAuth {
            token,
            sid,
            session,
        })
    });
    result.as_ref().map_err(|e| anyhow!(e.clone()))
}

fn broker_arguments(auth: &BrokerAuth) -> Vec<std::ffi::OsString> {
    vec![
        format!("--bootstrap-token={}", auth.token).into(),
        format!("--coordinator-sid={}", auth.sid).into(),
        format!("--coordinator-session={}", auth.session).into(),
    ]
}

fn connect() -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open(PIPE_NAME)
}

/// Start the installed on-demand `HalodBroker` service via the SCM. Returns
/// `Ok` if it started or was already running; `Err` if the service isn't
/// installed (dev run) or we lack rights.
fn try_start_broker_service(auth: &BrokerAuth) -> Result<()> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| anyhow!("open SCM: {e}"))?;
    let service = manager
        .open_service(
            BROKER_SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::START,
        )
        .map_err(|e| anyhow!("open service: {e}"))?;
    if matches!(
        service.query_status().map(|s| s.current_state),
        Ok(ServiceState::Running) | Ok(ServiceState::StartPending)
    ) {
        return Ok(());
    }
    let arguments = broker_arguments(auth);
    let arguments: Vec<&std::ffi::OsStr> = arguments.iter().map(|arg| arg.as_os_str()).collect();
    service
        .start(&arguments)
        .map_err(|e| anyhow!("start service: {e}"))
}

/// Bring the broker up: prefer the installed on-demand service (no UAC); if it
/// isn't installed, this is a dev run — spawn it via one UAC prompt.
fn ensure_broker_running(auth: &BrokerAuth) {
    match try_start_broker_service(auth) {
        Ok(()) => log::info!("[register_ops] started the {BROKER_SERVICE_NAME} service"),
        Err(e) => {
            log::debug!(
                "[register_ops] broker service unavailable ({e}); trying dev-run UAC spawn"
            );
            if !BROKER_SPAWN_REQUESTED.swap(true, Ordering::SeqCst) {
                match crate::platform::elevation::spawn_broker_elevated(
                    &auth.token,
                    &auth.sid,
                    auth.session,
                ) {
                    Ok(()) => log::info!("[register_ops] requested elevated halod-broker (dev)"),
                    Err(se) => log::warn!(
                        "[register_ops] could not launch halod-broker ({se}); \
                         register-bus devices will be unavailable"
                    ),
                }
            }
        }
    }
}

/// Connect to the broker, bringing it up on first attempt if it isn't there yet.
fn connect_or_spawn(scope: &CapabilityScope) -> Result<AuthorizedPipe> {
    let auth = broker_auth()?;
    for attempt in 0..CONNECT_ATTEMPTS {
        match connect() {
            Ok(mut pipe) => {
                let request = Request::Authenticate {
                    bootstrap_token: auth.token.clone(),
                    scope: scope.clone(),
                };
                if let Err(e) = proto::write_frame(&mut pipe, &request) {
                    log::debug!("[register_ops] broker authentication send failed: {e}");
                } else {
                    match proto::read_frame(&mut pipe) {
                        Ok(Response::Authorized {
                            capability,
                            expires_in_ms,
                        }) => {
                            // A later broker exit may legitimately require a
                            // fresh dev-run UAC launch. Only suppress repeated
                            // prompts until one authenticated connection has
                            // actually succeeded.
                            BROKER_SPAWN_REQUESTED.store(false, Ordering::SeqCst);
                            return Ok(AuthorizedPipe {
                                pipe,
                                capability,
                                renew_at: Instant::now()
                                    + Duration::from_millis(expires_in_ms / 2),
                            });
                        }
                        Ok(other) => log::debug!(
                            "[register_ops] broker authentication attempt {attempt} failed: {other:?}"
                        ),
                        Err(e) => log::debug!(
                            "[register_ops] broker authentication receive failed: {e}"
                        ),
                    }
                }
            }
            Err(e) => {
                if attempt == 0 {
                    ensure_broker_running(auth);
                }
                log::debug!("[register_ops] broker connect attempt {attempt} failed: {e}");
            }
        }
        std::thread::sleep(CONNECT_BACKOFF);
    }
    bail!("could not reach halod-broker on {PIPE_NAME}")
}

struct AuthorizedPipe {
    pipe: File,
    capability: String,
    renew_at: Instant,
}

impl AuthorizedPipe {
    fn request(&mut self, req: &Request) -> Result<Response> {
        if Instant::now() >= self.renew_at {
            proto::write_frame(
                &mut self.pipe,
                &Request::Renew {
                    capability: self.capability.clone(),
                },
            )
            .map_err(|e| anyhow!("broker capability renewal send: {e}"))?;
            match proto::read_frame(&mut self.pipe)
                .map_err(|e| anyhow!("broker capability renewal receive: {e}"))?
            {
                Response::Authorized {
                    capability,
                    expires_in_ms,
                } => {
                    self.capability = capability;
                    self.renew_at = Instant::now() + Duration::from_millis(expires_in_ms / 2);
                }
                Response::Error(e) => bail!("broker capability renewal failed: {e}"),
                other => bail!("unexpected broker capability renewal reply: {other:?}"),
            }
        }
        proto::write_frame(&mut self.pipe, req).map_err(|e| anyhow!("broker send: {e}"))?;
        proto::read_frame(&mut self.pipe).map_err(|e| anyhow!("broker recv: {e}"))
    }
}

fn expect_opened(resp: Response, what: &str) -> Result<u32> {
    match resp {
        Response::Opened(id) => Ok(id),
        Response::Error(e) => Err(anyhow!("broker refused to open {what}: {e}")),
        other => Err(anyhow!("unexpected broker reply opening {what}: {other:?}")),
    }
}

// ── SMBus over RPC ─────────────────────────────────────────────────────────

struct BrokerBus {
    pipe: AuthorizedPipe,
    bus_id: u32,
    /// Cached at open time so `supports_block_write(&self)` needs no round trip.
    supports_block: bool,
}

impl BrokerBus {
    fn request(&mut self, req: &Request) -> Result<Response> {
        self.pipe.request(req)
    }

    fn byte(&mut self, req: Request) -> Result<u8> {
        match self.request(&req)? {
            Response::Byte(b) => Ok(b),
            Response::Error(e) => bail!("broker: {e}"),
            other => bail!("unexpected broker reply: {other:?}"),
        }
    }

    fn unit(&mut self, req: Request) -> Result<()> {
        match self.request(&req)? {
            Response::Unit => Ok(()),
            Response::Error(e) => bail!("broker: {e}"),
            other => bail!("unexpected broker reply: {other:?}"),
        }
    }
}

impl SmBusSyncOps for BrokerBus {
    fn read_byte(&mut self, addr: u8) -> Result<u8> {
        self.byte(Request::ReadByte {
            bus: self.bus_id,
            addr,
        })
    }
    fn read_byte_data(&mut self, addr: u8, cmd: u8) -> Result<u8> {
        self.byte(Request::ReadByteData {
            bus: self.bus_id,
            addr,
            cmd,
        })
    }
    fn write_quick(&mut self, addr: u8) -> Result<bool> {
        match self.request(&Request::WriteQuick {
            bus: self.bus_id,
            addr,
        })? {
            Response::Bool(b) => Ok(b),
            Response::Error(e) => bail!("broker: {e}"),
            other => bail!("unexpected broker reply: {other:?}"),
        }
    }
    fn write_byte_data(&mut self, addr: u8, cmd: u8, val: u8) -> Result<()> {
        self.unit(Request::WriteByteData {
            bus: self.bus_id,
            addr,
            cmd,
            val,
        })
    }
    fn write_word_data(&mut self, addr: u8, cmd: u8, val: u16) -> Result<()> {
        self.unit(Request::WriteWordData {
            bus: self.bus_id,
            addr,
            cmd,
            val,
        })
    }
    fn write_block_data(&mut self, addr: u8, cmd: u8, data: &[u8]) -> Result<()> {
        self.unit(Request::WriteBlockData {
            bus: self.bus_id,
            addr,
            cmd,
            data: data.to_vec(),
        })
    }
    fn supports_block_write(&self) -> bool {
        self.supports_block
    }
}

pub fn open_bus(info: &BusInfo, addresses: &[u8]) -> Result<Box<dyn SmBusSyncOps + Send>> {
    let scope = CapabilityScope::Smbus {
        bus: info.clone(),
        addresses: addresses.to_vec(),
        max_operations_per_second: MAX_OPERATIONS_PER_SECOND,
        max_operations: MAX_OPERATIONS_PER_CAPABILITY,
    };
    let mut pipe = connect_or_spawn(&scope)?;
    let bus_id = expect_opened(
        pipe.request(&Request::OpenBus { info: info.clone() })?,
        "smbus",
    )?;

    // Cache block-write support up front so the trait's `&self` accessor is free.
    let supports_block = match pipe.request(&Request::SupportsBlockWrite { bus: bus_id })? {
        Response::Bool(b) => b,
        other => {
            log::debug!("[register_ops] supports_block_write reply {other:?}; assuming false");
            false
        }
    };

    Ok(Box::new(BrokerBus {
        pipe,
        bus_id,
        supports_block,
    }))
}

// ── PawnIO over RPC ────────────────────────────────────────────────────────

struct BrokerPawnio {
    // `PawnioOps::execute` takes `&self`, so the connection needs interior
    // mutability for the write-then-read round trip.
    pipe: Mutex<AuthorizedPipe>,
    handle: u32,
}

impl PawnioOps for BrokerPawnio {
    fn execute(&self, function: &str, args: &[u64]) -> Result<Vec<u64>> {
        let mut pipe = self.pipe.lock().unwrap_or_else(|e| e.into_inner());
        match pipe.request(&Request::PawnioExec {
            handle: self.handle,
            function: function.to_string(),
            args: args.to_vec(),
        })? {
            Response::Words(w) => Ok(w),
            Response::Error(e) => bail!("broker pawnio: {e}"),
            other => bail!("unexpected broker reply: {other:?}"),
        }
    }
}

pub fn open_pawnio(module: &str) -> Result<Box<dyn PawnioOps>> {
    let functions: &[&str] = match module {
        "AMDFamily17.bin" => &["ioctl_read_smn"],
        "LpcIO.bin" => &[
            "ioctl_select_slot",
            "ioctl_find_bars",
            "ioctl_pio_inb",
            "ioctl_pio_outb",
            "ioctl_superio_inb",
            "ioctl_superio_outb",
        ],
        other => bail!("broker PawnIO module is not allowlisted: {other}"),
    };
    let scope = CapabilityScope::Pawnio {
        module: module.to_string(),
        functions: functions
            .iter()
            .map(|function| (*function).to_string())
            .collect(),
        max_operations_per_second: MAX_OPERATIONS_PER_SECOND,
        max_operations: MAX_OPERATIONS_PER_CAPABILITY,
    };
    let mut pipe = connect_or_spawn(&scope)?;
    let handle = expect_opened(
        pipe.request(&Request::PawnioOpen {
            module: module.to_string(),
        })?,
        "pawnio module",
    )?;
    Ok(Box::new(BrokerPawnio {
        pipe: Mutex::new(pipe),
        handle,
    }))
}
