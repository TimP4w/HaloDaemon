// SPDX-License-Identifier: GPL-3.0-or-later
//! The `halod.udp` capability API. A plugin that declares a scoped `udp`
//! transport and holds the `network` permission gets `halod.udp:send{…}` and
//! `halod.udp:receive{…}` against the single configured destination. Like
//! `halod.http` this is a capability global, not a per-device `transport:`
//! userdata — the plugin has no free-roaming socket.

use std::time::Duration;

use mlua::{Lua, Table, Value};

use super::bytebuf::check_alloc;
use super::ffi::{bytes_from, to_lua_err};
use crate::services::udp::UdpRuntime;

pub fn register(lua: &Lua, runtime: UdpRuntime) -> mlua::Result<()> {
    let halod: Table = lua.globals().get("halod")?;
    let udp = lua.create_table()?;

    let send_runtime = runtime.clone();
    udp.set(
        "send",
        // `halod.udp:send{ bytes = ... }` passes the udp table itself as `_self`.
        lua.create_function(move |_, (_self, args): (Table, Table)| {
            let data = bytes_from(&args.get::<Value>("bytes")?)?;
            let sent = send_runtime.send(&data).map_err(to_lua_err)?;
            Ok(sent)
        })?,
    )?;

    udp.set(
        "receive",
        lua.create_function(move |lua, (_self, args): (Table, Table)| {
            let timeout = args
                .get::<Option<u64>>("timeout_ms")?
                .filter(|ms| *ms > 0)
                .map(Duration::from_millis);
            // Bound the buffer we're about to allocate on receipt.
            check_alloc(0)?;
            match runtime.receive(timeout).map_err(to_lua_err)? {
                Some(datagram) => Ok(Value::String(lua.create_string(&datagram)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    halod.set("udp", udp)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::udp::UdpBackend;
    use anyhow::Result;
    use std::sync::{Arc, Mutex};

    struct StubBackend {
        sent: Mutex<Vec<Vec<u8>>>,
        inbound: Mutex<std::collections::VecDeque<Vec<u8>>>,
    }
    impl UdpBackend for StubBackend {
        fn send(&self, data: &[u8]) -> Result<usize> {
            self.sent.lock().unwrap().push(data.to_vec());
            Ok(data.len())
        }
        fn receive(&self, _t: Duration, _max: usize) -> Result<Option<Vec<u8>>> {
            Ok(self.inbound.lock().unwrap().pop_front())
        }
    }

    fn lua_with_udp(inbound: Vec<Vec<u8>>) -> (Lua, Arc<StubBackend>) {
        let backend = Arc::new(StubBackend {
            sent: Mutex::new(Vec::new()),
            inbound: Mutex::new(inbound.into()),
        });
        let runtime = UdpRuntime::new(backend.clone(), 1024, Duration::from_millis(50));
        let lua = Lua::new();
        lua.globals()
            .set("halod", lua.create_table().unwrap())
            .unwrap();
        register(&lua, runtime).unwrap();
        (lua, backend)
    }

    #[test]
    fn send_reaches_the_backend_and_receive_returns_the_datagram() {
        let (lua, backend) = lua_with_udp(vec![vec![9, 8, 7]]);
        let got: mlua::String = lua
            .load(
                r#"halod.udp:send{ bytes = string.char(1, 2, 3) }
                   return halod.udp:receive{ timeout_ms = 10 }"#,
            )
            .eval()
            .unwrap();
        assert_eq!(got.as_bytes(), &[9, 8, 7]);
        assert_eq!(*backend.sent.lock().unwrap(), vec![vec![1u8, 2, 3]]);
    }

    #[test]
    fn receive_yields_nil_when_no_datagram_is_available() {
        let (lua, _) = lua_with_udp(vec![]);
        let nil: Value = lua
            .load("return halod.udp:receive{ timeout_ms = 5 }")
            .eval()
            .unwrap();
        assert!(matches!(nil, Value::Nil));
    }
}
