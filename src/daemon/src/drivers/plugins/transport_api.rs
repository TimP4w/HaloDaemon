// SPDX-License-Identifier: GPL-3.0-or-later
//! The `transport` object a plugin script uses to move bytes. It fronts one of
//! the two [`PluginIo`] shapes:
//!
//! - **Stream** (HID): `write`/`read`/… — bytes cross as Lua strings or
//!   `halod.buffer`s. The script sees synchronous calls; each drives the
//!   daemon's async transport via a captured runtime handle (`block_on`), legal
//!   because the worker runs on its own `std::thread`.
//! - **Register** (SMBus): `batch(fn)` — the callback runs against a scoped
//!   `ops` object inside one atomic bus-lock hold (`run_local`). `ops` exposes
//!   addressed register I/O; every op is checked against the plugin's declared
//!   address scope, so a script can never reach an address it didn't declare.
//!
//! Calling a stream method on a register transport (or vice-versa) raises a
//! clear Lua error.

use mlua::{Function, UserData, UserDataMethods, Value};
use tokio::runtime::Handle;

use crate::drivers::transports::smbus::SmBusSyncOps;
use crate::drivers::transports::Transport;

use super::ffi::{bytes_from, to_lua_err};
use super::transport::{AddrScope, BulkEndpoint, PluginIo, RegisterBus};

/// Lua userdata wrapping one transport. Rate limiting is inherited: this holds
/// the real (metered) transport, so a script cannot outrun the hardware.
pub struct TransportApi {
    io: PluginIo,
    handle: Handle,
}

impl TransportApi {
    pub fn new(io: PluginIo, handle: Handle) -> Self {
        Self { io, handle }
    }

    fn stream(&self) -> mlua::Result<&std::sync::Arc<dyn Transport>> {
        match &self.io {
            PluginIo::Stream { transport, .. } => Ok(transport),
            PluginIo::Register(_) => Err(mlua::Error::RuntimeError(
                "this transport is a register bus (SMBus); use transport:batch(fn)".into(),
            )),
        }
    }

    fn bulk(&self) -> mlua::Result<&BulkEndpoint> {
        match &self.io {
            PluginIo::Stream {
                bulk: Some(bulk), ..
            } => Ok(bulk),
            _ => Err(mlua::Error::RuntimeError(
                "this transport has no bulk endpoint".into(),
            )),
        }
    }

    fn register(&self) -> mlua::Result<&RegisterBus> {
        match &self.io {
            PluginIo::Register(r) => Ok(r),
            PluginIo::Stream { .. } => Err(mlua::Error::RuntimeError(
                "this transport is a byte stream (HID); use transport:write/read".into(),
            )),
        }
    }
}

impl UserData for TransportApi {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // ── Stream (HID) ─────────────────────────────────────────────────
        // The common shapes: `write` (bytes → unit), `read`/`read_nonblocking`
        // (size → string), and `write_then_read`/`feature_exchange`
        // (bytes+size → string). Each drives the async transport via `block_on`.
        macro_rules! stream_method {
            (bytes_unit $name:literal, $m:ident) => {
                methods.add_method($name, |_, this, data: Value| {
                    let bytes = bytes_from(&data)?;
                    this.handle
                        .block_on(this.stream()?.$m(&bytes))
                        .map_err(to_lua_err)
                });
            };
            (size_str $name:literal, $m:ident) => {
                methods.add_method($name, |lua, this, size: usize| {
                    let data = this
                        .handle
                        .block_on(this.stream()?.$m(size))
                        .map_err(to_lua_err)?;
                    lua.create_string(&data)
                });
            };
            (bytes_size_str $name:literal, $m:ident) => {
                methods.add_method($name, |lua, this, (data, size): (Value, usize)| {
                    let bytes = bytes_from(&data)?;
                    let reply = this
                        .handle
                        .block_on(this.stream()?.$m(&bytes, size))
                        .map_err(to_lua_err)?;
                    lua.create_string(&reply)
                });
            };
        }
        stream_method!(bytes_unit "write", write);
        stream_method!(size_str "read", read);
        stream_method!(size_str "read_nonblocking", read_nonblocking);
        stream_method!(bytes_size_str "write_then_read", write_then_read);
        stream_method!(bytes_size_str "feature_exchange", feature_exchange);

