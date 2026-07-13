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
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use halod_hwaccess::pawnio::{PawnioModule, PawnioOps};
use halod_hwaccess::proto::{self, Request, Response, PIPE_NAME};
use halod_hwaccess::smbus::{self, SmBusSyncOps};
use halod_hwaccess::winsec;

use crate::clientauth::{self, Admission, Gate};
use crate::pipe::{create_instance, wait_for_client, PipeSecurity, PipeStream};

/// Most concurrent client connections. The trusted daemon opens one connection
/// per bus/module handle it holds; this bounds a buggy or hostile peer from
/// exhausting threads and pipe instances (RF-13).
const MAX_CLIENTS: usize = 64;

/// Most bus / PawnIO handles a single connection may open, so one connection
/// cannot grow its handle maps without bound (RF-13).
const MAX_HANDLES_PER_KIND: usize = 64;

/// Gate state shared across accept threads: the bound coordinator identity and
/// the live connection count. [`wait_until_idle`] reads the count to stop the
/// on-demand service once its worker is gone rather than sitting elevated.
static GATE: Mutex<Gate> = Mutex::new(Gate::new());

fn gate() -> MutexGuard<'static, Gate> {
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

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

        // Bind the broker to exactly one interactive user: identify the client
        // and admit only the coordinator (see `clientauth`). A refused client is
        // disconnected by dropping its stream.
        let identity = match clientauth::pipe_client_identity(handle) {
            Ok(id) => id,
            Err(e) => {
                log::warn!("[broker] could not identify pipe client ({e}); refusing");
                drop(stream);
                continue;
            }
        };
        match clientauth::decide(&mut gate(), &identity, MAX_CLIENTS) {
            Admission::Ok => {
                std::thread::spawn(move || {
                    serve(stream);
                    clientauth::release(&mut gate());
                });
            }
            Admission::WrongUser => {
                log::warn!(
                    "[broker] refusing client SID {} session {}: broker is bound to another user",
                    identity.sid,
                    identity.session
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

/// Block until there have been zero live connections continuously for `grace`,
/// so the caller can stop the elevated service. The timer runs from startup, so
/// this also fires if a client never connects at all (e.g. the worker spawned
/// the broker but then gave up before it was ready) — the elevated helper must
/// not linger with no client. The worker holds its bus handles for the whole
/// session, so in the normal case this returns shortly after the worker exits.
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

/// Per-connection handle tables. Ids start at 1 so 0 is never a valid handle.
#[derive(Default)]
struct Conn {
    buses: HashMap<u32, Box<dyn SmBusSyncOps + Send>>,
    next_bus: u32,
    pawnio: HashMap<u32, PawnioModule>,
    next_pawnio: u32,
}

/// Allocate the next handle id in `next`, refusing once `map` is at
/// `MAX_HANDLES_PER_KIND` or the counter would overflow. Keeps id allocation
/// checked and the per-connection handle maps bounded (RF-13).
fn next_handle_id<V>(map: &HashMap<u32, V>, next: &mut u32, kind: &str) -> Result<u32> {
    if map.len() >= MAX_HANDLES_PER_KIND {
        bail!("too many open {kind} handles on this connection (max {MAX_HANDLES_PER_KIND})");
    }
    let id = next
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("{kind} handle id space exhausted"))?;
    *next = id;
    Ok(id)
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
            let id = next_handle_id(&conn.buses, &mut conn.next_bus, "bus")?;
            let bus = smbus::open_bus(&info)?;
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
            let id = next_handle_id(&conn.pawnio, &mut conn.next_pawnio, "pawnio")?;
            let m = PawnioModule::open(&[module.as_str()])?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_ids_start_at_one_and_increment() {
        let map: HashMap<u32, ()> = HashMap::new();
        let mut next = 0;
        assert_eq!(next_handle_id(&map, &mut next, "bus").unwrap(), 1);
        assert_eq!(next_handle_id(&map, &mut next, "bus").unwrap(), 2);
    }

    #[test]
    fn handle_ids_are_refused_at_the_per_connection_cap() {
        let map: HashMap<u32, ()> = (0..MAX_HANDLES_PER_KIND as u32).map(|i| (i, ())).collect();
        let mut next = 0;
        assert!(next_handle_id(&map, &mut next, "bus").is_err());
    }

    #[test]
    fn handle_id_allocation_is_checked_against_overflow() {
        let map: HashMap<u32, ()> = HashMap::new();
        let mut next = u32::MAX;
        assert!(next_handle_id(&map, &mut next, "bus").is_err());
    }
}
