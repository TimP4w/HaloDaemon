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

#[cfg(target_os = "linux")]
use mlua::LuaSerdeExt;
use mlua::{Function, UserData, UserDataMethods, Value};
use tokio::runtime::Handle;

use crate::infrastructure::drivers::transports::serial::SerialControl;
use crate::infrastructure::drivers::transports::smbus::SmBusSyncOps;
use crate::infrastructure::drivers::transports::usb::{UsbCollection, UsbControlResult};
use crate::infrastructure::drivers::transports::{HidTransport, Transport};

use super::bytebuf::check_alloc;
use super::ffi::{bytes_from, to_lua_err};
use super::transport::{command_result_table, AddrScope, CommandExecutor, PluginIo, RegisterBus};

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
            _ => Err(mlua::Error::RuntimeError(
                "this transport is not a byte stream; use transport:write/read on a HID device"
                    .into(),
            )),
        }
    }

    fn hid(&self) -> mlua::Result<&dyn HidTransport> {
        self.stream()?.as_hid().ok_or_else(|| {
            mlua::Error::RuntimeError("this byte stream is not a HID transport".into())
        })
    }

    fn serial(&self) -> mlua::Result<&dyn SerialControl> {
        self.stream()?.as_serial().ok_or_else(|| {
            mlua::Error::RuntimeError("this byte stream is not a serial port".into())
        })
    }

    fn usb(&self) -> mlua::Result<&dyn UsbCollection> {
        match &self.io {
            PluginIo::Stream { usb: Some(usb), .. } | PluginIo::Usb(usb) => Ok(usb.as_ref()),
            _ => Err(mlua::Error::RuntimeError(
                "this transport has no declared USB endpoint collection".into(),
            )),
        }
    }

    fn register(&self) -> mlua::Result<&RegisterBus> {
        match &self.io {
            PluginIo::Register(r) => Ok(r),
            _ => Err(mlua::Error::RuntimeError(
                "this transport is not a register bus (SMBus); use transport:batch(fn)".into(),
            )),
        }
    }

    fn command(&self) -> mlua::Result<&CommandExecutor> {
        match &self.io {
            PluginIo::Command(command) => Ok(command),
            _ => Err(mlua::Error::RuntimeError(
                "this transport cannot execute commands".into(),
            )),
        }
    }

    #[cfg(target_os = "linux")]
    fn hwmon(
        &self,
    ) -> mlua::Result<
        &std::sync::Arc<crate::infrastructure::drivers::transports::hwmon::HwmonTransport>,
    > {
        match &self.io {
            PluginIo::Hwmon(bus) => Ok(bus),
            _ => Err(mlua::Error::RuntimeError(
                "this transport is not Linux hwmon".into(),
            )),
        }
    }

    #[cfg(target_os = "windows")]
    fn amd_smn(
        &self,
    ) -> mlua::Result<&std::sync::Arc<crate::infrastructure::drivers::transports::amd_smn::AmdSmnBus>>
    {
        match &self.io {
            PluginIo::AmdSmn(bus) => Ok(bus),
            _ => Err(mlua::Error::RuntimeError(
                "this transport is not AMD SMN; use amd_smn:read(offset) only on an amd_smn device"
                    .into(),
            )),
        }
    }

    #[cfg(target_os = "windows")]
    fn lpcio(
        &self,
    ) -> mlua::Result<
        &std::sync::Arc<crate::infrastructure::drivers::transports::lpcio::LpcIoTransport>,
    > {
        match &self.io {
            PluginIo::Lpcio(bus) => Ok(bus),
            _ => Err(mlua::Error::RuntimeError(
                "this transport is not LPCIO; use lpcio typed operations only on an lpcio device"
                    .into(),
            )),
        }
    }
}

impl UserData for TransportApi {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        #[cfg(target_os = "linux")]
        methods.add_method("hwmon_list", |lua, this, ()| {
            lua.to_value(&this.hwmon()?.list())
        });
        #[cfg(target_os = "linux")]
        methods.add_method(
            "hwmon_read",
            |_, this, (key, attribute): (String, String)| {
                this.hwmon()?.read(&key, &attribute).map_err(to_lua_err)
            },
        );
        #[cfg(target_os = "linux")]
        methods.add_method(
            "hwmon_write",
            |_, this, (key, attribute, value): (String, String, String)| {
                this.hwmon()?
                    .write(&key, &attribute, &value)
                    .map_err(to_lua_err)
            },
        );

