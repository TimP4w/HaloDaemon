// SPDX-License-Identifier: GPL-3.0-or-later
//! The broker's RPC accept loop and per-connection dispatcher.
//!
//! Each accepted worker connection is served on its own thread with its own
//! bus/module handle maps, so handle ids are connection-scoped and every open
//! register bus / PawnIO module is dropped (closed) when that connection ends.
//! This is also why PawnIO is handle-based: each `LpcIoBus` on the daemon side
//! gets its own broker-side [`PawnioModule`], keeping per-chip `select_slot` /
//! `find_bars` state isolated.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;

use halod_hwaccess::pawnio::{PawnioModule, PawnioOps};
use halod_hwaccess::proto::{self, Request, Response, PIPE_NAME};
use halod_hwaccess::smbus::{self, SmBusSyncOps};
use halod_hwaccess::winsec;

use crate::pipe::{create_instance, wait_for_client, PipeSecurity, PipeStream};

/// Live client connections, and whether any client has ever connected. Used by
/// [`wait_until_idle`] so the on-demand service can stop itself once its worker
/// is gone rather than sitting elevated forever.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static EVER_CONNECTED: AtomicBool = AtomicBool::new(false);

/// Accept connections forever, serving each on its own thread. Returns only on
/// a fatal error creating the secured pipe.
pub fn serve_forever() -> Result<()> {
    let sec = PipeSecurity::from_sddl(&winsec::interactive_dacl_sddl())?;
    log::info!("[broker] pipe secured to interactive users + SYSTEM");

    loop {
        let handle = create_instance(PIPE_NAME, &sec)?;
        let stream = PipeStream::new(handle);
        if let Err(e) = wait_for_client(handle) {
            log::warn!("[broker] wait_for_client failed: {e}");
            drop(stream);
            continue;
        }
        EVER_CONNECTED.store(true, Ordering::SeqCst);
        ACTIVE.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            serve(stream);
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// Block until the broker has served at least one client and then had zero live
/// connections continuously for `grace`, so the caller can stop the elevated
/// service. The worker holds its bus handles for the whole session, so in
/// practice this returns shortly after the worker exits (all connections drop).
pub fn wait_until_idle(grace: Duration) {
    let mut empty_since: Option<Instant> = None;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let idle = EVER_CONNECTED.load(Ordering::SeqCst) && ACTIVE.load(Ordering::SeqCst) == 0;
        if idle {
            let since = *empty_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= grace {
                return;
            }
        } else {
            empty_since = None;
        }
    }
}

/// Per-connection handle tables. Ids start at 1 so 0 is never a valid handle.
#[derive(Default)]
struct Conn {
    buses: HashMap<u32, Box<dyn SmBusSyncOps + Send>>,
    next_bus: u32,
    pawnio: HashMap<u32, PawnioModule>,
    next_pawnio: u32,
}

fn serve(mut stream: PipeStream) {
    let mut conn = Conn::default();
    loop {
        let req: Request = match proto::read_frame(&mut stream) {
            Ok(r) => r,
            Err(e) => {
                log::debug!("[broker] connection closed: {e}");
                break;
            }
        };
        let resp = dispatch(&mut conn, req);
        if let Err(e) = proto::write_frame(&mut stream, &resp) {
            log::debug!("[broker] reply write failed: {e}");
            break;
        }
    }
}

/// Turn any `Result<Response>` into a `Response`, folding errors into
/// [`Response::Error`] so a single failing op never tears down the connection.
fn ok_or_err(r: Result<Response>) -> Response {
    r.unwrap_or_else(|e| Response::Error(format!("{e:#}")))
}

fn dispatch(conn: &mut Conn, req: Request) -> Response {
    match req {
        Request::Enumerate => Response::Buses(smbus::enumerate_buses()),
        Request::EnumerateGpu => Response::Buses(smbus::enumerate_gpu_buses()),

        Request::OpenBus { info } => ok_or_err((|| {
            let bus = smbus::open_bus(&info)?;
            conn.next_bus += 1;
            let id = conn.next_bus;
            log::info!(
                "[broker] open bus {id}: #{} {} ({:04x}:{:04x})",
                info.bus_number,
                info.adapter_name,
                info.pci_vendor,
                info.pci_device
            );
            conn.buses.insert(id, bus);
            Ok(Response::Opened(id))
        })()),

        Request::ReadByte { bus, addr } => with_bus(conn, bus, |b| {
            log::debug!("[broker] bus {bus} read_byte addr=0x{addr:02x}");
            Ok(Response::Byte(b.read_byte(addr)?))
        }),
        Request::ReadByteData { bus, addr, cmd } => with_bus(conn, bus, |b| {
            log::debug!("[broker] bus {bus} read_byte_data addr=0x{addr:02x} cmd=0x{cmd:02x}");
            Ok(Response::Byte(b.read_byte_data(addr, cmd)?))
        }),
        Request::WriteQuick { bus, addr } => with_bus(conn, bus, |b| {
            log::info!("[broker] bus {bus} write_quick addr=0x{addr:02x}");
            Ok(Response::Bool(b.write_quick(addr)?))
        }),
        Request::WriteByteData {
            bus,
            addr,
            cmd,
            val,
        } => with_bus(conn, bus, |b| {
            log::info!(
                "[broker] bus {bus} write_byte_data addr=0x{addr:02x} cmd=0x{cmd:02x} val=0x{val:02x}"
            );
            b.write_byte_data(addr, cmd, val)?;
            Ok(Response::Unit)
        }),
        Request::WriteWordData {
            bus,
            addr,
            cmd,
            val,
        } => with_bus(conn, bus, |b| {
            log::info!(
                "[broker] bus {bus} write_word_data addr=0x{addr:02x} cmd=0x{cmd:02x} val=0x{val:04x}"
            );
            b.write_word_data(addr, cmd, val)?;
            Ok(Response::Unit)
        }),
        Request::WriteBlockData {
            bus,
            addr,
            cmd,
            data,
        } => with_bus(conn, bus, |b| {
            log::info!(
                "[broker] bus {bus} write_block_data addr=0x{addr:02x} cmd=0x{cmd:02x} len={}",
                data.len()
            );
            b.write_block_data(addr, cmd, &data)?;
            Ok(Response::Unit)
        }),
        Request::SupportsBlockWrite { bus } => {
            with_bus(conn, bus, |b| Ok(Response::Bool(b.supports_block_write())))
        }

        Request::PawnioOpen { module } => ok_or_err((|| {
            let m = PawnioModule::open(&[module.as_str()])?;
            conn.next_pawnio += 1;
            let id = conn.next_pawnio;
            log::info!("[broker] open pawnio {id}: module {module}");
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
                .ok_or_else(|| anyhow::anyhow!("unknown pawnio handle {handle}"))?;
            log::debug!("[broker] pawnio {handle} exec {function} args={args:?}");
            Ok(Response::Words(m.execute(&function, &args)?))
        })()),
    }
}

/// Run `f` against the opened bus `bus`, or reply with an error if the id is
/// unknown to this connection.
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
