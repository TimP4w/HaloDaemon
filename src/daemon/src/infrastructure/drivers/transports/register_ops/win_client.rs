// SPDX-License-Identifier: GPL-3.0-or-later
//! Named-pipe RPC client to the elevated `halod-broker`.
//!
//! One connection per opened SMBus, AMD SMN, or LPC handle. A single blocking
//! request/response stream is retained per handle. [`connect_or_spawn`] brings the
//! broker up on first use: the installed on-demand `HalodBroker` service is
//! started via the SCM (no UAC); a dev run with no service installed falls back
//! to a single `runas` UAC prompt.

use parking_lot::Mutex;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;

use crate::infrastructure::platform::elevation;
use halod_hwaccess::proto::{
    self, BrokerError, CapabilityScope, Request, Response, BROKER_SERVICE_NAME,
    MAX_OPERATIONS_PER_SECOND, PIPE_NAME,
};
use halod_hwaccess::smbus::{BusInfo, SmBusSyncOps};
use halod_hwaccess::winsec;

/// How many connect attempts before giving up, and how long to wait between
/// them — enough to cover the SCM starting the on-demand broker, or a dev-run
/// UAC prompt being accepted.
const CONNECT_ATTEMPTS: u32 = 160;
const CONNECT_BACKOFF: Duration = Duration::from_millis(250);

/// Whether a dev run has already fired a UAC prompt, so repeated register-bus
/// opens don't each re-prompt after a decline.
static BROKER_SPAWN_REQUESTED: AtomicBool = AtomicBool::new(false);

static COORDINATOR_SID: OnceLock<std::result::Result<String, String>> = OnceLock::new();

fn coordinator_sid() -> Result<&'static str> {
    COORDINATOR_SID
        .get_or_init(|| {
            winsec::current_process_identity()
                .map(|(sid, _)| sid)
                .map_err(|e| format!("query coordinator identity: {e:#}"))
        })
        .as_deref()
        .map_err(|e| anyhow!(e.clone()))
}

/// Open the broker pipe at identification level only, so a server that is not
/// the broker cannot act as this user, then require the server process to be
/// `halod-broker.exe` next to this daemon. The image check raises the bar
/// against a squatter; it is not a boundary against same-user code.
fn open_broker_pipe() -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .security_qos_flags(SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0)
        .open(PIPE_NAME)
}

fn verify_broker_server(pipe: &File) -> Result<()> {
    let mut pid = 0u32;
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid) }
        .map_err(|e| anyhow!("GetNamedPipeServerProcessId: {e}"))?;
    let actual = winsec::process_image_path(pid)?;
    let expected = winsec::installed_sibling("halod-broker.exe")?;
    if !winsec::same_installed_binary(&expected, &actual) {
        return Err(WrongBrokerServer(format!(
            "broker pipe is served by {} (pid {pid}), not {}",
            actual.display(),
            expected.display()
        ))
        .into());
    }
    Ok(())
}

/// The pipe is served by something other than the installed broker. Retrying
/// cannot change that, so the open fails at once.
#[derive(Debug)]
struct WrongBrokerServer(String);

impl std::fmt::Display for WrongBrokerServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WrongBrokerServer {}

/// Bind a fresh connection to `scope`. A broker refusal is returned as a
/// [`BrokerError`] so the caller can tell it from a transport failure.
fn hello<S: Read + Write>(pipe: &mut S, scope: &CapabilityScope) -> Result<()> {
    proto::write_frame(
        pipe,
        &Request::Hello {
            scope: scope.clone(),
        },
    )
    .context("broker Hello send")?;
    match proto::read_frame(pipe).context("broker Hello receive")? {
        Response::Ready => Ok(()),
        Response::Error(e) => Err(e.into()),
        other => bail!("unexpected broker Hello reply: {other:?}"),
    }
}

/// Start the installed on-demand `HalodBroker` service via the SCM. Returns
/// `Ok` if it started or was already running; `Err` if the service isn't
/// installed (dev run) or we lack rights. The service takes no arguments: it
/// derives its coordinators from its own DACL.
fn try_start_broker_service() -> Result<()> {
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
    service
        .start::<&std::ffi::OsStr>(&[])
        .map_err(|e| anyhow!("start service: {e}"))
}

