// SPDX-License-Identifier: GPL-3.0-or-later
//! Small shared Lua<->Rust marshalling helpers, used by every FFI surface the
//! plugin host exposes (`transport_api`, `image_api`, `bytebuf`).

use mlua::Value;

use super::bytebuf::ByteBuf;

/// Accept either a Lua string or a `halod.buffer` as a byte payload.
pub(super) fn bytes_from(value: &Value) -> mlua::Result<Vec<u8>> {
    match value {
        Value::String(s) => Ok(s.as_bytes().to_vec()),
        Value::UserData(ud) => Ok(ud.borrow::<ByteBuf>()?.as_slice().to_vec()),
        other => Err(mlua::Error::RuntimeError(format!(
            "expected bytes (a string or halod.buffer), got {}",
            other.type_name()
        ))),
    }
}

/// Surface an `anyhow` error to Lua with its full context chain.
pub(super) fn to_lua_err(e: anyhow::Error) -> mlua::Error {
    mlua::Error::RuntimeError(format!("{e:#}"))
}
