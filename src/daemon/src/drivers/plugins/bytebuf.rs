// SPDX-License-Identifier: GPL-3.0-or-later
//! A mutable, fixed-length, bounds-checked byte buffer for plugin scripts.
//!
//! Lua strings make protocol code hazardous: 1-based `:byte(i)` offsets,
//! immutable (patching one byte means rebuilding the string), and no length
//! checks (a short reply yields `nil`, not an error at the read site). `ByteBuf`
//! gives 0-based, bounds-checked, little/big-endian accessors instead. Transport
//! methods accept either a Lua string or a `ByteBuf`, so it layers on without
//! breaking the string API.

use mlua::{Lua, UserData, UserDataMethods};

/// Exposed to Lua as `halod.buffer(n)` / `halod.buffer(str)`.
#[derive(Clone)]
pub struct ByteBuf {
    data: Vec<u8>,
}

impl ByteBuf {
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Self { data }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    fn check(&self, index: usize, width: usize) -> mlua::Result<()> {
        let end = index.checked_add(width);
        match end {
            Some(end) if end <= self.data.len() => Ok(()),
            _ => Err(mlua::Error::RuntimeError(format!(
                "buffer index out of range: [{index}..{}) in a {}-byte buffer",
                index.saturating_add(width),
                self.data.len()
            ))),
        }
    }
}

/// Register the `halod` table (with `buffer`) into the Lua globals.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    let halod = lua.create_table()?;
    let buffer = lua.create_function(|_, arg: mlua::Value| match arg {
        mlua::Value::Integer(n) if n >= 0 => Ok(ByteBuf::from_bytes(vec![0u8; n as usize])),
        mlua::Value::String(s) => Ok(ByteBuf::from_bytes(s.as_bytes().to_vec())),
        _ => Err(mlua::Error::RuntimeError(
            "halod.buffer expects a non-negative length or a string".into(),
        )),
    })?;
    halod.set("buffer", buffer)?;
    lua.globals().set("halod", halod)?;
    Ok(())
}

impl UserData for ByteBuf {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::Len, |_, this, ()| Ok(this.data.len()));
        methods.add_meta_method(mlua::MetaMethod::ToString, |lua, this, ()| {
            lua.create_string(&this.data)
        });

        methods.add_method("len", |_, this, ()| Ok(this.data.len()));
        methods.add_method("tostring", |lua, this, ()| lua.create_string(&this.data));

        methods.add_method("get_u8", |_, this, i: usize| {
            this.check(i, 1)?;
            Ok(this.data[i])
        });
        methods.add_method_mut("set_u8", |_, this, (i, v): (usize, u8)| {
            this.check(i, 1)?;
            this.data[i] = v;
            Ok(())
        });

        methods.add_method("get_u16_le", |_, this, i: usize| {
            this.check(i, 2)?;
            Ok(u16::from_le_bytes([this.data[i], this.data[i + 1]]))
        });
        methods.add_method("get_u16_be", |_, this, i: usize| {
            this.check(i, 2)?;
            Ok(u16::from_be_bytes([this.data[i], this.data[i + 1]]))
        });
        methods.add_method_mut("set_u16_le", |_, this, (i, v): (usize, u16)| {
            this.check(i, 2)?;
            this.data[i..i + 2].copy_from_slice(&v.to_le_bytes());
            Ok(())
        });
        methods.add_method_mut("set_u16_be", |_, this, (i, v): (usize, u16)| {
            this.check(i, 2)?;
            this.data[i..i + 2].copy_from_slice(&v.to_be_bytes());
            Ok(())
        });

        methods.add_method("get_u32_le", |_, this, i: usize| {
            this.check(i, 4)?;
            Ok(u32::from_le_bytes([
                this.data[i],
                this.data[i + 1],
                this.data[i + 2],
                this.data[i + 3],
            ]))
        });
        methods.add_method("get_u32_be", |_, this, i: usize| {
            this.check(i, 4)?;
            Ok(u32::from_be_bytes([
                this.data[i],
                this.data[i + 1],
                this.data[i + 2],
                this.data[i + 3],
            ]))
        });
        methods.add_method_mut("set_u32_le", |_, this, (i, v): (usize, u32)| {
            this.check(i, 4)?;
            this.data[i..i + 4].copy_from_slice(&v.to_le_bytes());
            Ok(())
        });
        methods.add_method_mut("set_u32_be", |_, this, (i, v): (usize, u32)| {
            this.check(i, 4)?;
            this.data[i..i + 4].copy_from_slice(&v.to_be_bytes());
            Ok(())
        });

        // slice(start, len) -> a new ByteBuf copy.
        methods.add_method("slice", |_, this, (start, len): (usize, usize)| {
            this.check(start, len)?;
            Ok(ByteBuf::from_bytes(this.data[start..start + len].to_vec()))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua() -> Lua {
        let lua = Lua::new();
        register(&lua).unwrap();
        lua
    }

    #[test]
    fn build_and_read_back_le_and_be() {
        let lua = lua();
        let out: mlua::String = lua
            .load(
                r#"
                local b = halod.buffer(4)
                b:set_u8(0, 0x07)
                b:set_u16_le(1, 0x1234)
                b:set_u8(3, 0xEE)
                assert(b:get_u8(0) == 0x07)
                assert(b:get_u16_le(1) == 0x1234)
                assert(b:get_u16_be(1) == 0x3412)
                assert(#b == 4)
                return b:tostring()
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(out.as_bytes().to_vec(), vec![0x07, 0x34, 0x12, 0xEE]);
    }

    #[test]
    fn wrap_string_and_slice() {
        let lua = lua();
        let out: mlua::String = lua
            .load(
                r#"
                local b = halod.buffer("\x01\x02\x03\x04")
                return b:slice(1, 2):tostring()
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(out.as_bytes().to_vec(), vec![0x02, 0x03]);
    }

    #[test]
    fn out_of_range_read_errors_at_the_read_site() {
        let err = lua()
            .load(r#"return halod.buffer(2):get_u16_le(1)"#)
            .eval::<u16>()
            .unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[test]
    fn out_of_range_write_errors() {
        let err = lua()
            .load(r#"halod.buffer(2):set_u32_le(0, 1)"#)
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[test]
    fn negative_length_rejected() {
        assert!(lua().load(r#"return halod.buffer(-1)"#).exec().is_err());
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn u16_le_round_trip(value: u16, pad in 0usize..8) {
            let script = format!(
                "local b = halod.buffer({}); b:set_u16_le(0, {value}); return b:get_u16_le(0)",
                2 + pad
            );
            let got: u16 = lua().load(&script).eval().unwrap();
            prop_assert_eq!(got, value);
        }

        #[test]
        fn u32_be_round_trip(value: u32) {
            let script = format!(
                "local b = halod.buffer(4); b:set_u32_be(0, {value}); return b:get_u32_be(0)"
            );
            let got: u32 = lua().load(&script).eval().unwrap();
            prop_assert_eq!(got, value);
        }
    }
}
