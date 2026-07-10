// SPDX-License-Identifier: GPL-3.0-or-later
//! The `transport` object a plugin script uses to move bytes. Bytes cross the
//! boundary as Lua strings. The script sees synchronous calls; each drives the
//! daemon's async transport via a captured runtime handle (`block_on`), which is
//! legal because the worker runs on its own `std::thread`, not a runtime worker.

use std::sync::Arc;

use mlua::{UserData, UserDataMethods};
use tokio::runtime::Handle;

use crate::drivers::transports::Transport;

/// Lua userdata wrapping one transport. Rate limiting is inherited: this holds
/// the real (metered) transport, so a script cannot outrun the hardware.
pub struct TransportApi {
    transport: Arc<dyn Transport>,
    handle: Handle,
}

impl TransportApi {
    pub fn new(transport: Arc<dyn Transport>, handle: Handle) -> Self {
        Self { transport, handle }
    }
}

fn to_lua_err(e: anyhow::Error) -> mlua::Error {
    mlua::Error::RuntimeError(format!("{e:#}"))
}

impl UserData for TransportApi {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("write", |_, this, data: mlua::String| {
            let bytes = data.as_bytes().to_vec();
            this.handle
                .block_on(this.transport.write(&bytes))
                .map_err(to_lua_err)
        });

        methods.add_method("read", |lua, this, size: usize| {
            let data = this
                .handle
                .block_on(this.transport.read(size))
                .map_err(to_lua_err)?;
            lua.create_string(&data)
        });

        methods.add_method(
            "write_then_read",
            |lua, this, (data, size): (mlua::String, usize)| {
                let bytes = data.as_bytes().to_vec();
                let reply = this
                    .handle
                    .block_on(this.transport.write_then_read(&bytes, size))
                    .map_err(to_lua_err)?;
                lua.create_string(&reply)
            },
        );

        methods.add_method(
            "feature_exchange",
            |lua, this, (data, size): (mlua::String, usize)| {
                let bytes = data.as_bytes().to_vec();
                let reply = this
                    .handle
                    .block_on(this.transport.feature_exchange(&bytes, size))
                    .map_err(to_lua_err)?;
                lua.create_string(&reply)
            },
        );

        methods.add_method("write_many", |_, this, packets: Vec<mlua::String>| {
            let owned: Vec<Vec<u8>> = packets.iter().map(|p| p.as_bytes().to_vec()).collect();
            this.handle
                .block_on(this.transport.write_many(&owned))
                .map_err(to_lua_err)
        });
    }
}
