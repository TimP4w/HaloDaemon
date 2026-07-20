// SPDX-License-Identifier: GPL-3.0-or-later
//! Image/codec helpers exposed to plugin scripts under the `halod` table. LCD
//! panels want pixel data in formats (Q565, BGR888, resized RGBA/GIF) that a Lua
//! script can't produce performantly — these run the CPU-heavy work in Rust and
//! hand back a `halod.buffer`. Bytes in may be a Lua string or a `halod.buffer`.

use mlua::{Lua, Value};

use super::bytebuf::{check_alloc, ByteBuf};
use super::ffi::{bytes_from as bytes_of, to_lua_err};
use crate::util::image;

/// Reject plugin-supplied dimensions whose RGBA buffer (`w*h*4`) would exceed the
/// native-allocation cap, so a decode/resize can't be steered into an OOM.
fn check_dims(w: u32, h: u32) -> mlua::Result<()> {
    let bytes = (w as usize)
        .checked_mul(h as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| mlua::Error::RuntimeError(format!("image dimensions {w}x{h} overflow")))?;
    check_alloc(bytes)
}

/// Add the image codecs to the (already-created) `halod` global table.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    let halod: mlua::Table = lua.globals().get("halod")?;

    halod.set(
        "rgba_to_q565",
        lua.create_function(|_, (rgba, w, h): (Value, u32, u32)| {
            check_dims(w, h)?;
            let out = image::rgba_to_q565(&bytes_of(&rgba)?, w, h).map_err(to_lua_err)?;
            Ok(ByteBuf::from_bytes(out))
        })?,
    )?;

    halod.set(
        "rgba_to_bgr888",
        lua.create_function(|_, rgba: Value| {
            let rgba = bytes_of(&rgba)?;
            let output_len = (rgba.len() / 4)
                .checked_mul(3)
                .ok_or_else(|| mlua::Error::RuntimeError("BGR888 output length overflow".into()))?;
            check_alloc(output_len)?;
            Ok(ByteBuf::from_bytes(image::rgba_to_bgr888(&rgba)))
        })?,
    )?;

    halod.set(
        "rgba_rotate_square",
        lua.create_function(|_, (rgba, size, deg): (Value, u32, u32)| {
            check_dims(size, size)?;
            Ok(ByteBuf::from_bytes(image::rotate_rgba_square(
                &bytes_of(&rgba)?,
                size,
                deg,
            )))
        })?,
    )?;

    halod.set(
        "image_decode",
        lua.create_function(|_, (bytes, w, h): (Value, u32, u32)| {
            check_dims(w, h)?;
            let out =
                image::decode_static_image_rgba(&bytes_of(&bytes)?, w, h).map_err(to_lua_err)?;
            Ok(ByteBuf::from_bytes(out))
        })?,
    )?;

    halod.set(
        "gif_resize",
        lua.create_function(|_, (bytes, w, h): (Value, u32, u32)| {
            check_dims(w, h)?;
            let out = image::resize_gif(&bytes_of(&bytes)?, w, h, |_| {}).map_err(to_lua_err)?;
            Ok(ByteBuf::from_bytes(out))
        })?,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use mlua::Lua;

    fn lua() -> Lua {
        let lua = Lua::new();
        super::super::bytebuf::register(&lua).unwrap();
        super::register(&lua).unwrap();
        lua
    }

    #[test]
    fn rgba_to_bgr888_from_lua_reorders() {
        let lua = lua();
        let out: mlua::LuaString = lua
            .load(
                r#"
                local rgba = halod.buffer("\x0A\x14\x1E\xFF")
                return halod.rgba_to_bgr888(rgba):tostring()
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(out.as_bytes().to_vec(), vec![0x1E, 0x14, 0x0A]);
    }

    #[test]
    fn rgba_to_q565_from_lua_produces_q565_file() {
        let lua = lua();
        let out: mlua::LuaString = lua
            .load(
                r#"
                local rgba = halod.buffer(4 * 4 * 4)
                return halod.rgba_to_q565(rgba, 4, 4):tostring()
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(&out.as_bytes()[0..4], b"q565");
    }

    #[test]
    fn rgba_to_q565_size_mismatch_errors_in_lua() {
        let lua = lua();
        let err = lua
            .load(r#"return halod.rgba_to_q565(halod.buffer(4), 2, 2)"#)
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("size mismatch"), "{err}");
    }
}
