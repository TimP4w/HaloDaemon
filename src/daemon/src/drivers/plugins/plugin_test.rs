// SPDX-License-Identifier: GPL-3.0-or-later
//! `halod plugin-test <package-dir>` — drives a plugin package's declared
//! `test.lua` against a recording mock transport, so the *official plugin
//! repo*'s own CI can validate a driver change without real hardware. The
//! daemon owns the Lua worker + transport machinery here; the test *cases*
//! live in the plugin repo, one `test.lua` per package.
//!
//! Scope today: `hid`/`tcp`-transport (`PluginIo::Stream`) device plugins,
//! exercising `initialize()` and `apply()` (static color) against the first
//! declared device. SMBus/usb_control support can be added the same way
//! (a new recording backend + `PluginIo` arm) when a package needs it.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use mlua::{Function, Lua, Table, Value};

use crate::drivers::chain::ChainAdapter;
use crate::drivers::transports::Transport;
use crate::drivers::{Metered, RgbCapability};
use std::collections::HashMap;

use halod_shared::types::{EffectParamValue, RgbColor, RgbState, WriteRateLimit, WriteRateStatus};

use super::device::LuaDevice;
use super::manifest::{parse_manifest_from_dir, PluginManifest};
use super::transport::PluginIo;
use super::worker::{DevMatch, PluginHandle};

/// `mlua::Error` isn't reliably `Send + Sync` (an `ExternalError` may box a
/// non-Send/Sync inner error), so it can't flow through `anyhow::Result` via
/// plain `?` — flatten it to a message first.
trait LuaResultExt<T> {
    fn anyhow(self) -> Result<T>;
}
impl<T> LuaResultExt<T> for mlua::Result<T> {
    fn anyhow(self) -> Result<T> {
        self.map_err(|e| anyhow::anyhow!("{e}"))
    }
}

fn mlua_err(e: anyhow::Error) -> mlua::Error {
    mlua::Error::RuntimeError(format!("{e:#}"))
}

/// Records every write; replays scripted reads in order. Never touches real hardware.
struct RecordingStream {
    written: Mutex<Vec<Vec<u8>>>,
    reads: Mutex<std::collections::VecDeque<Vec<u8>>>,
    rate: Metered<()>,
}

impl RecordingStream {
    fn new(reads: Vec<Vec<u8>>) -> Self {
        Self {
            written: Mutex::new(Vec::new()),
            reads: Mutex::new(reads.into()),
            rate: Metered::new((), None),
        }
    }
}

#[async_trait]
impl Transport for RecordingStream {
    async fn write(&self, data: &[u8]) -> Result<()> {
        self.rate.write_access(data.len()).await?;
        self.written
            .lock()
            .expect("recording stream poisoned")
            .push(data.to_vec());
        Ok(())
    }

    async fn read(&self, _size: usize) -> Result<Vec<u8>> {
        self.reads
            .lock()
            .expect("recording stream poisoned")
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("no more scripted reads queued for this device"))
    }

    fn rate_status(&self) -> WriteRateStatus {
        self.rate.status()
    }

    fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.rate.set_limit(limit);
    }
}

/// Pass/fail counters a running `test.lua` accumulates into via `h:assert`/`h:assert_eq`.
#[derive(Default)]
struct Report {
    passed: u32,
    failed: u32,
}

/// Run `package`'s `test.lua` (if present) and return a process exit code:
/// `0` if every assertion passed (or there was no `test.lua` to run), `1` if
/// any failed. `handle` is used to drive the plugin's async capability calls
/// from the synchronous Lua callbacks.
pub fn run(handle: tokio::runtime::Handle, package: &Path) -> Result<i32> {
    let manifest = parse_manifest_from_dir(package)
        .with_context(|| format!("parsing plugin package {}", package.display()))?;

    let test_path = package.join("test.lua");
    if !test_path.is_file() {
        println!("{}: no test.lua — skipping", manifest.plugin_id);
        return Ok(0);
    }
    let test_src = std::fs::read_to_string(&test_path)
        .with_context(|| format!("reading {}", test_path.display()))?;

    let report = Arc::new(Mutex::new(Report::default()));
    let lua = Lua::new();
    let harness = build_harness(&lua, &manifest, handle, report.clone())?;

    let test_fn: Function = lua
        .load(&test_src)
        .set_name(test_path.display().to_string())
        .eval()
        .anyhow()
        .with_context(|| format!("loading {}", test_path.display()))?;
    test_fn
        .call::<()>(harness)
        .anyhow()
        .with_context(|| format!("running {}", test_path.display()))?;

    let report = report.lock().expect("report poisoned");
    println!(
        "{}: {} passed, {} failed",
        manifest.plugin_id, report.passed, report.failed
    );
    Ok(if report.failed == 0 { 0 } else { 1 })
}

