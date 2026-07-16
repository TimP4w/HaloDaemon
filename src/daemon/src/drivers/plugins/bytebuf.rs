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

/// Upper bound on any single plugin-driven native allocation (`halod.buffer`,
/// `transport:usb_read`/`usb_control`, image-codec output). `set_memory_limit` only tracks
/// Lua's own allocator, so an unbounded `vec![0u8; n]` from a Lua-supplied length
/// would OOM/abort the (root) daemon; this caps every such allocation. Shares the
/// plugin-VM heap ceiling so the two never drift — see [`super::PLUGIN_VM_MEMORY_BYTES`].
pub const MAX_ALLOC_BYTES: usize = super::PLUGIN_VM_MEMORY_BYTES;

/// Reject a plugin-supplied length past [`MAX_ALLOC_BYTES`] with a Lua error
/// instead of letting a downstream `vec![0u8; n]` OOM/abort the process.
pub fn check_alloc(n: usize) -> mlua::Result<()> {
    if n > MAX_ALLOC_BYTES {
        return Err(mlua::Error::RuntimeError(format!(
            "requested allocation of {n} bytes exceeds the {MAX_ALLOC_BYTES}-byte limit"
        )));
    }
    Ok(())
}

/// Fallibly allocate a zeroed buffer of `n` bytes, capped by [`check_alloc`].
pub fn alloc_zeroed(n: usize) -> mlua::Result<Vec<u8>> {
    check_alloc(n)?;
    Ok(vec![0u8; n])
}

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

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.data
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
        mlua::Value::Integer(n) if n >= 0 => {
            let n = usize::try_from(n).map_err(|_| {
                mlua::Error::RuntimeError("halod.buffer length does not fit this platform".into())
            })?;
            Ok(ByteBuf::from_bytes(alloc_zeroed(n)?))
        }
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

        // One get/set pair per (width, endianness). The getter slices `$w` bytes
        // (bounds already checked) into the fixed array `from_*_bytes` wants.
        macro_rules! int_accessors {
            ($ty:ty, $w:expr, $get:literal, $from:ident, $set:literal, $to:ident) => {
                methods.add_method($get, |_, this, i: usize| {
                    this.check(i, $w)?;
                    // check() above guarantees the slice is exactly $w bytes.
                    Ok(<$ty>::$from(
                        this.data[i..i + $w]
                            .try_into()
                            .expect("checked slice is exactly the accessor width"),
                    ))
                });
                methods.add_method_mut($set, |_, this, (i, v): (usize, $ty)| {
                    this.check(i, $w)?;
                    this.data[i..i + $w].copy_from_slice(&v.$to());
                    Ok(())
                });
            };
        }
        int_accessors!(
            u16,
            2,
            "get_u16_le",
            from_le_bytes,
            "set_u16_le",
            to_le_bytes
        );
        int_accessors!(
            u16,
            2,
            "get_u16_be",
            from_be_bytes,
            "set_u16_be",
            to_be_bytes
        );
        int_accessors!(
            u32,
            4,
            "get_u32_le",
            from_le_bytes,
            "set_u32_le",
            to_le_bytes
        );
        int_accessors!(
            u32,
            4,
            "get_u32_be",
            from_be_bytes,
            "set_u32_be",
            to_be_bytes
        );

        methods.add_method("slice", |_, this, (start, len): (usize, usize)| {
            this.check(start, len)?;
            Ok(ByteBuf::from_bytes(this.data[start..start + len].to_vec()))
        });

        // set_bytes(start, str_or_buffer) copies a whole run in one call —
        // the difference between one host round-trip and one per byte, which
        // matters for a per-pixel render loop (e.g. build a row as a Lua
        // string via string.char/table.concat, then write it in one call).
        methods.add_method_mut(
            "set_bytes",
            |_, this, (start, src): (usize, mlua::Value)| {
                let bytes: Vec<u8> = match &src {
                    mlua::Value::String(s) => s.as_bytes().to_vec(),
                    mlua::Value::UserData(ud) => ud.borrow::<ByteBuf>()?.data.clone(),
                    _ => {
                        return Err(mlua::Error::RuntimeError(
                            "set_bytes expects a string or a halod.buffer".into(),
                        ))
                    }
                };
                this.check(start, bytes.len())?;
                this.data[start..start + bytes.len()].copy_from_slice(&bytes);
                Ok(())
            },
        );
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
    fn buffer_rejects_an_allocation_past_the_cap() {
        let lua = lua();
        let err = lua
            .load("return halod.buffer(1000000000000)")
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
        // A buffer right at the cap is still allowed.
        assert!(alloc_zeroed(MAX_ALLOC_BYTES).is_ok());
        assert!(alloc_zeroed(MAX_ALLOC_BYTES + 1).is_err());
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
        assert_eq!(out.as_bytes(), &[0x07, 0x34, 0x12, 0xEE]);
    }

    #[test]
    fn set_bytes_writes_a_string_in_one_call() {
        let lua = lua();
        let out: mlua::String = lua
            .load(
                r#"
                local b = halod.buffer(6)
                b:set_u8(0, 0xFF)
                b:set_bytes(1, string.char(1, 2, 3, 4))
                b:set_u8(5, 0xEE)
                return b:tostring()
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(out.as_bytes(), &[0xFF, 1, 2, 3, 4, 0xEE]);
    }

    #[test]
    fn set_bytes_accepts_another_buffer_as_the_source() {
        let lua = lua();
        let out: mlua::String = lua
            .load(
                r#"
                local src = halod.buffer(string.char(9, 8, 7))
                local dst = halod.buffer(5)
                dst:set_bytes(1, src)
                return dst:tostring()
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(out.as_bytes(), &[0, 9, 8, 7, 0]);
    }

    #[test]
    fn set_bytes_out_of_range_errors_without_partial_write() {
        let lua = lua();
        let out: mlua::String = lua
            .load(
                r#"
                local b = halod.buffer(4)
                local ok, err = pcall(function() b:set_bytes(2, string.char(1, 2, 3)) end)
                assert(not ok and tostring(err):find("out of range"), "expected out-of-range error")
                return b:tostring()
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(
            out.as_bytes(),
            &[0, 0, 0, 0],
            "a rejected set_bytes must leave the buffer untouched"
        );
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
        assert_eq!(out.as_bytes(), &[0x02, 0x03]);
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

        #[test]
        fn u16_be_round_trip(value: u16, pad in 0usize..8) {
            let script = format!(
                "local b = halod.buffer({}); b:set_u16_be(0, {value}); return b:get_u16_be(0)",
                2 + pad
            );
            let got: u16 = lua().load(&script).eval().unwrap();
            prop_assert_eq!(got, value);
        }

        #[test]
        fn u32_le_round_trip(value: u32, pad in 0usize..8) {
            let script = format!(
                "local b = halod.buffer({}); b:set_u32_le(0, {value}); return b:get_u32_le(0)",
                4 + pad
            );
            let got: u32 = lua().load(&script).eval().unwrap();
            prop_assert_eq!(got, value);
        }

        // A read at any offset succeeds iff it fits, and never panics — the
        // regression guard for the width-conversion `expect` in `int_accessors!`.
        #[test]
        fn read_at_any_offset_matches_bounds(len in 0usize..16, i in 0usize..64) {
            for (getter, w) in [("get_u16_le", 2usize), ("get_u16_be", 2), ("get_u32_le", 4), ("get_u32_be", 4)] {
                let script = format!("return halod.buffer({len}):{getter}({i})");
                let got = lua().load(&script).eval::<u32>();
                prop_assert_eq!(got.is_ok(), i + w <= len, "{} at {} in {}", getter, i, len);
            }
        }
    }
}