/// Bring the broker up: prefer the installed on-demand service (no UAC); if it
/// isn't installed, this is a dev run — spawn it via one UAC prompt.
fn ensure_broker_running(sid: &str) {
    match try_start_broker_service() {
        Ok(()) => log::info!("started the {BROKER_SERVICE_NAME} service"),
        Err(e) => {
            log::debug!("broker service unavailable ({e}); trying dev-run UAC spawn");
            if !BROKER_SPAWN_REQUESTED.swap(true, Ordering::SeqCst) {
                match elevation::spawn_broker_elevated(sid) {
                    Ok(()) => log::info!("requested elevated halod-broker (dev)"),
                    Err(se) => log::warn!(
                        "could not launch halod-broker ({se}); \
                         register-bus devices will be unavailable"
                    ),
                }
            }
        }
    }
}

/// Connect to the broker, bringing it up on first attempt if it isn't there yet.
fn connect_or_spawn(scope: &CapabilityScope) -> Result<BrokerPipe> {
    let sid = coordinator_sid()?;
    let mut ensured = false;
    for attempt in 0..CONNECT_ATTEMPTS {
        match open_broker_pipe() {
            Ok(mut pipe) => {
                match verify_broker_server(&pipe).and_then(|()| hello(&mut pipe, scope)) {
                    Ok(()) => {
                        // A later broker exit may legitimately require a fresh
                        // dev-run UAC launch.
                        BROKER_SPAWN_REQUESTED.store(false, Ordering::SeqCst);
                        return Ok(BrokerPipe { pipe });
                    }
                    Err(e) if e.is::<BrokerError>() || e.is::<WrongBrokerServer>() => {
                        log::warn!("halod-broker connection refused: {e}");
                        return Err(e.context("halod-broker connection refused"));
                    }
                    Err(e) => log::debug!("broker connect attempt {attempt} failed: {e:#}"),
                }
            }
            Err(e) => {
                if !ensured {
                    ensured = true;
                    ensure_broker_running(sid);
                }
                log::debug!("broker connect attempt {attempt} failed: {e}");
            }
        }
        std::thread::sleep(CONNECT_BACKOFF);
    }
    bail!("could not reach halod-broker on {PIPE_NAME}")
}

struct BrokerPipe {
    pipe: File,
}

/// The request never reached the broker, so it is safe to reconnect and retry.
#[derive(Debug)]
struct BrokerSessionLost(String);

impl std::fmt::Display for BrokerSessionLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BrokerSessionLost {}

impl BrokerPipe {
    fn request(&mut self, req: &Request) -> Result<Response> {
        proto::write_frame(&mut self.pipe, req)
            .map_err(|e| BrokerSessionLost(format!("broker send: {e}")))?;
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
    pipe: BrokerPipe,
    bus_id: u32,
    scope: CapabilityScope,
    info: BusInfo,
    /// Cached at open time so `supports_block_write(&self)` needs no round trip.
    supports_block: bool,
}

impl BrokerBus {
    fn reconnect(&mut self) -> Result<()> {
        let (pipe, bus_id, supports_block) = connect_broker_bus(&self.scope, &self.info)?;
        self.pipe = pipe;
        self.bus_id = bus_id;
        self.supports_block = supports_block;
        Ok(())
    }

