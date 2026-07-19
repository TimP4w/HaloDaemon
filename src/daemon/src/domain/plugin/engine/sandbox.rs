// SPDX-License-Identifier: GPL-3.0-or-later
//! Restricts the Lua environment a plugin runs in. Plugins are trusted (they can
//! talk to their matched device) but sandboxed against filesystem/process/native
//! escape hatches. `string`/`table`/`math` (incl. Lua 5.4 bitwise ops and
//! `string.pack`) stay available — they're what protocol encoding needs.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{HookTriggers, Lua, LuaOptions, StdLib, Table, VmState};

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
    "print",
];

/// Strip every escape hatch, then re-inject `halod`/`log`, then selectively
/// re-enable permission-gated globals the plugin was actually granted.
/// `granted` is the effective declared ∩ user-accepted set guaranteed by
/// `Registry::granted_for`; this function only ever adds capability, never checks
/// consent itself. `config` is this plugin's own resolved config values (see
/// `plugins::resolved_config_for`) — never another plugin's, since the caller
/// builds one `config` map per VM.
/// A plugin's optional `log` level.
fn plugin_log_level(name: Option<&str>) -> log::Level {
    match name {
        Some("trace") => log::Level::Trace,
        Some("debug") => log::Level::Debug,
        Some("warn") => log::Level::Warn,
        _ => log::Level::Info,
    }
}

pub fn apply(
    lua: &Lua,
    granted: &[Permission],
    config: &crate::domain::plugin::ResolvedConfig,
) -> mlua::Result<()> {
    let globals = lua.globals();
    strip_escape_hatches(lua)?;
    let logger = lua.create_function(|_, (msg, level): (mlua::String, Option<mlua::String>)| {
        let level = plugin_log_level(level.as_ref().map(|l| l.to_string_lossy()).as_deref());
        log::log!(level, "[plugin] {}", msg.to_string_lossy());
        Ok(())
    })?;
    globals.set("log", logger)?;
    bytebuf::register(lua)?;
    inject_platform(lua)?;
    register_sleep(lua)?;
    super::image_api::register(lua)?;
    inject_config(lua, config)?;

    if granted.contains(&Permission::Os) {
        reinject_clock(lua)?;
    }
    Ok(())
}

/// Install the package-local module loader. Module functions are compiled from
/// the sources indexed while parsing this package; the VM never receives a
/// filesystem path and cannot traverse into a sibling plugin. Results follow
/// Lua `require` semantics and are cached once per VM.
pub(super) fn install_package_modules(
    lua: &Lua,
    sources: &std::collections::BTreeMap<String, String>,
) -> mlua::Result<()> {
    let loaders = lua.create_table()?;
    for (name, source) in sources {
        let function = lua
            .load(source)
            .set_name(format!("@{name}"))
            .into_function()?;
        loaders.set(name.as_str(), function)?;
    }
    let cache = lua.create_table()?;
    let loading = lua.create_table()?;
    let require = lua.create_function(move |_, name: String| {
        let loader = match loaders.get::<mlua::Value>(name.as_str())? {
            mlua::Value::Function(loader) => loader,
            _ => {
                return Err(mlua::Error::RuntimeError(format!(
                    "package-local Lua module '{name}' is not available"
                )))
            }
        };
        let cached = cache.get::<mlua::Value>(name.as_str())?;
        if !matches!(cached, mlua::Value::Nil) {
            return Ok(cached);
        }
        if loading.get::<bool>(name.as_str()).unwrap_or(false) {
            return Err(mlua::Error::RuntimeError(format!(
                "circular package-local Lua module dependency at '{name}'"
            )));
        }
        loading.set(name.as_str(), true)?;
        let result = loader.call::<mlua::Value>(());
        loading.set(name.as_str(), false)?;
        let mut value = result?;
        if matches!(value, mlua::Value::Nil) {
            value = mlua::Value::Boolean(true);
        }
        cache.set(name.as_str(), value.clone())?;
        Ok(value)
    })?;
    let halod: Table = lua.globals().get("halod")?;
    halod.set("require", require)
}

/// Remove every filesystem/process/native escape hatch from a runtime VM.
pub(super) fn strip_escape_hatches(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in REMOVED {
        globals.set(*name, mlua::Value::Nil)?;
    }
    Ok(())
}

