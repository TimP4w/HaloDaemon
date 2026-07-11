// SPDX-License-Identifier: GPL-3.0-or-later
//! Restricts the Lua environment a plugin runs in. Plugins are trusted (they can
//! talk to their matched device) but sandboxed against filesystem/process/native
//! escape hatches. `string`/`table`/`math` (incl. Lua 5.4 bitwise ops and
//! `string.pack`) stay available — they're what protocol encoding needs.

use std::collections::HashMap;

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
/// consent itself. `config` is this plugin's own resolved config values (see
/// `plugins::resolved_config_for`) — never another plugin's, since the caller
/// builds one `config` map per VM.
pub fn apply(
    lua: &Lua,
    granted: &[Permission],
    config: &HashMap<String, String>,
) -> mlua::Result<()> {
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
    inject_config(lua, config)?;

    if granted.contains(&Permission::Os) {
        reinject_clock(lua)?;
    }
    Ok(())
}

/// Populate `halod.config` with this plugin's resolved values. Read-only in
/// spirit (nothing enforces it in Lua, but plugins have no reason to mutate
/// it and no callback ever reads it back from the host).
fn inject_config(lua: &Lua, config: &HashMap<String, String>) -> mlua::Result<()> {
    let halod: Table = lua.globals().get("halod")?;
    let table = lua.create_table()?;
    for (key, value) in config {
        table.set(key.as_str(), value.as_str())?;
    }
    halod.set("config", table)
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
    fn removes_every_escape_hatch_and_keeps_string_lib() {
        let lua = Lua::new();
        apply(&lua, &[], &HashMap::new()).unwrap();
        for name in REMOVED {
            let v: mlua::Value = lua.load(format!("return {name}")).eval().unwrap();
            assert!(v.is_nil(), "escape hatch '{name}' was not stripped");
        }
        // Encoding primitives survive.
        let out: String = lua.load(r#"return string.char(65, 66)"#).eval().unwrap();
        assert_eq!(out, "AB");
    }

    #[test]
    fn log_is_available() {
        let lua = Lua::new();
        apply(&lua, &[], &HashMap::new()).unwrap();
        lua.load(r#"log("hello from a test plugin")"#)
            .exec()
            .unwrap();
    }

    #[test]
    fn granted_os_permission_reinjects_only_the_clock() {
        let lua = Lua::new();
        apply(&lua, &[Permission::Os], &HashMap::new()).unwrap();
        let now: u64 = lua.load("return os.time()").eval().unwrap();
        assert!(now > 0);
        // Filesystem/process access must stay stripped even with Os granted.
        assert!(lua
            .load("return os.remove")
            .eval::<mlua::Value>()
            .unwrap()
            .is_nil());
    }

    #[test]
    fn halod_config_exposes_only_the_given_values() {
        let lua = Lua::new();
        let config = HashMap::from([("host".to_string(), "127.0.0.1".to_string())]);
        apply(&lua, &[], &config).unwrap();
        let host: String = lua.load("return halod.config.host").eval().unwrap();
        assert_eq!(host, "127.0.0.1");
        assert!(lua
            .load("return halod.config.token")
            .eval::<mlua::Value>()
            .unwrap()
            .is_nil());
    }

    #[test]
    fn two_vms_built_with_different_config_maps_never_see_each_others_values() {
        let lua_a = Lua::new();
        apply(
            &lua_a,
            &[],
            &HashMap::from([("secret".to_string(), "plugin-a-value".to_string())]),
        )
        .unwrap();
        let lua_b = Lua::new();
        apply(
            &lua_b,
            &[],
            &HashMap::from([("secret".to_string(), "plugin-b-value".to_string())]),
        )
        .unwrap();

        let a: String = lua_a.load("return halod.config.secret").eval().unwrap();
        let b: String = lua_b.load("return halod.config.secret").eval().unwrap();
        assert_eq!(a, "plugin-a-value");
        assert_eq!(b, "plugin-b-value");
    }

    #[test]
    fn empty_config_map_still_exposes_an_empty_table() {
        let lua = Lua::new();
        apply(&lua, &[], &HashMap::new()).unwrap();
        lua.load("assert(type(halod.config) == 'table')")
            .exec()
            .unwrap();
    }
}
