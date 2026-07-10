// SPDX-License-Identifier: GPL-3.0-or-later
//! Restricts the Lua environment a plugin runs in. Plugins are trusted (they can
//! talk to their matched device) but sandboxed against filesystem/process/native
//! escape hatches. `string`/`table`/`math` (incl. Lua 5.4 bitwise ops and
//! `string.pack`) stay available — they're what protocol encoding needs.

use mlua::Lua;

/// Globals removed before a plugin script runs.
const REMOVED: &[&str] = &[
    "os",
    "io",
    "package",
    "require",
    "dofile",
    "loadfile",
    "load",
    "debug",
    "collectgarbage",
];

pub fn apply(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in REMOVED {
        globals.set(*name, mlua::Value::Nil)?;
    }
    let logger = lua.create_function(|_, msg: mlua::String| {
        log::info!("[plugin] {}", msg.to_string_lossy());
        Ok(())
    })?;
    globals.set("log", logger)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_escape_hatches_and_keeps_string_lib() {
        let lua = Lua::new();
        apply(&lua).unwrap();
        assert!(lua
            .load("return os")
            .eval::<mlua::Value>()
            .unwrap()
            .is_nil());
        assert!(lua
            .load("return io")
            .eval::<mlua::Value>()
            .unwrap()
            .is_nil());
        // Encoding primitives survive.
        let out: String = lua.load(r#"return string.char(65, 66)"#).eval().unwrap();
        assert_eq!(out, "AB");
    }

    #[test]
    fn log_is_available() {
        let lua = Lua::new();
        apply(&lua).unwrap();
        lua.load(r#"log("hello from a test plugin")"#)
            .exec()
            .unwrap();
    }
}
