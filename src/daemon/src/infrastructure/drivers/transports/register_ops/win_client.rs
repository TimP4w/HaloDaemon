// SPDX-License-Identifier: GPL-3.0-or-later
//! Named-pipe RPC client to the elevated `halod-broker`.
//!
//! One connection per opened SMBus, AMD SMN, or LPC handle. A single blocking
//! request/response stream is retained per handle. [`connect_or_spawn`] brings the
//! broker up on first use: the installed on-demand `HalodBroker` service is
//! started via the SCM (no UAC); a dev run with no service installed falls back
//! to a single `runas` UAC prompt.

use std::fs::{File, OpenOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

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
                match crate::infrastructure::platform::elevation::spawn_broker_elevated(
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
                        // A live broker with a different bootstrap token is
                        // not still starting up. Retrying this same token 160
                        // times only floods the log and cannot succeed; the
                        // coordinator must be restarted (or the stale broker
                        // stopped) to establish a matching session.
                        Ok(Response::Error(error)) => {
                            bail!("halod-broker rejected this daemon's bootstrap token: {error}");
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

#[derive(Debug)]
struct BrokerSessionLost(String);

impl std::fmt::Display for BrokerSessionLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BrokerSessionLost {}

impl AuthorizedPipe {
    fn reauthenticate(&mut self, scope: &CapabilityScope) -> Result<()> {
        let auth = broker_auth()?;
        proto::write_frame(
            &mut self.pipe,
            &Request::Authenticate {
                bootstrap_token: auth.token.clone(),
                scope: scope.clone(),
            },
        )
        .map_err(|e| anyhow!("broker reauthentication send: {e}"))?;
        match proto::read_frame(&mut self.pipe)
            .map_err(|e| anyhow!("broker reauthentication receive: {e}"))?
        {
            Response::Authorized {
                capability,
                expires_in_ms,
            } => {
                self.capability = capability;
                self.renew_at = Instant::now() + Duration::from_millis(expires_in_ms / 2);
                Ok(())
            }
            Response::Error(error) => bail!("broker reauthentication failed: {error}"),
            other => bail!("unexpected broker reauthentication reply: {other:?}"),
        }
    }

    fn request(&mut self, req: &Request) -> Result<Response> {
        if Instant::now() >= self.renew_at {
            proto::write_frame(
                &mut self.pipe,
                &Request::Renew {
                    capability: self.capability.clone(),
                },
            )
            .map_err(|e| BrokerSessionLost(format!("broker capability renewal send: {e}")))?;
            match proto::read_frame(&mut self.pipe)
                .map_err(|e| BrokerSessionLost(format!("broker capability renewal receive: {e}")))?
            {
                Response::Authorized {
                    capability,
                    expires_in_ms,
                } => {
                    self.capability = capability;
                    self.renew_at = Instant::now() + Duration::from_millis(expires_in_ms / 2);
                }
                Response::Error(e) => {
                    return Err(BrokerSessionLost(format!(
                        "broker capability renewal failed: {e}"
                    ))
                    .into());
                }
                other => {
                    return Err(BrokerSessionLost(format!(
                        "unexpected broker capability renewal reply: {other:?}"
                    ))
                    .into());
                }
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
    scope: CapabilityScope,
    info: BusInfo,
    /// Cached at open time so `supports_block_write(&self)` needs no round trip.
    supports_block: bool,
}

impl BrokerBus {
    fn reconnect(&mut self) -> Result<()> {
        match self.pipe.reauthenticate(&self.scope) {
            Ok(()) => {
                let (bus_id, supports_block) = open_bus_on_pipe(&mut self.pipe, &self.info)?;
                self.bus_id = bus_id;
                self.supports_block = supports_block;
                Ok(())
            }
            Err(error) => {
                log::debug!(
                    "[register_ops] broker pipe could not be reauthenticated ({error:#}); \
                     opening a new connection"
                );
                let (pipe, bus_id, supports_block) = connect_broker_bus(&self.scope, &self.info)?;
                self.pipe = pipe;
                self.bus_id = bus_id;
                self.supports_block = supports_block;
                Ok(())
            }
        }
    }

    fn request(&mut self, make_request: impl Fn(u32) -> Request) -> Result<Response> {
        let first = self.pipe.request(&make_request(self.bus_id));
        let reason = match first {
            Ok(Response::Error(ref error)) if error == "capability expired" => {
                format!("broker capability expired: {error}")
            }
            Err(ref error) if error.is::<BrokerSessionLost>() => format!("{error:#}"),
            _ => return first,
        };

        log::info!("[register_ops] SMBus broker session lost; reconnecting ({reason})");
        self.reconnect()
            .with_context(|| format!("recover SMBus broker session after {reason}"))?;
        self.pipe
            .request(&make_request(self.bus_id))
            .context("retry SMBus request after broker session recovery")
    }

    fn byte(&mut self, make_request: impl Fn(u32) -> Request) -> Result<u8> {
        match self.request(make_request)? {
            Response::Byte(b) => Ok(b),
            Response::Error(e) => bail!("broker: {e}"),
            other => bail!("unexpected broker reply: {other:?}"),
        }
    }

    fn unit(&mut self, make_request: impl Fn(u32) -> Request) -> Result<()> {
        match self.request(make_request)? {
            Response::Unit => Ok(()),
            Response::Error(e) => bail!("broker: {e}"),
            other => bail!("unexpected broker reply: {other:?}"),
        }
    }
}

impl SmBusSyncOps for BrokerBus {
    fn read_byte(&mut self, addr: u8) -> Result<u8> {
        self.byte(|bus| Request::ReadByte { bus, addr })
    }
    fn read_byte_data(&mut self, addr: u8, cmd: u8) -> Result<u8> {
        self.byte(|bus| Request::ReadByteData { bus, addr, cmd })
    }
    fn write_quick(&mut self, addr: u8) -> Result<bool> {
        match self.request(|bus| Request::WriteQuick { bus, addr })? {
            Response::Bool(b) => Ok(b),
            Response::Error(e) => bail!("broker: {e}"),
            other => bail!("unexpected broker reply: {other:?}"),
        }
    }
    fn write_byte_data(&mut self, addr: u8, cmd: u8, val: u8) -> Result<()> {
        self.unit(|bus| Request::WriteByteData {
            bus,
            addr,
            cmd,
            val,
        })
    }
    fn write_word_data(&mut self, addr: u8, cmd: u8, val: u16) -> Result<()> {
        self.unit(|bus| Request::WriteWordData {
            bus,
            addr,
            cmd,
            val,
        })
    }
    fn write_block_data(&mut self, addr: u8, cmd: u8, data: &[u8]) -> Result<()> {
        self.unit(|bus| Request::WriteBlockData {
            bus,
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
    let (pipe, bus_id, supports_block) = connect_broker_bus(&scope, info)?;

    Ok(Box::new(BrokerBus {
        pipe,
        bus_id,
        scope,
        info: info.clone(),
        supports_block,
    }))
}

fn connect_broker_bus(
    scope: &CapabilityScope,
    info: &BusInfo,
) -> Result<(AuthorizedPipe, u32, bool)> {
    let mut pipe = connect_or_spawn(scope)?;
    let (bus_id, supports_block) = open_bus_on_pipe(&mut pipe, info)?;
    Ok((pipe, bus_id, supports_block))
}

fn open_bus_on_pipe(pipe: &mut AuthorizedPipe, info: &BusInfo) -> Result<(u32, bool)> {
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

    Ok((bus_id, supports_block))
}

// ── Typed AMD SMN and LPC RPC clients ──────────────────────────────────────

pub struct AmdSmnBrokerClient {
    pipe: Mutex<AuthorizedPipe>,
    handle: u32,
}

impl AmdSmnBrokerClient {
    pub fn read_smn(&self, offset: u32) -> Result<u32> {
        let mut pipe = self.pipe.lock().unwrap_or_else(|e| e.into_inner());
        match pipe.request(&Request::ReadSmn {
            handle: self.handle,
            offset,
        })? {
            Response::Dword(value) => Ok(value),
            Response::Error(e) => bail!("broker AMD SMN: {e}"),
            other => bail!("unexpected broker reply: {other:?}"),
        }
    }
}

pub fn open_amd_smn() -> Result<AmdSmnBrokerClient> {
    let scope = CapabilityScope::AmdSmn {
        max_operations_per_second: MAX_OPERATIONS_PER_SECOND,
        max_operations: MAX_OPERATIONS_PER_CAPABILITY,
    };
    let mut pipe = connect_or_spawn(&scope)?;
    let handle = expect_opened(pipe.request(&Request::OpenAmdSmn)?, "AMD SMN")?;
    Ok(AmdSmnBrokerClient {
        pipe: Mutex::new(pipe),
        handle,
    })
}

pub struct LpcIoBrokerClient {
    pipe: Mutex<AuthorizedPipe>,
    handle: u32,
}

impl LpcIoBrokerClient {
    fn request(&self, request: Request) -> Result<Response> {
        self.pipe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .request(&request)
    }

    fn unit(&self, request: Request) -> Result<()> {
        match self.request(request)? {
            Response::Unit => Ok(()),
            Response::Error(e) => bail!("broker LPC: {e}"),
            other => bail!("unexpected broker reply: {other:?}"),
        }
    }

    fn byte(&self, request: Request) -> Result<u8> {
        match self.request(request)? {
            Response::Byte(value) => Ok(value),
            Response::Error(e) => bail!("broker LPC: {e}"),
            other => bail!("unexpected broker reply: {other:?}"),
        }
    }

    pub fn select_slot(&self, slot: u8) -> Result<()> {
        if slot > 1 {
            bail!("LPC slot {slot} is outside 0..=1");
        }
        self.unit(Request::LpcSelectSlot {
            handle: self.handle,
            slot,
        })
    }

    pub fn find_bars(&self) -> Result<()> {
        self.unit(Request::LpcFindBars {
            handle: self.handle,
        })
    }

    pub fn read_port(&self, port: u16) -> Result<u8> {
        self.byte(Request::LpcReadPort {
            handle: self.handle,
            port,
        })
    }

    pub fn write_port(&self, port: u16, value: u8) -> Result<()> {
        self.unit(Request::LpcWritePort {
            handle: self.handle,
            port,
            value,
        })
    }

    pub fn superio_inb(&self, register: u8) -> Result<u8> {
        self.byte(Request::LpcSuperioInb {
            handle: self.handle,
            register,
        })
    }

    pub fn superio_outb(&self, register: u8, value: u8) -> Result<()> {
        self.unit(Request::LpcSuperioOutb {
            handle: self.handle,
            register,
            value,
        })
    }
}

pub fn open_lpc_io() -> Result<LpcIoBrokerClient> {
    let scope = CapabilityScope::LpcIo {
        max_operations_per_second: MAX_OPERATIONS_PER_SECOND,
        max_operations: MAX_OPERATIONS_PER_CAPABILITY,
    };
    let mut pipe = connect_or_spawn(&scope)?;
    let handle = expect_opened(pipe.request(&Request::OpenLpcIo)?, "LPC I/O")?;
    Ok(LpcIoBrokerClient {
        pipe: Mutex::new(pipe),
        handle,
    })
}