    fn request(&mut self, make_request: impl Fn(u32) -> Request) -> Result<Response> {
        match self.pipe.request(&make_request(self.bus_id)) {
            Err(error) if error.is::<BrokerSessionLost>() => {
                log::info!("SMBus broker session lost; reconnecting ({error:#})");
                self.reconnect()
                    .with_context(|| format!("recover SMBus broker session after {error:#}"))?;
                self.pipe
                    .request(&make_request(self.bus_id))
                    .context("retry SMBus request after broker session recovery")
            }
            other => other,
        }
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

fn connect_broker_bus(scope: &CapabilityScope, info: &BusInfo) -> Result<(BrokerPipe, u32, bool)> {
    let mut pipe = connect_or_spawn(scope)?;
    let (bus_id, supports_block) = open_bus_on_pipe(&mut pipe, info)?;
    Ok((pipe, bus_id, supports_block))
}

fn open_bus_on_pipe(pipe: &mut BrokerPipe, info: &BusInfo) -> Result<(u32, bool)> {
    let bus_id = expect_opened(
        pipe.request(&Request::OpenBus { info: info.clone() })?,
        "smbus",
    )?;

    // Cache block-write support up front so the trait's `&self` accessor is free.
    let supports_block = match pipe.request(&Request::SupportsBlockWrite { bus: bus_id })? {
        Response::Bool(b) => b,
        other => {
            log::debug!("supports_block_write reply {other:?}; assuming false");
            false
        }
    };

    Ok((bus_id, supports_block))
}

// ── Typed AMD SMN and LPC RPC clients ──────────────────────────────────────

pub struct AmdSmnBrokerClient {
    pipe: Mutex<BrokerPipe>,
    handle: u32,
}

impl AmdSmnBrokerClient {
    pub fn read_smn(&self, offset: u32) -> Result<u32> {
        let mut pipe = self.pipe.lock();
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
    };
    let mut pipe = connect_or_spawn(&scope)?;
    let handle = expect_opened(pipe.request(&Request::OpenAmdSmn)?, "AMD SMN")?;
    Ok(AmdSmnBrokerClient {
        pipe: Mutex::new(pipe),
        handle,
    })
}

pub struct LpcIoBrokerClient {
    pipe: Mutex<BrokerPipe>,
    handle: u32,
}

impl LpcIoBrokerClient {
    fn request(&self, request: Request) -> Result<Response> {
        self.pipe.lock().request(&request)
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
    };
    let mut pipe = connect_or_spawn(&scope)?;
    let handle = expect_opened(pipe.request(&Request::OpenLpcIo)?, "LPC I/O")?;
    Ok(LpcIoBrokerClient {
        pipe: Mutex::new(pipe),
        handle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Duplex test stream: reads scripted broker replies, records requests.
    struct Duplex {
        replies: Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }

    impl Duplex {
        fn replying(reply: &Response) -> Self {
            let mut replies = Vec::new();
            proto::write_frame(&mut replies, reply).unwrap();
            Self {
                replies: Cursor::new(replies),
                sent: Vec::new(),
            }
        }
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.replies.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.sent.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn lpc() -> CapabilityScope {
        CapabilityScope::LpcIo {
            max_operations_per_second: MAX_OPERATIONS_PER_SECOND,
        }
    }

    #[test]
    fn hello_sends_the_scope_and_accepts_ready() {
        let mut pipe = Duplex::replying(&Response::Ready);
        hello(&mut pipe, &lpc()).unwrap();
        let sent: Request = proto::read_frame(&mut Cursor::new(pipe.sent)).unwrap();
        assert_eq!(sent, Request::Hello { scope: lpc() });
    }

    #[test]
    fn hello_refusal_is_a_broker_error_not_a_transport_failure() {
        let mut pipe = Duplex::replying(&Response::Error(BrokerError::Failed("no".into())));
        assert!(hello(&mut pipe, &lpc()).unwrap_err().is::<BrokerError>());

        let mut pipe = Duplex::replying(&Response::Opened(1));
        assert!(!hello(&mut pipe, &lpc()).unwrap_err().is::<BrokerError>());

        let mut silent = Duplex {
            replies: Cursor::new(Vec::new()),
            sent: Vec::new(),
        };
        assert!(!hello(&mut silent, &lpc()).unwrap_err().is::<BrokerError>());
    }

    #[test]
    fn a_server_that_is_not_the_installed_broker_is_refused() {
        // This test binary serves the pipe; it is not halod-broker.exe.
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        use windows::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT};

        let name = format!(r"\\.\pipe\halod-client-test-{}", std::process::id());
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let server = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                1,
                0,
                0,
                0,
                None,
            )
        };
        assert!(!server.is_invalid());
        let client = OpenOptions::new()
            .read(true)
            .write(true)
            .security_qos_flags(SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0)
            .open(&name)
            .unwrap();
        let error = verify_broker_server(&client).unwrap_err();
        assert!(error.is::<WrongBrokerServer>(), "{error:#}");
        drop(client);
        unsafe {
            let _ = CloseHandle(server);
        }
    }
}