        // ── Stream (HID) ─────────────────────────────────────────────────
        // The common shapes: `write` (bytes → unit), `read`/`read_nonblocking`
        // (size → string), and `write_then_read`/`feature_exchange`
        // (bytes+size → string). Each drives the async transport via `block_on`.
        macro_rules! stream_method {
            (bytes_unit $accessor:ident, $name:literal, $m:ident) => {
                methods.add_method($name, |_, this, data: Value| {
                    let bytes = bytes_from(&data)?;
                    this.handle
                        .block_on(this.$accessor()?.$m(&bytes))
                        .map_err(to_lua_err)
                });
            };
            (size_str $accessor:ident, $name:literal, $m:ident) => {
                methods.add_method($name, |lua, this, size: usize| {
                    check_alloc(size)?;
                    let data = this
                        .handle
                        .block_on(this.$accessor()?.$m(size))
                        .map_err(to_lua_err)?;
                    lua.create_string(&data)
                });
            };
            (bytes_size_str $accessor:ident, $name:literal, $m:ident) => {
                methods.add_method($name, |lua, this, (data, size): (Value, usize)| {
                    check_alloc(size)?;
                    let bytes = bytes_from(&data)?;
                    let reply = this
                        .handle
                        .block_on(this.$accessor()?.$m(&bytes, size))
                        .map_err(to_lua_err)?;
                    lua.create_string(&reply)
                });
            };
        }
        stream_method!(bytes_unit stream, "write", write);
        stream_method!(size_str stream, "read", read);
        stream_method!(size_str hid, "read_nonblocking", read_nonblocking);
        stream_method!(size_str hid, "read_any", read_any);
        stream_method!(bytes_unit hid, "defer_event", defer_event);
        stream_method!(bytes_size_str stream, "write_then_read", write_then_read);
        stream_method!(bytes_size_str hid, "feature_exchange", feature_exchange);
        stream_method!(bytes_unit hid, "send_feature_report", send_feature_report);
        stream_method!(bytes_unit hid, "write_companion", write_companion);
        stream_method!(size_str hid, "read_companion", read_companion);
        stream_method!(bytes_size_str hid, "write_then_read_companion", write_then_read_companion);

        methods.add_method(
            "get_feature_report",
            |lua, this, (report_id, size): (u8, usize)| {
                check_alloc(size)?;
                let data = this
                    .handle
                    .block_on(this.hid()?.get_feature_report(report_id, size))
                    .map_err(to_lua_err)?;
                lua.create_string(&data)
            },
        );
        methods.add_method(
            "get_input_report",
            |lua, this, (report_id, size): (u8, usize)| {
                check_alloc(size)?;
                let data = this
                    .handle
                    .block_on(this.hid()?.get_input_report(report_id, size))
                    .map_err(to_lua_err)?;
                lua.create_string(&data)
            },
        );

        methods.add_method("has_companion", |_, this, ()| {
            Ok(this.hid()?.has_companion())
        });

        // ── Serial line control ──────────────────────────────────────────
        methods.add_method("set_dtr", |_, this, level: bool| {
            this.serial()?.set_dtr(level).map_err(to_lua_err)
        });
        methods.add_method("set_rts", |_, this, level: bool| {
            this.serial()?.set_rts(level).map_err(to_lua_err)
        });
        methods.add_method("send_break", |_, this, duration_ms: u64| {
            this.serial()?.send_break(duration_ms).map_err(to_lua_err)
        });
        methods.add_method("flush_input", |_, this, ()| {
            this.serial()?.flush_input().map_err(to_lua_err)
        });

        methods.add_method("write_many", |_, this, packets: Vec<Value>| {
            let owned: Vec<Vec<u8>> = packets
                .iter()
                .map(bytes_from)
                .collect::<mlua::Result<_>>()?;
            this.handle
                .block_on(this.stream()?.write_many(&owned))
                .map_err(to_lua_err)
        });

        methods.add_method("write_many_companion", |_, this, packets: Vec<Value>| {
            let owned: Vec<Vec<u8>> = packets
                .iter()
                .map(bytes_from)
                .collect::<mlua::Result<_>>()?;
            this.handle
                .block_on(this.hid()?.write_many_companion(&owned))
                .map_err(to_lua_err)
        });