        methods.add_method("write_many", |_, this, packets: Vec<Value>| {
            let owned: Vec<Vec<u8>> = packets
                .iter()
                .map(bytes_from)
                .collect::<mlua::Result<_>>()?;
            this.handle
                .block_on(this.stream()?.write_many(&owned))
                .map_err(to_lua_err)
        });

        // Push a payload over the device's USB bulk-OUT endpoint (LCD images).
        // Blocking, on the worker thread; the endpoint opens on first use.
        methods.add_method("write_bulk", |_, this, data: Value| {
            let bytes = bytes_from(&data)?;
            this.bulk()?.write(&bytes).map_err(to_lua_err)
        });

        // ── Register (SMBus) ─────────────────────────────────────────────
        // One atomic batch: the callback receives a scoped `ops` object and
        // runs entirely under one bus-lock hold. Read results drive its control
        // flow (probing, the ENE broadcast remap). Returns the callback's value.
        methods.add_method("batch", |lua, this, func: Function| {
            let reg = this.register()?;
            reg.run_local(|ops, scope| {
                let scoped = ScopedOps { ops, scope };
                lua.scope(|s| {
                    let ud = s.create_userdata(scoped)?;
                    func.call::<Value>(ud)
                })
                .map_err(|e| anyhow::anyhow!("{e}"))
            })
            .map_err(to_lua_err)
        });
    }
}

/// The `ops` object handed to a `transport:batch(fn)` callback. Lives only for
/// the duration of the callback (an mlua scoped userdata), borrowing the bus's
/// synchronous op interface. Reads return `nil` on NAK/error and writes return
/// a success bool, so the script branches on hardware responses; an op naming
/// an address outside the plugin's scope raises.
struct ScopedOps<'a> {
    ops: &'a mut dyn SmBusSyncOps,
    scope: &'a AddrScope,
}

impl UserData for ScopedOps<'_> {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("read_byte", |_, this, addr: u8| {
            this.scope.check(addr).map_err(to_lua_err)?;
            Ok(this.ops.read_byte(addr).ok())
        });
        methods.add_method_mut("read_byte_data", |_, this, (addr, cmd): (u8, u8)| {
            this.scope.check(addr).map_err(to_lua_err)?;
            Ok(this.ops.read_byte_data(addr, cmd).ok())
        });
        methods.add_method_mut("write_quick", |_, this, addr: u8| {
            this.scope.check(addr).map_err(to_lua_err)?;
            Ok(this.ops.write_quick(addr).unwrap_or(false))
        });
        methods.add_method_mut(
            "write_byte_data",
            |_, this, (addr, cmd, val): (u8, u8, u8)| {
                this.scope.check(addr).map_err(to_lua_err)?;
                Ok(this.ops.write_byte_data(addr, cmd, val).is_ok())
            },
        );
        methods.add_method_mut(
            "write_word_data",
            |_, this, (addr, cmd, val): (u8, u8, u16)| {
                this.scope.check(addr).map_err(to_lua_err)?;
                Ok(this.ops.write_word_data(addr, cmd, val).is_ok())
            },
        );
        methods.add_method_mut(
            "write_block_data",
            |_, this, (addr, cmd, data): (u8, u8, Value)| {
                this.scope.check(addr).map_err(to_lua_err)?;
                let bytes = bytes_from(&data)?;
                Ok(this.ops.write_block_data(addr, cmd, &bytes).is_ok())
            },
        );
        methods.add_method("supports_block_write", |_, this, ()| {
            Ok(this.ops.supports_block_write())
        });
    }
}