/// Build the `h` table (`assert`/`assert_eq`/`open`) a package's `test.lua` receives.
fn build_harness(
    lua: &Lua,
    manifest: &PluginManifest,
    handle: tokio::runtime::Handle,
    report: Arc<Mutex<Report>>,
) -> Result<Table> {
    let h = lua.create_table().anyhow()?;

    let assert_report = report.clone();
    h.set(
        "assert",
        // `h:assert(...)` passes `h` itself as the leading self argument.
        lua.create_function(
            move |_, (_self, cond, msg): (Table, bool, Option<String>)| {
                let mut r = assert_report.lock().expect("report poisoned");
                let msg = msg.unwrap_or_default();
                if cond {
                    r.passed += 1;
                    println!("ok - {msg}");
                } else {
                    r.failed += 1;
                    println!("not ok - {msg}");
                }
                Ok(())
            },
        )
        .anyhow()?,
    )
    .anyhow()?;

    let eq_report = report.clone();
    h.set(
        "assert_eq",
        lua.create_function(
            move |_, (_self, a, b, msg): (Table, Value, Value, Option<String>)| {
                let mut r = eq_report.lock().expect("report poisoned");
                let msg = msg.unwrap_or_default();
                if lua_values_equal(&a, &b) {
                    r.passed += 1;
                    println!("ok - {msg}");
                } else {
                    r.failed += 1;
                    println!("not ok - {msg}: {a:?} != {b:?}");
                }
                Ok(())
            },
        )
        .anyhow()?,
    )
    .anyhow()?;

    let open_manifest = manifest.clone();
    let open_handle = handle.clone();
    h.set(
        "open",
        lua.create_function(move |lua, (_self, spec): (Table, Option<Table>)| {
            open_device(lua, &open_manifest, open_handle.clone(), spec).map_err(mlua_err)
        })
        .anyhow()?,
    )
    .anyhow()?;

    let integration_manifest = manifest.clone();
    h.set(
        "open_integration",
        lua.create_function(move |lua, (_self, spec): (Table, Option<Table>)| {
            open_integration(lua, &integration_manifest, handle.clone(), spec).map_err(mlua_err)
        })
        .anyhow()?,
    )
    .anyhow()?;

    Ok(h)
}

