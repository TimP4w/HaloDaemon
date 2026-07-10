// SPDX-License-Identifier: GPL-3.0-or-later
//! Restricts the Lua environment a plugin runs in. Plugins are trusted (they can
//! talk to their matched device) but sandboxed against filesystem/process/native
//! escape hatches. `string`/`table`/`math` (incl. Lua 5.4 bitwise ops and
//! `string.pack`) stay available — they're what protocol encoding needs.

use mlua::{Lua, Table};

use halod_shared::types::Permission;

use super::bytebuf;

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

/// Strip every escape hatch, then re-inject `halod`/`log`, then selectively
/// re-enable permission-gated globals the plugin was actually granted.
/// `granted` is the intersection the caller already computed (declared ∩
/// user-accepted) — this function only ever adds capability, never checks
/// consent itself.
pub fn apply(lua: &Lua, granted: &[Permission]) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in REMOVED {
        globals.set(*name, mlua::Value::Nil)?;
    }
    let logger = lua.create_function(|_, msg: mlua::String| {
        log::info!("[plugin] {}", msg.to_string_lossy());
        Ok(())
    })?;
    globals.set("log", logger)?;
    bytebuf::register(lua)?;
    super::image_api::register(lua)?;

    if granted.contains(&Permission::Os) {
        reinject_clock(lua)?;
    }
    Ok(())
}

/// `Permission::Os` only ever re-enables the read-only wall clock — never
/// filesystem/process access, which `os` also carries and stays stripped.
fn reinject_clock(lua: &Lua) -> mlua::Result<()> {
    let os: Table = lua.create_table()?;
    let time = lua.create_function(|_, ()| {
        Ok(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0))
    })?;
    let clock = lua.create_function(|_, ()| {
        Ok(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0))
    })?;
    os.set("time", time)?;
    os.set("clock", clock)?;
    lua.globals().set("os", os)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_escape_hatches_and_keeps_string_lib() {
        let lua = Lua::new();
        apply(&lua, &[]).unwrap();
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
        apply(&lua, &[]).unwrap();
        lua.load(r#"log("hello from a test plugin")"#)
            .exec()
            .unwrap();
    }

    #[test]
    fn ungranted_os_permission_leaves_os_nil() {
        let lua = Lua::new();
        apply(&lua, &[]).unwrap();
        assert!(lua
            .load("return os")
            .eval::<mlua::Value>()
            .unwrap()
            .is_nil());
    }

    #[test]
    fn granted_os_permission_reinjects_only_the_clock() {
        let lua = Lua::new();
        apply(&lua, &[Permission::Os]).unwrap();
        let now: u64 = lua.load("return os.time()").eval().unwrap();
        assert!(now > 0);
        // Filesystem/process access must stay stripped even with Os granted.
        assert!(lua
            .load("return os.remove")
            .eval::<mlua::Value>()
            .unwrap()
            .is_nil());
    }
}
