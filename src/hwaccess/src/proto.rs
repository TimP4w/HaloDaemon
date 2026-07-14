// SPDX-License-Identifier: GPL-3.0-or-later
//! Wire protocol between the daemon (client) and the elevated broker (server).
//!
//! The surface is exactly the register-bus primitives: the [`crate::smbus`]
//! enumeration + `SmBusSyncOps` methods, AMD SMN reads, and the fixed LPC
//! operations used by SuperIO. There is deliberately no filesystem,
//! process-spawn, module-name, or generic function-execution operation — that
//! narrowness is the security value of the split.
//!
//! Framing is a `u32` little-endian length prefix followed by that many bytes
//! of JSON. This is a local IPC (named pipe / same-user socket), not a network
//! protocol, so JSON's readability is worth more than a compact binary codec,
//! and there is no versioning surface to design for.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::smbus::BusInfo;

/// Named pipe the broker serves and the daemon connects to. Restricted to the
/// concrete coordinator SID by the broker's protected DACL.
pub const PIPE_NAME: &str = r"\\.\pipe\halod-broker";

/// SCM name of the on-demand LocalSystem broker service. The installer registers
/// it under this name and the worker starts it (via a granted `SERVICE_START`
/// right) the first time it needs a register bus. Shared so the broker
/// (register) and the daemon (start) agree on one string.
pub const BROKER_SERVICE_NAME: &str = "HalodBroker";

/// Lifetime of one connection-bound authorization. Clients renew before it
/// expires; the bootstrap secret itself exists only for one broker process.
pub const CAPABILITY_TTL_MS: u64 = 60_000;

/// Hard protocol bounds. The broker may clamp a requested scope further, but
/// never permits a caller to raise these ceilings.
/// Covers the complete `u8` address representation without making the scope
/// list unbounded. Runtime-loaded plugins may declare any future device
/// addresses; no broker rebuild or hard-coded address allowlist is involved.
pub const MAX_SCOPE_ADDRESSES: usize = 256;
// One SMBus capability is shared by every device discovered on that physical
// bus. Four-to-eight RGB DIMMs at 60 FPS legitimately produce several thousand
// small register RPCs per second, so this DoS ceiling must sit above that
// aggregate. Hardware-specific byte-rate safety remains enforced in the daemon.
pub const MAX_OPERATIONS_PER_SECOND: u32 = 20_000;
// Clients renew halfway through the 60-second TTL. Keep the total above one
// full half-life at the maximum request rate so a legitimate stream cannot
// exhaust its capability before it is eligible for renewal.
pub const MAX_OPERATIONS_PER_CAPABILITY: u32 = 1_000_000;

/// Exact elevated surface requested for one named-pipe connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapabilityScope {
    Smbus {
        bus: BusInfo,
        addresses: Vec<u8>,
        max_operations_per_second: u32,
        max_operations: u32,
    },
    AmdSmn {
        max_operations_per_second: u32,
        max_operations: u32,
    },
    LpcIo {
        max_operations_per_second: u32,
        max_operations: u32,
    },
}