/// Build an integration worker directly so external integration plugins can
/// test enumeration without opening a real network connection.
fn open_integration(
    lua: &Lua,
    manifest: &PluginManifest,
    handle: tokio::runtime::Handle,
    spec_table: Option<Table>,
) -> Result<Table> {
    if manifest.plugin_type != halod_shared::types::PluginKind::Integration {
        anyhow::bail!("open_integration requires an integration plugin");
    }
    let recording = Arc::new(RecordingStream::new(reads_from_spec(&spec_table)));
    let worker = PluginHandle::spawn(
        manifest.script_source.clone(),
        PluginIo::Stream {
            transport: recording.clone() as Arc<dyn Transport>,
            bulk: None,
        },
        DevMatch {
            transport: "tcp".into(),
            ..Default::default()
        },
        manifest.permissions.clone(),
        std::collections::HashMap::new(),
        handle.clone(),
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let dev = lua.create_table().anyhow()?;
    {
        let worker = worker.clone();
        let handle = handle.clone();
        dev.set(
            "initialize",
            lua.create_function(move |_, _self: Table| {
                Ok(handle.block_on(worker.initialize()).map_err(mlua_err)?.ok)
            })
            .anyhow()?,
        )
        .anyhow()?;
    }
    {
        let worker = worker.clone();
        let handle = handle.clone();
        dev.set(
            "enumerate_controllers",
            lua.create_function(move |lua, _self: Table| {
                let controllers = handle
                    .block_on(worker.enumerate_controllers())
                    .map_err(mlua_err)?;
                let out = lua.create_table()?;
                for (i, controller) in controllers.iter().enumerate() {
                    let item = lua.create_table()?;
                    item.set("index", controller.index)?;
                    item.set("name", controller.name.clone())?;
                    item.set("serial", controller.serial.clone())?;
                    item.set("location", controller.location.clone())?;
                    let zones = lua.create_table()?;
                    for (z, zone) in controller.zones.iter().enumerate() {
                        let zone_t = lua.create_table()?;
                        zone_t.set("id", zone.id.clone())?;
                        zone_t.set("name", zone.name.clone())?;
                        zone_t.set("led_count", zone.led_count)?;
                        zones.set(z + 1, zone_t)?;
                    }
                    item.set("zones", zones)?;
                    out.set(i + 1, item)?;
                }
                Ok(out)
            })
            .anyhow()?,
        )
        .anyhow()?;
    }
    {
        let recording = recording.clone();
        dev.set(
            "writes",
            lua.create_function(move |lua, _self: Table| {
                let written = recording.written.lock().expect("recording poisoned");
                let out = lua.create_table()?;
                for (i, bytes) in written.iter().enumerate() {
                    out.set(i + 1, lua.create_sequence_from(bytes.iter().copied())?)?;
                }
                Ok(out)
            })
            .anyhow()?,
        )
        .anyhow()?;
    }
    Ok(dev)
}

/// `serde`-compare two Lua values (mlua's `serialize` feature makes `Value` `Serialize`).
fn lua_values_equal(a: &Value, b: &Value) -> bool {
    let a = serde_json::to_value(a).unwrap_or(serde_json::Value::Null);
    let b = serde_json::to_value(b).unwrap_or(serde_json::Value::Null);
    a == b
}

/// Read a Lua array-of-byte-arrays table (`spec.reads`) into scripted read replies.
fn reads_from_spec(spec: &Option<Table>) -> Vec<Vec<u8>> {
    let Some(spec) = spec else {
        return Vec::new();
    };
    let Ok(reads_tbl) = spec.get::<Table>("reads") else {
        return Vec::new();
    };
    reads_tbl
        .sequence_values::<Table>()
        .filter_map(|r| r.ok())
        .map(|bytes| {
            bytes
                .sequence_values::<u8>()
                .filter_map(|b| b.ok())
                .collect()
        })
        .collect()
}

/// Build a `LuaDevice` over the plugin's first declared device, wired to a
/// fresh `RecordingStream`, and return it as a Lua table of methods.
fn open_device(
    lua: &Lua,
    manifest: &PluginManifest,
    handle: tokio::runtime::Handle,
    spec_table: Option<Table>,
) -> Result<Table> {
    let spec = manifest
        .devices
        .first()
        .context("plugin declares no devices")?;
    if !matches!(spec.transport.as_str(), "hid" | "tcp") {
        anyhow::bail!(
            "plugin-test harness only supports hid/tcp transports today (got '{}')",
            spec.transport
        );
    }

    let recording = Arc::new(RecordingStream::new(reads_from_spec(&spec_table)));
    let dev_match = DevMatch {
        transport: spec.transport.clone(),
        bus: spec.bus.clone(),
        addr: None,
        vid: spec.vid,
        pid: spec.pid,
        index: None,
    };
    let device = Arc::new(LuaDevice::with_transport(
        "plugin-test".to_owned(),
        manifest,
        spec,
        dev_match,
        PluginIo::Stream {
            transport: recording.clone() as Arc<dyn Transport>,
            bulk: None,
        },
        handle.clone(),
        // The harness grants every declared permission (so gated transports
        // open), uses no config, and has no `AppState` to notify.
        manifest.permissions.clone(),
        std::collections::HashMap::new(),
        std::sync::Weak::new(),
        Arc::new(std::sync::Mutex::new(
            super::device::RuntimeState::OpeningTransport,
        )),
    ));

    let dev_table = lua.create_table().anyhow()?;

    {
        let device = device.clone();
        let handle = handle.clone();
        dev_table
            .set(
                "initialize",
                lua.create_function(move |_, _self: Table| {
                    handle
                        .block_on(crate::drivers::Device::initialize(&*device))
                        .map_err(mlua_err)
                })
                .anyhow()?,
            )
            .anyhow()?;
    }

    {
        let device = device.clone();
        let handle = handle.clone();
        dev_table
            .set(
                "apply",
                lua.create_function(move |_, (_self, state): (Table, Table)| {
                    let rgb_state = table_to_rgb_state(&state).map_err(mlua_err)?;
                    handle.block_on(device.apply(rgb_state)).map_err(mlua_err)
                })
                .anyhow()?,
            )
            .anyhow()?;
    }

    {
        let recording = recording.clone();
        dev_table
            .set(
                "writes",
                lua.create_function(move |lua, _self: Table| {
                    let written = recording.written.lock().expect("recording poisoned");
                    let out = lua.create_table()?;
                    for (i, bytes) in written.iter().enumerate() {
                        let entry = lua.create_table()?;
                        entry.set("data", lua.create_sequence_from(bytes.iter().copied())?)?;
                        out.set(i + 1, entry)?;
                    }
                    Ok(out)
                })
                .anyhow()?,
            )
            .anyhow()?;
    }

    {
        let device = device.clone();
        let handle = handle.clone();
        dev_table
            .set(
                "write_ext_frame",
                lua.create_function(move |_, (_self, channel, colors): (Table, String, Table)| {
                    let composed: Vec<RgbColor> = colors
                        .sequence_values::<Table>()
                        .filter_map(|c| c.ok().map(|t| table_to_color(&t)))
                        .collect();
                    handle
                        .block_on(device.write_composed_frame(&channel, &composed))
                        .map_err(mlua_err)
                })
                .anyhow()?,
            )
            .anyhow()?;
    }

    {
        let recording = recording.clone();
        dev_table
            .set(
                "clear",
                lua.create_function(move |_, _self: Table| {
                    recording
                        .written
                        .lock()
                        .expect("recording poisoned")
                        .clear();
                    Ok(())
                })
                .anyhow()?,
            )
            .anyhow()?;
    }

    Ok(dev_table)
}

fn table_to_color(t: &Table) -> RgbColor {
    RgbColor {
        r: t.get("r").unwrap_or(0),
        g: t.get("g").unwrap_or(0),
        b: t.get("b").unwrap_or(0),
    }
}

/// Map one Lua param value to an `EffectParamValue` (mirrors the untagged serde
/// shape a native effect param uses): a `{r,g,b}` table is a color, otherwise a
/// number/string/bool maps to the matching scalar.
fn value_to_param(v: &Value) -> Option<EffectParamValue> {
    match v {
        Value::Table(t) => Some(EffectParamValue::Color(table_to_color(t))),
        Value::Integer(i) => Some(EffectParamValue::Float(*i as f64)),
        Value::Number(n) => Some(EffectParamValue::Float(*n)),
        Value::String(s) => Some(EffectParamValue::Str(s.to_string_lossy().to_string())),
        Value::Boolean(b) => Some(EffectParamValue::Bool(*b)),
        _ => None,
    }
}

/// The `RgbState` shapes the harness builds: `static`, `per_led`, and
/// `native_effect`. Extend here as packages need more shapes.
fn table_to_rgb_state(state: &Table) -> Result<RgbState> {
    let mode: String = state
        .get("mode")
        .anyhow()
        .context("state.mode is required")?;
    match mode.as_str() {
        "static" => {
            let color: Table = state
                .get("color")
                .anyhow()
                .context("static state needs a color")?;
            Ok(RgbState::Static {
                color: table_to_color(&color),
            })
        }
        "per_led" => {
            let zones_t: Table = state
                .get("zones")
                .anyhow()
                .context("per_led state needs a zones table")?;
            let mut zones = HashMap::new();
            for pair in zones_t.pairs::<String, Table>() {
                let (zone_id, leds_t) = pair.anyhow()?;
                let mut leds = HashMap::new();
                for led in leds_t.pairs::<String, Table>() {
                    let (led_id, c) = led.anyhow()?;
                    leds.insert(led_id, table_to_color(&c));
                }
                zones.insert(zone_id, leds);
            }
            Ok(RgbState::PerLed { zones })
        }
        "native_effect" => {
            let id: String = state
                .get("id")
                .anyhow()
                .context("native_effect state needs an id")?;
            let mut params = HashMap::new();
            if let Ok(params_t) = state.get::<Table>("params") {
                for pair in params_t.pairs::<String, Value>() {
                    let (key, v) = pair.anyhow()?;
                    if let Some(pv) = value_to_param(&v) {
                        params.insert(key, pv);
                    }
                }
            }
            Ok(RgbState::NativeEffect { id, params })
        }
        other => anyhow::bail!("plugin-test harness does not support state.mode = '{other}' yet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(dir: &Path, test_lua: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: fixture\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("main.lua"),
            r#"return {
                initialize = function(dev) return true end,
                apply = function(dev, state)
                    if state.mode == "static" then
                        local c = state.color
                        dev.transport:write(string.char(0xAA, c.r, c.g, c.b))
                    end
                end,
            }"#,
        )
        .unwrap();
        std::fs::write(dir.join("test.lua"), test_lua).unwrap();
    }

    fn run_fixture(test_lua: &str) -> i32 {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fixture");
        write_fixture(&dir, test_lua);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        run(handle, &dir).unwrap()
    }

    #[test]
    fn passing_assertions_exit_zero() {
        let code = run_fixture(
            r#"return function(h)
                local dev = h:open()
                h:assert(dev:initialize(), "init")
                dev:apply({ mode = "static", color = { r = 1, g = 2, b = 3 } })
                h:assert_eq(dev:writes()[1].data, { 0xAA, 1, 2, 3 }, "bytes match")
            end"#,
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn failing_assertion_exits_nonzero() {
        let code = run_fixture(
            r#"return function(h)
                h:assert(false, "deliberate failure")
            end"#,
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn clear_drains_the_recorded_write_log() {
        let code = run_fixture(
            r#"return function(h)
                local dev = h:open()
                dev:apply({ mode = "static", color = { r = 1, g = 2, b = 3 } })
                dev:clear()
                h:assert_eq(#dev:writes(), 0, "writes cleared")
            end"#,
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn missing_test_lua_is_a_skip_not_a_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: fixture\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        assert_eq!(run(handle, &dir).unwrap(), 0);
    }
}