/// Build a fresh sandboxed plugin VM: strip escape hatches (and, for a full
/// runtime, re-inject the `halod` surface for `granted`/`config`), cap the Lua
/// allocator at `memory_limit`, and install the `instruction_budget` hook.
/// Returns the VM and the budget counter (reset per call to make the budget
/// per-callback). Every plugin VM — device worker, effect worker, `pre_scan`,
/// manifest parse — goes through here so none can silently skip a limit (an
/// earlier drift left the effect VM with no memory cap; another the manifest
/// parser hand-rolling the trio outside this chokepoint).
pub(super) fn bootstrap_vm(
    granted: &[Permission],
    config: &crate::domain::plugin::ResolvedConfig,
    memory_limit: usize,
    instruction_budget: u64,
) -> mlua::Result<(Lua, Rc<Cell<u64>>)> {
    let libs = StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::COROUTINE | StdLib::UTF8;
    let lua = Lua::new_with(libs, LuaOptions::default())?;
    lua.set_app_data(CallDeadline(Rc::new(Cell::new(None))));
    apply(&lua, granted, config)?;
    lua.set_memory_limit(memory_limit)?;
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

/// Absolute wall-clock deadline for the current callback, stored as VM app-data
/// and reset per job. Blocking host calls (sleep) refuse to run past it, so a
/// callback can't chain sleeps to outlast the worker's request deadline.
pub(super) struct CallDeadline(pub Rc<Cell<Option<Instant>>>);

/// Reset the current callback's deadline to `now + budget`.
pub(super) fn set_call_deadline(lua: &Lua, budget: Duration) {
    if let Some(d) = lua.app_data_ref::<CallDeadline>() {
        d.0.set(Some(Instant::now() + budget));
    }
}

/// Expose `halod.sleep_ms(ms)`: a blocking sleep on the (per-device) worker
/// thread, for protocols that need timed gaps between transfers (DDC/CI's write
/// gap and read delay, say). Blocking the worker only serializes that one
/// device's own queued commands — the async runtime is untouched.
fn register_sleep(lua: &Lua) -> mlua::Result<()> {
    let halod: Table = lua.globals().get("halod")?;
    let sleep = lua.create_function(|lua, ms: u64| {
        let requested = Duration::from_millis(ms.min(MAX_SLEEP_MS));
        let remaining = lua
            .app_data_ref::<CallDeadline>()
            .and_then(|d| d.0.get())
            .map(|dl| dl.saturating_duration_since(Instant::now()));
        match remaining {
            Some(r) if r.is_zero() => {
                return Err(mlua::Error::RuntimeError(
                    "plugin callback deadline exceeded".into(),
                ))
            }
            Some(r) => std::thread::sleep(requested.min(r)),
            None => std::thread::sleep(requested),
        }
        Ok(())
    })?;
    halod.set("sleep_ms", sleep)?;
    // A duration-only clock is safe to expose to every package: unlike wall
    // time it conveys no host date/time information and is useful for polling
    // coalescing without requesting the `os` permission.
    let origin = std::sync::OnceLock::<std::time::Instant>::new();
    let monotonic_ms = lua.create_function(move |_, ()| {
        Ok(origin
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis() as u64)
    })?;
    halod.set("monotonic_ms", monotonic_ms)
}

fn inject_platform(lua: &Lua) -> mlua::Result<()> {
    let halod: Table = lua.globals().get("halod")?;
    halod.set("platform", std::env::consts::OS)
}

/// Populate `halod.config` with this plugin's resolved values. Read-only in
/// spirit (nothing enforces it in Lua, but plugins have no reason to mutate
/// it and no callback ever reads it back from the host).
fn inject_config(lua: &Lua, config: &crate::domain::plugin::ResolvedConfig) -> mlua::Result<()> {
    let halod: Table = lua.globals().get("halod")?;
    let table = lua.create_table()?;
    for (key, value) in config {
        match value {
            crate::domain::plugin::ResolvedConfigValue::Boolean(value) => {
                table.set(key.as_str(), *value)?
            }
            crate::domain::plugin::ResolvedConfigValue::Number(value) => {
                table.set(key.as_str(), *value)?
            }
            crate::domain::plugin::ResolvedConfigValue::Integer(value) => {
                table.set(key.as_str(), *value)?
            }
            crate::domain::plugin::ResolvedConfigValue::String(value) => {
                table.set(key.as_str(), value.as_str())?
            }
        }
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
    use std::collections::HashMap;

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
    fn bootstrap_never_loads_dangerous_stdlibs_internally() {
        let (lua, _) = bootstrap_vm(&[], &HashMap::new(), 1024 * 1024, 1_000_000).unwrap();
        let loaded: Table = lua.named_registry_value("_LOADED").unwrap();

        for name in ["os", "io", "package", "debug"] {
            let value: mlua::Value = loaded.get(name).unwrap();
            assert!(value.is_nil(), "stdlib '{name}' was loaded internally");
        }
        for name in ["string", "table", "math", "coroutine", "utf8"] {
            let value: mlua::Value = loaded.get(name).unwrap();
            assert!(!value.is_nil(), "stdlib '{name}' was not loaded");
        }
    }

    #[test]
    fn log_accepts_an_optional_level_and_never_fails_the_call() {
        let (lua, _) = bootstrap_vm(&[], &HashMap::new(), 1024 * 1024, 1_000_000).unwrap();
        for call in [
            r#"log("m")"#,
            r#"log("m", "trace")"#,
            r#"log("m", "nonsense")"#,
            r#"log("m", 42)"#,
        ] {
            lua.load(call)
                .exec()
                .unwrap_or_else(|e| panic!("{call}: {e}"));
        }
        assert_eq!(plugin_log_level(Some("trace")), log::Level::Trace);
        assert_eq!(plugin_log_level(Some("nonsense")), log::Level::Info);
        assert_eq!(plugin_log_level(None), log::Level::Info);
    }

    #[test]
    fn print_is_not_available_to_plugins() {
        let (lua, _) = bootstrap_vm(&[], &HashMap::new(), 1024 * 1024, 1_000_000).unwrap();
        let print: mlua::Value = lua.globals().get("print").unwrap();
        assert!(print.is_nil());
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
        let config = HashMap::from([(
            "host".to_string(),
            crate::domain::plugin::ResolvedConfigValue::String("127.0.0.1".to_string()),
        )]);
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
            &HashMap::from([(
                "secret".to_string(),
                crate::domain::plugin::ResolvedConfigValue::String("plugin-a-value".to_string()),
            )]),
        )
        .unwrap();
        let lua_b = Lua::new();
        apply(
            &lua_b,
            &[],
            &HashMap::from([(
                "secret".to_string(),
                crate::domain::plugin::ResolvedConfigValue::String("plugin-b-value".to_string()),
            )]),
        )
        .unwrap();

        let a: String = lua_a.load("return halod.config.secret").eval().unwrap();
        let b: String = lua_b.load("return halod.config.secret").eval().unwrap();
        assert_eq!(a, "plugin-a-value");
        assert_eq!(b, "plugin-b-value");
    }

    #[test]
    fn halod_config_preserves_declared_lua_types() {
        let lua = Lua::new();
        let config = HashMap::from([
            (
                "enabled".to_string(),
                crate::domain::plugin::ResolvedConfigValue::Boolean(true),
            ),
            (
                "scale".to_string(),
                crate::domain::plugin::ResolvedConfigValue::Number(1.5),
            ),
            (
                "timeout".to_string(),
                crate::domain::plugin::ResolvedConfigValue::Integer(2500),
            ),
            (
                "mode".to_string(),
                crate::domain::plugin::ResolvedConfigValue::String("quiet".to_string()),
            ),
        ]);
        apply(&lua, &[], &config).unwrap();

        lua.load(
            "assert(type(halod.config.enabled) == 'boolean')\n\
             assert(type(halod.config.scale) == 'number')\n\
             assert(type(halod.config.timeout) == 'number')\n\
             assert(type(halod.config.mode) == 'string')",
        )
        .exec()
        .unwrap();
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
    fn package_modules_are_cached_and_cannot_escape_the_index() {
        let lua = Lua::new();
        apply(&lua, &[], &HashMap::new()).unwrap();
        let sources = std::collections::BTreeMap::from([(
            "lib.counter".to_owned(),
            "local n = 0; return function() n = n + 1; return n end".to_owned(),
        )]);
        install_package_modules(&lua, &sources).unwrap();
        let values: (u32, u32) = lua
            .load(
                "local a = halod.require('lib.counter'); \
                 local b = halod.require('lib.counter'); return a(), b()",
            )
            .eval()
            .unwrap();
        assert_eq!(values, (1, 2), "the same module instance is cached");

        for name in ["../other/main", "other.main", "C:\\other\\main"] {
            let err = lua
                .load(format!("return halod.require({name:?})"))
                .eval::<mlua::Value>()
                .unwrap_err();
            assert!(err.to_string().contains("not available"), "{err}");
        }
    }

    #[test]
    fn monotonic_clock_is_available_without_os_permission() {
        let lua = Lua::new();
        apply(&lua, &[], &HashMap::new()).unwrap();
        let first: u64 = lua.load("return halod.monotonic_ms()").eval().unwrap();
        let second: u64 = lua.load("return halod.monotonic_ms()").eval().unwrap();
        assert!(second >= first);
        let os: mlua::Value = lua.load("return os").eval().unwrap();
        assert!(os.is_nil());
    }

    #[test]
    fn platform_is_available_without_os_permission() {
        let lua = Lua::new();
        apply(&lua, &[], &HashMap::new()).unwrap();
        let platform: String = lua.load("return halod.platform").eval().unwrap();
        assert_eq!(platform, std::env::consts::OS);
    }

    #[test]
    fn sleep_refuses_once_the_callback_deadline_passes() {
        let lua = Lua::new();
        lua.set_app_data(CallDeadline(Rc::new(Cell::new(None))));
        apply(&lua, &[], &HashMap::new()).unwrap();
        // Deadline already in the past → sleep must error.
        set_call_deadline(&lua, Duration::from_millis(0));
        std::thread::sleep(Duration::from_millis(1));
        let err = lua.load("halod.sleep_ms(10)").exec().unwrap_err();
        assert!(err.to_string().contains("deadline"), "{err}");
    }

    #[test]
    fn sleep_within_deadline_is_allowed() {
        let lua = Lua::new();
        lua.set_app_data(CallDeadline(Rc::new(Cell::new(None))));
        apply(&lua, &[], &HashMap::new()).unwrap();
        set_call_deadline(&lua, Duration::from_secs(5));
        lua.load("halod.sleep_ms(1)").exec().unwrap();
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