/// A request from the daemon to the broker. `bus` fields carry a broker-side
/// bus handle id previously returned by [`Response::Opened`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// First frame on a connection. The bootstrap secret is delivered through
    /// the SCM start arguments (or the dev-run UAC command line), never over a
    /// shared persistent channel. The returned capability is bound to this
    /// connection, identity, scope, and a short expiry.
    Authenticate {
        bootstrap_token: String,
        scope: CapabilityScope,
    },
    /// Extend an authenticated connection's short-lived capability.
    Renew {
        capability: String,
    },
    /// Enumerate chipset SMBus controllers.
    Enumerate,
    /// Enumerate GPU SMBus/i2c controllers.
    EnumerateGpu,
    /// Open the register bus described by `info`; replies [`Response::Opened`].
    OpenBus {
        info: BusInfo,
    },
    ReadByte {
        bus: u32,
        addr: u8,
    },
    ReadByteData {
        bus: u32,
        addr: u8,
        cmd: u8,
    },
    WriteQuick {
        bus: u32,
        addr: u8,
    },
    WriteByteData {
        bus: u32,
        addr: u8,
        cmd: u8,
        val: u8,
    },
    WriteWordData {
        bus: u32,
        addr: u8,
        cmd: u8,
        val: u16,
    },
    WriteBlockData {
        bus: u32,
        addr: u8,
        cmd: u8,
        data: Vec<u8>,
    },
    SupportsBlockWrite {
        bus: u32,
    },
    OpenAmdSmn,
    ReadSmn {
        handle: u32,
        offset: u32,
    },
    OpenLpcIo,
    LpcSelectSlot {
        handle: u32,
        slot: u8,
    },
    LpcFindBars {
        handle: u32,
    },
    LpcReadPort {
        handle: u32,
        port: u16,
    },
    LpcWritePort {
        handle: u32,
        port: u16,
        value: u8,
    },
    LpcSuperioInb {
        handle: u32,
        register: u8,
    },
    LpcSuperioOutb {
        handle: u32,
        register: u8,
        value: u8,
    },
}

/// A reply from the broker to the daemon. Every op can instead fail with
/// [`Response::Error`], carrying the broker-side error string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Authorized {
        capability: String,
        expires_in_ms: u64,
    },
    Buses(Vec<BusInfo>),
    Opened(u32),
    Dword(u32),
    Byte(u8),
    Bool(bool),
    Unit,
    Error(String),
}

/// Upper bound on a single framed message, guarding the broker against a
/// hostile or corrupt length prefix. A block write (the largest request) is a
/// few dozen bytes; 64 KiB is comfortably generous.
pub const MAX_FRAME_LEN: usize = 64 * 1024;