        methods.add_method(
            "usb_write",
            |_, this, (endpoint, data, timeout_ms, device_id): (u8, Value, u64, Option<String>)| {
                let bytes = bytes_from(&data)?;
                this.usb()?
                    .write(device_id.as_deref(), endpoint, &bytes, timeout_ms)
                    .map_err(to_lua_err)
            },
        );
        methods.add_method(
            "usb_read",
            |lua, this, (endpoint, length, timeout_ms, device_id): (u8, usize, u64, Option<String>)| {
                check_alloc(length)?;
                let bytes = this.usb()?.read(device_id.as_deref(), endpoint, length, timeout_ms).map_err(to_lua_err)?;
                lua.create_string(&bytes)
            },
        );
        methods.add_method(
            "usb_control",
            |lua,
             this,
             (request_type, request, value, index, data, read_length, timeout_ms, device_id): (
                u8,
                u8,
                u16,
                u16,
                Value,
                usize,
                u64,
                Option<String>,
            )| {
                check_alloc(read_length)?;
                let bytes = bytes_from(&data)?;
                match this
                    .usb()?
                    .control(
                        device_id.as_deref(),
                        request_type,
                        request,
                        value,
                        index,
                        &bytes,
                        read_length,
                        timeout_ms,
                    )
                    .map_err(to_lua_err)?
                {
                    UsbControlResult::Written(n) => Ok(Value::Integer(n as mlua::Integer)),
                    UsbControlResult::Read(bytes) => Ok(Value::String(lua.create_string(&bytes)?)),
                }
            },
        );

        methods.add_method(
            "run",
            |lua, this, (executable, args): (String, Vec<String>)| {
                let result = this
                    .command()?
                    .run(&executable, &args)
                    .map_err(to_lua_err)?;
                command_result_table(lua, &result)
            },
        );

        // ── Typed PawnIO services (Windows) ──────────────────────────────
        // These are deliberately separate Lua objects rather than a raw port
        // handle.  A package gets only the operations its manifest authority
        // advertises, and every LPC write remains metered by LpcIoBus.
        #[cfg(target_os = "windows")]
        methods.add_method("amd_smn_read", |_, this, offset: u32| {
            this.amd_smn()?.read_smn(offset).map_err(to_lua_err)
        });

