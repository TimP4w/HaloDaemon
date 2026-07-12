// SPDX-License-Identifier: GPL-3.0-or-later
//! Restricts the Lua environment a plugin runs in. Plugins are trusted (they can
//! talk to their matched device) but sandboxed against filesystem/process/native
//! escape hatches. `string`/`table`/`math` (incl. Lua 5.4 bitwise ops and
//! `string.pack`) stay available — they're what protocol encoding needs.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{HookTriggers, Lua, Table, VmState};

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
    strip_escape_hatches(lua)?;
    let logger = lua.create_function(|_, msg: mlua::String| {
        log::info!("[plugin] {}", msg.to_string_lossy());
        Ok(())
    })?;
    globals.set("log", logger)?;
    bytebuf::register(lua)?;
    register_sleep(lua)?;
    super::image_api::register(lua)?;
    inject_config(lua, config)?;

    if granted.contains(&Permission::Os) {
        reinject_clock(lua)?;
    }
    Ok(())
}

/// Remove every filesystem/process/native escape hatch from `lua`'s globals.
/// Shared by the runtime sandbox and the manifest parser, so a plugin script
/// can't reach `os`/`io`/`require`/… even while its manifest table is being
/// read (which evaluates the whole script).
pub(super) fn strip_escape_hatches(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in REMOVED {
        globals.set(*name, mlua::Value::Nil)?;
    }
    Ok(())
}

/// Build a fresh sandboxed plugin VM: strip escape hatches, re-inject the
/// `halod` surface for `granted`/`config`, cap the Lua allocator at
/// `memory_limit`, and install the `instruction_budget` hook. Returns the VM and
/// the budget counter (reset per call to make the budget per-callback). Every
/// plugin VM — device worker, effect worker, `pre_scan`, manifest parse — goes
/// through here so none can silently skip a limit (an earlier drift left the
/// effect VM with no memory cap).
pub(super) fn bootstrap_vm(
    granted: &[Permission],
    config: &HashMap<String, String>,
    memory_limit: usize,
    instruction_budget: u64,
) -> mlua::Result<(Lua, Rc<Cell<u64>>)> {
    let lua = Lua::new();
    apply(&lua, granted, config)?;
    let _ = lua.set_memory_limit(memory_limit);
    let budget = install_instruction_budget_hook(&lua, instruction_budget);
    Ok((lua, budget))
}

/// How many instructions elapse between hook firings. The budget is only
/// enforced to this granularity, which keeps the hook's own overhead negligible.
const BUDGET_HOOK_STEP: u32 = 10_000;

/// Install an instruction-count hook that errors once the VM burns through
/// `budget` instructions, and return the shared counter so a caller can reset it
/// (`counter.set(0)`) to make the budget per-call rather than per-VM-lifetime.
/// The counter is a single-threaded `Rc<Cell>` — every plugin VM lives on its
/// own worker thread and never crosses threads. Shared by the manifest parser,
/// the effect worker, and the device worker so a runaway `while true do end`
/// can't hang any of them.
pub(super) fn install_instruction_budget_hook(lua: &Lua, budget: u64) -> Rc<Cell<u64>> {
    let counter = Rc::new(Cell::new(0u64));
    let hook_counter = counter.clone();
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(BUDGET_HOOK_STEP),
        move |_, _| {
            let n = hook_counter.get().saturating_add(BUDGET_HOOK_STEP as u64);
            hook_counter.set(n);
            if n > budget {
                return Err(mlua::Error::RuntimeError(
                    "plugin script exceeded its instruction budget".into(),
                ));
            }
            Ok(VmState::Continue)
        },
    );
    counter
}

/// Longest a single `halod.sleep_ms` call may block the worker thread. The
/// runtime instruction budget kills an *uncaught* runaway, but a `pcall`-catching
/// loop stays on the worker until the caller's per-request deadline fires
/// (see `LuaWorker`), so this only bounds a single call from pathologically
/// stalling the device's command queue — protocol inter-transfer gaps are
/// milliseconds, not seconds.
const MAX_SLEEP_MS: u64 = 5_000;

/// Expose `halod.sleep_ms(ms)`: a blocking sleep on the (per-device) worker
/// thread, for protocols that need timed gaps between transfers (DDC/CI's write
/// gap and read delay, say). Blocking the worker only serializes that one
/// device's own queued commands — the async runtime is untouched.
fn register_sleep(lua: &Lua) -> mlua::Result<()> {
    let halod: Table = lua.globals().get("halod")?;
    let sleep = lua.create_function(|_, ms: u64| {
        std::thread::sleep(std::time::Duration::from_millis(ms.min(MAX_SLEEP_MS)));
        Ok(())
    })?;
    halod.set("sleep_ms", sleep)
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

    #[test]
    fn instruction_budget_hook_kills_a_runaway_loop() {
        let lua = Lua::new();
        install_instruction_budget_hook(&lua, 1_000_000);
        let err = lua.load("while true do end").exec().unwrap_err();
        assert!(err.to_string().contains("instruction budget"));
    }

    #[test]
    fn instruction_budget_hook_allows_bounded_work() {
        let lua = Lua::new();
        install_instruction_budget_hook(&lua, 1_000_000);
        let sum: i64 = lua
            .load("local s = 0 for i = 1, 100 do s = s + i end return s")
            .eval()
            .unwrap();
        assert_eq!(sum, 5050);
    }

    #[test]
    fn resetting_the_counter_makes_the_budget_per_call() {
        let lua = Lua::new();
        let budget = install_instruction_budget_hook(&lua, 1_000_000);
        let loop_src = "local s = 0 for i = 1, 1000 do s = s + i end";
        // Run enough iterations that, without a reset, the cumulative count would
        // exceed the budget — resetting before each call keeps every call bounded.
        for _ in 0..1_000 {
            budget.set(0);
            lua.load(loop_src).exec().unwrap();
        }
    }
}