/// Serialize `msg` to a length-prefixed JSON frame and write it to `w`.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME_LEN",
        ));
    }
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one length-prefixed JSON frame from `r` and deserialize it.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<T> {
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame length exceeds MAX_FRAME_LEN",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn round_trip<T>(msg: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let mut buf = Vec::new();
        write_frame(&mut buf, msg).expect("encode");
        let mut cursor = std::io::Cursor::new(buf);
        read_frame(&mut cursor).expect("decode")
    }

    fn arb_bus_info() -> impl Strategy<Value = BusInfo> {
        (
            any::<u8>(),
            ".*",
            any::<u16>(),
            any::<u16>(),
            any::<u16>(),
            any::<u16>(),
        )
            .prop_map(|(bus_number, adapter_name, pv, pd, psv, psd)| BusInfo {
                bus_number,
                adapter_name,
                pci_vendor: pv,
                pci_device: pd,
                pci_sub_vendor: psv,
                pci_sub_device: psd,
            })
    }

    fn arb_request() -> impl Strategy<Value = Request> {
        prop_oneof![
            (
                ".*",
                arb_bus_info(),
                prop::collection::vec(any::<u8>(), 0..8)
            )
                .prop_map(|(bootstrap_token, bus, addresses)| Request::Authenticate {
                    bootstrap_token,
                    scope: CapabilityScope::Smbus {
                        bus,
                        addresses,
                        max_operations_per_second: 100,
                        max_operations: 1_000,
                    },
                }),
            ".*".prop_map(|bootstrap_token| Request::Authenticate {
                bootstrap_token,
                scope: CapabilityScope::AmdSmn {
                    max_operations_per_second: 100,
                    max_operations: 1_000,
                },
            }),
            ".*".prop_map(|bootstrap_token| Request::Authenticate {
                bootstrap_token,
                scope: CapabilityScope::LpcIo {
                    max_operations_per_second: 100,
                    max_operations: 1_000,
                },
            }),
            ".*".prop_map(|capability| Request::Renew { capability }),
            Just(Request::Enumerate),
            Just(Request::EnumerateGpu),
            arb_bus_info().prop_map(|info| Request::OpenBus { info }),
            (any::<u32>(), any::<u8>()).prop_map(|(bus, addr)| Request::ReadByte { bus, addr }),
            (any::<u32>(), any::<u8>(), any::<u8>())
                .prop_map(|(bus, addr, cmd)| Request::ReadByteData { bus, addr, cmd }),
            (any::<u32>(), any::<u8>()).prop_map(|(bus, addr)| Request::WriteQuick { bus, addr }),
            (any::<u32>(), any::<u8>(), any::<u8>(), any::<u8>()).prop_map(
                |(bus, addr, cmd, val)| Request::WriteByteData {
                    bus,
                    addr,
                    cmd,
                    val
                }
            ),
            (any::<u32>(), any::<u8>(), any::<u8>(), any::<u16>()).prop_map(
                |(bus, addr, cmd, val)| Request::WriteWordData {
                    bus,
                    addr,
                    cmd,
                    val
                }
            ),
            (
                any::<u32>(),
                any::<u8>(),
                any::<u8>(),
                prop::collection::vec(any::<u8>(), 0..40)
            )
                .prop_map(|(bus, addr, cmd, data)| Request::WriteBlockData {
                    bus,
                    addr,
                    cmd,
                    data
                }),
            any::<u32>().prop_map(|bus| Request::SupportsBlockWrite { bus }),
            Just(Request::OpenAmdSmn),
            (any::<u32>(), any::<u32>())
                .prop_map(|(handle, offset)| Request::ReadSmn { handle, offset }),
            Just(Request::OpenLpcIo),
            (any::<u32>(), any::<u8>())
                .prop_map(|(handle, slot)| Request::LpcSelectSlot { handle, slot }),
            any::<u32>().prop_map(|handle| Request::LpcFindBars { handle }),
            (any::<u32>(), any::<u16>())
                .prop_map(|(handle, port)| Request::LpcReadPort { handle, port }),
            (any::<u32>(), any::<u16>(), any::<u8>()).prop_map(|(handle, port, value)| {
                Request::LpcWritePort {
                    handle,
                    port,
                    value,
                }
            }),
            (any::<u32>(), any::<u8>())
                .prop_map(|(handle, register)| { Request::LpcSuperioInb { handle, register } }),
            (any::<u32>(), any::<u8>(), any::<u8>()).prop_map(|(handle, register, value)| {
                Request::LpcSuperioOutb {
                    handle,
                    register,
                    value,
                }
            }),
        ]
    }

    fn arb_response() -> impl Strategy<Value = Response> {
        prop_oneof![
            ".*".prop_map(|capability| Response::Authorized {
                capability,
                expires_in_ms: CAPABILITY_TTL_MS,
            }),
            prop::collection::vec(arb_bus_info(), 0..4).prop_map(Response::Buses),
            any::<u32>().prop_map(Response::Opened),
            any::<u32>().prop_map(Response::Dword),
            any::<u8>().prop_map(Response::Byte),
            any::<bool>().prop_map(Response::Bool),
            Just(Response::Unit),
            ".*".prop_map(Response::Error),
        ]
    }

    proptest! {
        #[test]
        fn request_frame_round_trips(req in arb_request()) {
            prop_assert_eq!(round_trip(&req), req);
        }

        #[test]
        fn response_frame_round_trips(resp in arb_response()) {
            prop_assert_eq!(round_trip(&resp), resp);
        }
    }

    #[test]
    fn over_long_length_prefix_is_rejected() {
        // A hostile length prefix must not cause a huge allocation / read.
        let mut framed = ((MAX_FRAME_LEN + 1) as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(b"{}");
        let mut cursor = std::io::Cursor::new(framed);
        let got: io::Result<Request> = read_frame(&mut cursor);
        assert!(got.is_err());
    }
}