        #[cfg(target_os = "windows")]
        methods.add_method("lpcio_select_slot", |_, this, slot: u8| {
            this.lpcio()?.select_slot(slot).map_err(to_lua_err)
        });
        #[cfg(target_os = "windows")]
        methods.add_method("lpcio_find_bars", |_, this, ()| {
            this.lpcio()?.find_bars().map_err(to_lua_err)
        });
        // Reopen the Nuvoton HWM I/O window through a single trusted operation.
        // Packages may request this at poll boundaries, but cannot enter
        // extended-function mode or touch Super-I/O config registers directly.
        // Modern NCT679x firmware can reassert CR 0x28 bit 4 after discovery.
        #[cfg(target_os = "windows")]
        methods.add_method(
            "lpcio_prepare_hwm",
            |_, this, (slot, unlock): (u8, bool)| {
                this.lpcio()?.prepare_hwm(slot, unlock).map_err(to_lua_err)
            },
        );
        #[cfg(target_os = "windows")]
        methods.add_method("lpcio_read_port", |_, this, port: u16| {
            this.lpcio()?.read_port(port).map_err(to_lua_err)
        });
        // HWM index/data access is a stateful five-write transaction. Keep it
        // under one bus mutex so sensor and fan child workers cannot interleave
        // their register selectors and manufacture random readings.
        #[cfg(target_os = "windows")]
        methods.add_method("lpcio_hwm_read", |_, this, (base, register): (u16, u16)| {
            this.lpcio()?.hwm_read(base, register).map_err(to_lua_err)
        });
        #[cfg(target_os = "windows")]
        methods.add_method(
            "lpcio_hwm_write",
            |_, this, (base, register, value): (u16, u16, u8)| {
                this.lpcio()?
                    .hwm_write(base, register, value)
                    .map_err(to_lua_err)
            },
        );
        #[cfg(target_os = "windows")]
        methods.add_method("lpcio_write_port", |_, this, (port, value): (u16, u8)| {
            this.lpcio()?.write_port(port, value).map_err(to_lua_err)
        });
        #[cfg(target_os = "windows")]
        methods.add_method("lpcio_superio_inb", |_, this, register: u8| {
            this.lpcio()?.superio_inb(register).map_err(to_lua_err)
        });
        #[cfg(target_os = "windows")]
        methods.add_method(
            "lpcio_superio_outb",
            |_, this, (register, value): (u8, u8)| {
                this.lpcio()?
                    .superio_outb(register, value)
                    .map_err(to_lua_err)
            },
        );

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

impl ScopedOps<'_> {
    fn write_block_data(&mut self, addr: u8, cmd: u8, data: &[u8]) -> mlua::Result<bool> {
        self.scope.check(addr).map_err(to_lua_err)?;
        if data.len() > halod_hwaccess::smbus::SMBUS_BLOCK_MAX {
            return Err(mlua::Error::RuntimeError(format!(
                "SMBus block write exceeds the {}-byte maximum",
                halod_hwaccess::smbus::SMBUS_BLOCK_MAX
            )));
        }
        Ok(self.ops.write_block_data(addr, cmd, data).is_ok())
    }
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
                let bytes = bytes_from(&data)?;
                this.write_block_data(addr, cmd, &bytes)
            },
        );
        methods.add_method("supports_block_write", |_, this, ()| {
            Ok(this.ops.supports_block_write())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct BlockWriteOps {
        writes: usize,
    }

    impl SmBusSyncOps for BlockWriteOps {
        fn read_byte(&mut self, _addr: u8) -> anyhow::Result<u8> {
            unreachable!()
        }

        fn read_byte_data(&mut self, _addr: u8, _cmd: u8) -> anyhow::Result<u8> {
            unreachable!()
        }

        fn write_quick(&mut self, _addr: u8) -> anyhow::Result<bool> {
            unreachable!()
        }

        fn write_byte_data(&mut self, _addr: u8, _cmd: u8, _val: u8) -> anyhow::Result<()> {
            unreachable!()
        }

        fn write_word_data(&mut self, _addr: u8, _cmd: u8, _val: u16) -> anyhow::Result<()> {
            unreachable!()
        }

        fn write_block_data(&mut self, _addr: u8, _cmd: u8, _data: &[u8]) -> anyhow::Result<()> {
            self.writes += 1;
            Ok(())
        }
    }

    #[test]
    fn hid_report_methods_round_trip_through_lua() {
        use crate::infrastructure::drivers::transports::mock::test_transport::MockTransport;
        use std::sync::Arc;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let transport = Arc::new(MockTransport::new(vec![
            vec![0x00, 0xAA, 0xBB], // get_feature_report reply
            vec![0x00, 0xCC],       // get_input_report reply
        ]));
        let io = PluginIo::Stream {
            transport: transport.clone() as Arc<dyn Transport>,
            usb: None,
        };
        let api = TransportApi::new(io, rt.handle().clone());
        let lua = mlua::Lua::new();
        lua.globals().set("t", api).unwrap();
        let out: mlua::Table = lua
            .load(
                r#"
                t:send_feature_report(string.char(0x01, 0x02))
                return { t:get_feature_report(0x00, 2), t:get_input_report(0x00, 1) }
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(
            out.get::<mlua::LuaString>(1).unwrap().as_bytes(),
            &[0x00, 0xAA, 0xBB]
        );
        assert_eq!(
            out.get::<mlua::LuaString>(2).unwrap().as_bytes(),
            &[0x00, 0xCC]
        );
        let written = rt.block_on(async { transport.written.lock().await.clone() });
        assert_eq!(written, vec![vec![0x01u8, 0x02]]);
    }

    #[test]
    fn scoped_smbus_block_write_accepts_the_32_byte_maximum() {
        let scope = AddrScope::single(0x50);
        let mut ops = BlockWriteOps::default();
        let mut scoped = ScopedOps {
            ops: &mut ops,
            scope: &scope,
        };

        assert!(scoped.write_block_data(0x50, 0, &[0; 32]).unwrap());
        assert_eq!(ops.writes, 1);
    }

    #[test]
    fn scoped_smbus_block_write_rejects_more_than_32_bytes_before_transport() {
        let scope = AddrScope::single(0x50);
        let mut ops = BlockWriteOps::default();
        let mut scoped = ScopedOps {
            ops: &mut ops,
            scope: &scope,
        };

        let error = scoped.write_block_data(0x50, 0, &[0; 33]).unwrap_err();
        assert!(error.to_string().contains("32-byte maximum"));
        assert_eq!(ops.writes, 0);
    }
}
