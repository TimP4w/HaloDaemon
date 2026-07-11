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

use crate::drivers::transports::Transport;
use crate::drivers::{Metered, RgbCapability};
use halod_shared::types::{RgbColor, RgbState, WriteRateLimit, WriteRateStatus};

use super::device::LuaDevice;
use super::manifest::{parse_manifest_from_dir, PluginManifest};
use super::transport::PluginIo;
use super::worker::DevMatch;

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

    let manifest = manifest.clone();
    h.set(
        "open",
        lua.create_function(move |lua, (_self, spec): (Table, Option<Table>)| {
            open_device(lua, &manifest, handle.clone(), spec).map_err(mlua_err)
        })
        .anyhow()?,
    )
    .anyhow()?;

    Ok(h)
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
        pid: spec.pid,
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

/// The only `RgbState` shape the harness builds today: `{ mode = "static",
/// color = { r=.., g=.., b=.. } }`. Extend here as packages need more shapes.
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
                color: RgbColor {
                    r: color.get("r").unwrap_or(0),
                    g: color.get("g").unwrap_or(0),
                    b: color.get("b").unwrap_or(0),
                },
            })
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
