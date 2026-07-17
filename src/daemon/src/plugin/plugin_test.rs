// SPDX-License-Identifier: GPL-3.0-or-later
//! `halod plugin-test <package-dir>` — drives a plugin package's declared
//! `test.lua` against a recording mock transport, so the *official plugin
//! repo*'s own CI can validate a driver change without real hardware. The
//! daemon owns the Lua worker + transport machinery here; the test *cases*
//! live in the plugin repo, one `test.lua` per package.
//!
//! Covers HID/TCP streams and scoped USB endpoint/control collections against
//! the first declared device without opening host hardware.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use mlua::{Function, Lua, LuaSerdeExt, Table, Value};

use crate::drivers::chain::ChainAdapter;
use crate::drivers::transports::usb::{UsbCollection, UsbControlResult};
use crate::drivers::transports::{HidTransport, Transport, TransportEvent};
use crate::drivers::{Metered, RgbCapability};
use std::collections::HashMap;

use halod_shared::types::{EffectParamValue, RgbColor, RgbState, WriteRateLimit, WriteRateStatus};

use super::manifest::{parse_manifest_from_dir, PluginManifest, UsbConfig};
use super::runtime::device::{LuaDevice, LuaDeviceParts, LuaDeviceSpawnParts, LuaDeviceWorker};
use super::runtime::transport::{CommandExecutor, CommandRunResult, PluginIo};
use super::runtime::worker::{DevMatch, PluginHandle};

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

/// One recorded write, tagged with the collection it was routed to so a test
/// can assert long frames went to the companion and short frames to primary.
struct WriteRecord {
    endpoint: &'static str,
    data: Vec<u8>,
}

/// Records every write; replays scripted reads in order. Never touches real
/// hardware. When `companion` is set it advertises a companion collection so a
/// package's Windows short/long routing can be exercised, and it buffers
/// `defer_event` reports for delivery through the event path (`drain_events`).
struct RecordingStream {
    written: Mutex<Vec<WriteRecord>>,
    reads: Mutex<std::collections::VecDeque<Vec<u8>>>,
    deferred: Mutex<std::collections::VecDeque<Vec<u8>>>,
    companion: bool,
    rate: Metered<()>,
}

impl RecordingStream {
    fn new(reads: Vec<Vec<u8>>) -> Self {
        Self::with_companion(reads, false)
    }

    fn with_companion(reads: Vec<Vec<u8>>, companion: bool) -> Self {
        Self {
            written: Mutex::new(Vec::new()),
            reads: Mutex::new(reads.into()),
            deferred: Mutex::new(std::collections::VecDeque::new()),
            companion,
            rate: Metered::new((), None),
        }
    }

    fn record(&self, endpoint: &'static str, data: &[u8]) {
        self.written
            .lock()
            .expect("recording stream poisoned")
            .push(WriteRecord {
                endpoint,
                data: data.to_vec(),
            });
    }

    /// Push a report onto the event queue as if a reader thread had received it
    /// unsolicited — lets a test drive `event()` without a real device.
    fn queue_event(&self, data: Vec<u8>) {
        self.deferred
            .lock()
            .expect("recording stream poisoned")
            .push_back(data);
    }
}

#[async_trait]
impl Transport for RecordingStream {
    async fn write(&self, data: &[u8]) -> Result<()> {
        self.rate.write_access(data.len()).await?;
        self.record("primary", data);
        Ok(())
    }

    async fn read(&self, _size: usize) -> Result<Vec<u8>> {
        self.reads
            .lock()
            .expect("recording stream poisoned")
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("no more scripted reads queued for this device"))
    }

    fn as_hid(&self) -> Option<&dyn HidTransport> {
        Some(self)
    }

    fn rate_status(&self) -> WriteRateStatus {
        self.rate.status()
    }

    fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.rate.set_limit(limit);
    }
}

#[async_trait]
impl HidTransport for RecordingStream {
    async fn feature_exchange(&self, data: &[u8], size: usize) -> Result<Vec<u8>> {
        self.write_then_read(data, size).await
    }

    async fn write_companion(&self, data: &[u8]) -> Result<()> {
        if !self.companion {
            anyhow::bail!("companion collection not available on this recording stream");
        }
        self.rate.write_access(data.len()).await?;
        self.record("companion", data);
        Ok(())
    }

    async fn defer_event(&self, data: &[u8]) -> Result<()> {
        self.queue_event(data.to_vec());
        Ok(())
    }

    async fn drain_events(&self, limit: usize) -> Result<Vec<TransportEvent>> {
        let mut deferred = self.deferred.lock().expect("recording stream poisoned");
        let count = deferred.len().min(limit);
        Ok(deferred
            .drain(..count)
            .map(|data| TransportEvent {
                endpoint: "deferred",
                data,
            })
            .collect())
    }

    fn has_companion(&self) -> bool {
        self.companion
    }

    async fn read_companion(&self, size: usize) -> Result<Vec<u8>> {
        self.read(size).await
    }
}

struct UsbWriteRecord {
    device: String,
    endpoint: Option<u8>,
    request_type: Option<u8>,
    request: Option<u8>,
    data: Vec<u8>,
}

struct RecordingUsb {
    config: UsbConfig,
    written: Mutex<Vec<UsbWriteRecord>>,
    reads: Mutex<std::collections::VecDeque<Vec<u8>>>,
    rate: Metered<()>,
}

impl RecordingUsb {
    fn new(config: UsbConfig, reads: Vec<Vec<u8>>) -> Self {
        Self {
            config,
            written: Mutex::new(Vec::new()),
            reads: Mutex::new(reads.into()),
            rate: Metered::new((), None),
        }
    }
    fn device(&self, id: Option<&str>) -> Result<&super::manifest::UsbDeviceConfig> {
        let id = id.unwrap_or("primary");
        self.config
            .devices
            .iter()
            .find(|d| d.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown USB device '{id}'"))
    }
    fn scripted_read(&self, length: usize) -> Result<Vec<u8>> {
        let mut data = self
            .reads
            .lock()
            .expect("recording USB poisoned")
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("no more scripted USB reads"))?;
        data.truncate(length);
        Ok(data)
    }
}

impl UsbCollection for RecordingUsb {
    fn write(
        &self,
        device_id: Option<&str>,
        endpoint: u8,
        data: &[u8],
        timeout_ms: u64,
    ) -> Result<usize> {
        let device = self.device(device_id)?;
        let policy = device
            .endpoints
            .iter()
            .find(|e| e.address == endpoint)
            .ok_or_else(|| anyhow::anyhow!("USB endpoint outside recording allowlist"))?;
        if endpoint & 0x80 != 0
            || data.len() > policy.max_transfer_size
            || timeout_ms == 0
            || timeout_ms > policy.max_timeout_ms
        {
            anyhow::bail!("USB recording write exceeds endpoint policy");
        }
        self.rate.write_access_blocking(data.len())?;
        self.written
            .lock()
            .expect("recording USB poisoned")
            .push(UsbWriteRecord {
                device: device.id.clone(),
                endpoint: Some(endpoint),
                request_type: None,
                request: None,
                data: data.to_vec(),
            });
        Ok(data.len())
    }
    fn read(
        &self,
        device_id: Option<&str>,
        endpoint: u8,
        length: usize,
        timeout_ms: u64,
    ) -> Result<Vec<u8>> {
        let device = self.device(device_id)?;
        let policy = device
            .endpoints
            .iter()
            .find(|e| e.address == endpoint)
            .ok_or_else(|| anyhow::anyhow!("USB endpoint outside recording allowlist"))?;
        if endpoint & 0x80 == 0
            || length > policy.max_transfer_size
            || timeout_ms == 0
            || timeout_ms > policy.max_timeout_ms
        {
            anyhow::bail!("USB recording read exceeds endpoint policy");
        }
        self.scripted_read(length)
    }
    fn control(
        &self,
        device_id: Option<&str>,
        request_type: u8,
        request: u8,
        _value: u16,
        _index: u16,
        bytes: &[u8],
        read_length: usize,
        timeout_ms: u64,
    ) -> Result<UsbControlResult> {
        let device = self.device(device_id)?;
        let policy = device
            .control
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("USB control outside recording allowlist"))?;
        let length = bytes.len().max(read_length);
        if length > policy.max_transfer_size
            || timeout_ms == 0
            || timeout_ms > policy.max_timeout_ms
        {
            anyhow::bail!("USB recording control exceeds policy");
        }
        if request_type & 0x80 != 0 {
            return Ok(UsbControlResult::Read(self.scripted_read(read_length)?));
        }
        self.rate.write_access_blocking(bytes.len())?;
        self.written
            .lock()
            .expect("recording USB poisoned")
            .push(UsbWriteRecord {
                device: device.id.clone(),
                endpoint: None,
                request_type: Some(request_type),
                request: Some(request),
                data: bytes.to_vec(),
            });
        Ok(UsbControlResult::Written(bytes.len()))
    }
    fn rate_status(&self) -> WriteRateStatus {
        self.rate.status()
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

    if manifest.plugin_type == halod_shared::types::PluginKind::Lcd {
        validate_lcd_widgets(&handle, &manifest)?;
    }

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

fn validate_lcd_widgets(runtime: &tokio::runtime::Handle, manifest: &PluginManifest) -> Result<()> {
    let ids: Vec<String> = manifest
        .widgets
        .iter()
        .map(|widget| widget.id.clone())
        .collect();
    let _runtime_guard = runtime.enter();
    let worker = super::runtime::widget_worker::PluginWidgetHandle::spawn_with_data(
        manifest.script_source.clone(),
        manifest.module_sources.clone(),
        ids,
        manifest.permissions.clone(),
        HashMap::new(),
        super::runtime::data_api::DataRuntime::new(
            Arc::new(crate::services::data_bus::DataBus::default()),
            manifest.plugin_id.clone(),
            &manifest.provides,
            manifest.consumes.clone(),
        ),
    );
    let font = ab_glyph::FontArc::try_from_slice(include_bytes!(
        "../../../assets/fonts/NotoSans-Regular.ttf"
    ))
    .expect("bundled test font is valid");
    for widget in &manifest.widgets {
        let params = widget
            .params
            .iter()
            .map(|param| (param.id.clone(), param.default.clone()))
            .collect();
        let assets = std::iter::once(&widget.icon)
            .chain(&widget.assets)
            .map(|name| {
                let path = manifest.plugin_dir.join("assets").join(name);
                let data = std::fs::read(&path)
                    .with_context(|| format!("reading widget asset {}", path.display()))?;
                let image = super::rasterize_widget_svg(&data, 128)?;
                Ok((name.clone(), image))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let pixels = runtime.block_on(worker.render(
            super::runtime::widget_worker::WidgetRenderInput {
                widget_id: widget.id.clone(),
                width: 128,
                height: 128,
                time: 0.0,
                dt: 0.0,
                params,
                color: RgbColor {
                    r: 0,
                    g: 200,
                    b: 220,
                },
                font: font.clone(),
                audio: None,
                media_art: None,
                images: HashMap::new(),
                assets,
                preview: true,
            },
        ))?;
        anyhow::ensure!(
            pixels.chunks_exact(4).any(|pixel| pixel[3] != 0),
            "widget '{}' preview is completely transparent",
            widget.id
        );
        println!("ok - widget '{}' still preview", widget.id);
    }
    Ok(())
}

/// Build the `h` table (`assert`/`assert_eq`/`open`) a package's `test.lua` receives.
fn build_harness(
    lua: &Lua,
    manifest: &PluginManifest,
    handle: tokio::runtime::Handle,
    report: Arc<Mutex<Report>>,
) -> Result<Table> {
    let h = lua.create_table().anyhow()?;
    let data_bus = Arc::new(crate::services::data_bus::DataBus::default());

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
    let open_bus = data_bus.clone();
    h.set(
        "open",
        lua.create_function(move |lua, (_self, spec): (Table, Option<Table>)| {
            open_device(
                lua,
                &open_manifest,
                open_handle.clone(),
                open_bus.clone(),
                spec,
            )
            .map_err(mlua_err)
        })
        .anyhow()?,
    )
    .anyhow()?;

    let integration_manifest = manifest.clone();
    let integration_bus = data_bus.clone();
    h.set(
        "open_integration",
        lua.create_function(move |lua, (_self, spec): (Table, Option<Table>)| {
            open_integration(
                lua,
                &integration_manifest,
                handle.clone(),
                integration_bus.clone(),
                spec,
            )
            .map_err(mlua_err)
        })
        .anyhow()?,
    )
    .anyhow()?;

    let inject_bus = data_bus.clone();
    h.set(
        "inject_data",
        lua.create_function(
            move |_, (_self, key, value, stale_ms): (Table, String, Value, Option<u64>)| {
                let value =
                    crate::services::data_bus::DataValue::from_lua(value).map_err(mlua_err)?;
                inject_bus
                    .publish(
                        "test-host",
                        &key,
                        value,
                        crate::services::data_bus::host_policy(std::time::Duration::from_millis(
                            stale_ms.unwrap_or(60_000),
                        )),
                    )
                    .map_err(mlua_err)
            },
        )
        .anyhow()?,
    )
    .anyhow()?;
    let inspect_bus = data_bus.clone();
    h.set(
        "data_record",
        lua.create_function(move |lua, (_self, key): (Table, String)| {
            crate::services::data_bus::snapshot_to_lua(lua, &inspect_bus.read(&key))
        })
        .anyhow()?,
    )
    .anyhow()?;
    h.set(
        "invalidate_data",
        lua.create_function(move |_, (_self, key): (Table, String)| {
            data_bus
                .invalidate("test-host", &key, "invalidated")
                .map_err(mlua_err)
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
    data_bus: Arc<crate::services::data_bus::DataBus>,
    spec_table: Option<Table>,
) -> Result<Table> {
    if manifest.plugin_type != halod_shared::types::PluginKind::Integration {
        anyhow::bail!("open_integration requires an integration plugin");
    }
    let recording = Arc::new(RecordingStream::new(reads_from_spec(&spec_table)));
    #[cfg(target_os = "linux")]
    let hwmon = if manifest.transports.hwmon.is_some() {
        Some(Arc::new(
            crate::drivers::transports::hwmon::HwmonTransport::from_fixture(
                hwmon_fixtures_from_spec(&spec_table)?,
            ),
        ))
    } else {
        None
    };
    let io = if let Some(command) = &manifest.transports.command {
        PluginIo::Command(CommandExecutor::scripted(
            command.commands.clone(),
            command_results_from_spec(&spec_table)?,
        ))
    } else {
        #[cfg(target_os = "linux")]
        {
            hwmon
                .clone()
                .map(PluginIo::Hwmon)
                .unwrap_or_else(|| PluginIo::Stream {
                    transport: recording.clone() as Arc<dyn Transport>,
                    usb: None,
                })
        }
        #[cfg(not(target_os = "linux"))]
        {
            PluginIo::Stream {
                transport: recording.clone() as Arc<dyn Transport>,
                usb: None,
            }
        }
    };
    let transport_kind = manifest
        .transports
        .integration_transport_kind()
        .unwrap_or("tcp");
    let worker = PluginHandle::spawn_with_data(
        manifest.script_source.clone(),
        manifest.module_sources.clone(),
        io,
        DevMatch {
            transport: transport_kind.into(),
            ..Default::default()
        },
        manifest.permissions.clone(),
        std::collections::HashMap::new(),
        handle.clone(),
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
        super::runtime::data_api::DataRuntime::new(
            data_bus,
            manifest.plugin_id.clone(),
            &manifest.provides,
            manifest.consumes.clone(),
        ),
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
                    item.set("id", controller.id.clone())?;
                    item.set("key", controller.key.clone())?;
                    item.set("name", controller.name.clone())?;
                    item.set("serial", controller.serial.clone())?;
                    item.set("location", controller.location.clone())?;
                    item.set("extra", lua.to_value(&controller.extra)?)?;
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
        let worker = worker.clone();
        let handle = handle.clone();
        dev.set(
            "open_controller",
            lua.create_function(move |lua, (_self, wanted): (Table, u32)| {
                let controllers = handle
                    .block_on(worker.enumerate_controllers())
                    .map_err(mlua_err)?;
                let controller = controllers
                    .into_iter()
                    .find(|controller| controller.index == wanted)
                    .ok_or_else(|| mlua::Error::RuntimeError("unknown controller index".into()))?;
                let child = worker.child(DevMatch {
                    transport: transport_kind.into(),
                    index: Some(controller.index),
                    key: controller.key,
                    name: Some(controller.name),
                    extra: controller.extra,
                    ..Default::default()
                });
                integration_child_table(lua, child, handle.clone())
            })
            .anyhow()?,
        )
        .anyhow()?;
    }
    #[cfg(target_os = "linux")]
    if let Some(hwmon) = hwmon {
        dev.set(
            "hwmon_read",
            lua.create_function(move |_, (_self, key, attribute): (Table, String, String)| {
                hwmon.read(&key, &attribute).map_err(mlua_err)
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
                for (i, rec) in written.iter().enumerate() {
                    out.set(i + 1, lua.create_sequence_from(rec.data.iter().copied())?)?;
                }
                Ok(out)
            })
            .anyhow()?,
        )
        .anyhow()?;
    }
    Ok(dev)
}

fn integration_child_table(
    lua: &Lua,
    child: PluginHandle,
    handle: tokio::runtime::Handle,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    {
        let child = child.clone();
        let handle = handle.clone();
        table.set(
            "initialize",
            lua.create_function(move |lua, _self: Table| {
                let outcome = handle.block_on(child.initialize()).map_err(mlua_err)?;
                let zones = lua.create_table()?;
                for (i, zone) in outcome.zones.unwrap_or_default().iter().enumerate() {
                    let item = lua.create_table()?;
                    item.set("id", zone.id.clone())?;
                    item.set("name", zone.name.clone())?;
                    item.set("led_count", zone.led_count)?;
                    zones.set(i + 1, item)?;
                }
                Ok((outcome.ok, zones))
            })?,
        )?;
    }
    {
        let child = child.clone();
        let handle = handle.clone();
        table.set(
            "get_sensors",
            lua.create_function(move |lua, _self: Table| {
                let sensors = handle.block_on(child.get_sensors()).map_err(mlua_err)?;
                lua.to_value(&sensors)
            })?,
        )?;
    }
    {
        let child = child.clone();
        let handle = handle.clone();
        table.set(
            "get_cooling_status",
            lua.create_function(move |lua, (_self, channel_id): (Table, String)| {
                let status = handle
                    .block_on(child.cooling_status(&channel_id))
                    .map_err(mlua_err)?;
                lua.to_value(&status)
            })?,
        )?;
    }
    {
        let child = child.clone();
        let handle = handle.clone();
        table.set(
            "set_cooling_duty",
            lua.create_function(move |_, (_self, channel_id, duty): (Table, String, u8)| {
                handle
                    .block_on(child.cooling_set_duty(&channel_id, duty))
                    .map_err(mlua_err)
            })?,
        )?;
    }
    Ok(table)
}

#[cfg(target_os = "linux")]
fn hwmon_fixtures_from_spec(
    spec: &Option<Table>,
) -> Result<
    Vec<(
        String,
        String,
        std::collections::HashMap<String, String>,
        Vec<String>,
    )>,
> {
    let Some(spec) = spec else {
        return Ok(Vec::new());
    };
    let Ok(fixtures) = spec.get::<Table>("hwmon") else {
        return Ok(Vec::new());
    };
    fixtures
        .sequence_values::<Table>()
        .map(|fixture| {
            let fixture = fixture.anyhow()?;
            let stable_id = fixture.get::<String>("stable_id").anyhow()?;
            let name = fixture.get::<String>("name").anyhow()?;
            let attributes = fixture.get::<Table>("attributes").anyhow()?;
            let mut values = std::collections::HashMap::new();
            for pair in attributes.pairs::<String, String>() {
                let (attribute, value) = pair.anyhow()?;
                values.insert(attribute, value);
            }
            let writable_attributes = match fixture.get::<Table>("writable_attributes") {
                Ok(attributes) => attributes
                    .sequence_values::<String>()
                    .collect::<mlua::Result<Vec<_>>>()
                    .anyhow()?,
                Err(_) => values
                    .keys()
                    .filter(|attribute| attribute.starts_with("pwm"))
                    .cloned()
                    .collect(),
            };
            Ok((stable_id, name, values, writable_attributes))
        })
        .collect()
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

/// Read `spec.command_results`, a queue of complete `command.run` outcomes.
/// Spawn failures are intentionally not scriptable as results: like production,
/// they surface as Lua errors rather than being confused with timeouts.
fn command_results_from_spec(spec: &Option<Table>) -> Result<Vec<CommandRunResult>> {
    let Some(spec) = spec else {
        return Ok(Vec::new());
    };
    let Ok(results) = spec.get::<Table>("command_results") else {
        return Ok(Vec::new());
    };
    results
        .sequence_values::<Table>()
        .map(|result| {
            let result = result.anyhow()?;
            let stdout = result.get::<mlua::String>("stdout").anyhow()?;
            let stderr = result.get::<mlua::String>("stderr").anyhow()?;
            Ok(CommandRunResult {
                success: result.get::<bool>("success").anyhow()?,
                exit_code: result.get::<i32>("exit_code").anyhow()?,
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                timed_out: result.get::<bool>("timed_out").anyhow()?,
            })
        })
        .collect()
}

/// Build a `LuaDevice` over the plugin's first declared device, wired to a
/// fresh `RecordingStream`, and return it as a Lua table of methods.
fn open_device(
    lua: &Lua,
    manifest: &PluginManifest,
    handle: tokio::runtime::Handle,
    data_bus: Arc<crate::services::data_bus::DataBus>,
    spec_table: Option<Table>,
) -> Result<Table> {
    let spec = manifest
        .devices
        .first()
        .context("plugin declares no devices")?;
    if !matches!(spec.transport.as_str(), "hid" | "tcp" | "usb" | "command") {
        anyhow::bail!(
            "plugin-test harness only supports hid/tcp/usb/command transports today (got '{}')",
            spec.transport
        );
    }

    let companion = spec_table
        .as_ref()
        .and_then(|table| table.get::<Option<bool>>("companion").ok().flatten())
        .unwrap_or(false);
    let recording = Arc::new(RecordingStream::with_companion(
        reads_from_spec(&spec_table),
        companion,
    ));
    let usb_recording = manifest
        .transports
        .usb
        .clone()
        .map(|config| Arc::new(RecordingUsb::new(config, reads_from_spec(&spec_table))));
    let pid = spec_table
        .as_ref()
        .and_then(|table| table.get::<Option<u16>>("pid").ok().flatten())
        .or(spec.pid);
    let key = spec_table
        .as_ref()
        .and_then(|table| table.get::<Option<String>>("key").ok().flatten());
    let dev_match = DevMatch {
        transport: spec.transport.clone(),
        bus: spec.bus.clone(),
        addr: None,
        vid: spec.vid,
        pid,
        index: None,
        key,
        name: None,
        extra: Default::default(),
    };
    let transport = match spec.transport.as_str() {
        "usb" => PluginIo::Usb(usb_recording.clone().expect("USB manifest config")),
        "command" => {
            let command = manifest
                .transports
                .command
                .as_ref()
                .context("command device has no command transport configuration")?;
            PluginIo::Command(CommandExecutor::scripted(
                command.commands.clone(),
                command_results_from_spec(&spec_table)?,
            ))
        }
        _ => PluginIo::Stream {
            transport: recording.clone() as Arc<dyn Transport>,
            usb: usb_recording
                .clone()
                .map(|usb| usb as Arc<dyn UsbCollection>),
        },
    };
    let device = Arc::new(LuaDevice::new(LuaDeviceParts {
        id: "plugin-test".to_owned(),
        manifest,
        spec: Some(spec),
        notify: std::sync::Weak::new(),
        runtime: Some(Arc::new(std::sync::Mutex::new(
            super::runtime::device::RuntimeState::OpeningTransport,
        ))),
        worker: LuaDeviceWorker::Spawn(Box::new(LuaDeviceSpawnParts {
            dev_match,
            transport,
            handle: handle.clone(),
            // The harness grants every declared permission (so gated transports
            // open), uses no config, and has no `AppState` to notify.
            granted: manifest.permissions.clone(),
            config: std::collections::HashMap::new(),
            data: super::runtime::data_api::DataRuntime::new(
                data_bus,
                manifest.plugin_id.clone(),
                &manifest.provides,
                manifest.consumes.clone(),
            ),
        })),
    }));

    let dev_table = lua.create_table().anyhow()?;

    {
        let device = device.clone();
        let handle = handle.clone();
        dev_table
            .set(
                "initialize",
                lua.create_function(move |_, _self: Table| {
                    let initialized = handle
                        .block_on(crate::drivers::Device::initialize(&*device))
                        .map_err(mlua_err)?;
                    // Package tests drive status reads explicitly through
                    // `poll_sensors`; a live background ticker would race for
                    // the same scripted reports and make tests nondeterministic.
                    device.set_polling_paused(true);
                    Ok(initialized)
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
                "lcd_stream_frame",
                lua.create_function(
                    move |_,
                          (_self, bytes, width, height, _rotation, _raw, _brightness): (
                        Table,
                        Table,
                        u32,
                        u32,
                        u32,
                        bool,
                        u8,
                    )| {
                        let rgba = bytes
                            .sequence_values::<u8>()
                            .collect::<mlua::Result<Vec<_>>>()?;
                        handle
                            .block_on(crate::drivers::LcdCapability::stream_frame(
                                &*device, &rgba, width, height,
                            ))
                            .map_err(mlua_err)
                    },
                )
                .anyhow()?,
            )
            .anyhow()?;
    }

    {
        let device = device.clone();
        let handle = handle.clone();
        dev_table
            .set(
                "keyboard_layout_status",
                lua.create_function(move |lua, _self: Table| {
                    let status = handle.block_on(
                        crate::drivers::KeyboardLayoutCapability::keyboard_layout_status(&*device),
                    );
                    lua.to_value(&status)
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
                "serialize",
                lua.create_function(move |lua, _self: Table| {
                    let wire = handle.block_on(crate::drivers::Device::serialize(&*device));
                    lua.to_value(&wire)
                })
                .anyhow()?,
            )
            .anyhow()?;
    }

    {
        let device = device.clone();
        dev_table
            .set(
                "rgb_descriptor",
                lua.create_function(move |lua, _self: Table| {
                    lua.to_value(crate::drivers::RgbCapability::descriptor(&*device))
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
                "write_frame",
                lua.create_function(move |_, (_self, zone, colors): (Table, String, Table)| {
                    let colors: Vec<RgbColor> = colors
                        .sequence_values::<Table>()
                        .filter_map(|color| color.ok().map(|table| table_to_color(&table)))
                        .collect();
                    handle
                        .block_on(crate::drivers::RgbCapability::write_frame(
                            &*device, &zone, &colors,
                        ))
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
                "dpi_status",
                lua.create_function(move |lua, _self: Table| {
                    let status =
                        handle.block_on(crate::drivers::DpiCapability::dpi_status(&*device));
                    lua.to_value(&status)
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
                "key_remap_status",
                lua.create_function(move |lua, _self: Table| {
                    let status = handle.block_on(
                        crate::drivers::KeyRemapCapability::get_key_remap_status(&*device),
                    );
                    lua.to_value(&status)
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
                "connection_status",
                lua.create_function(move |lua, _self: Table| {
                    let status = handle.block_on(
                        crate::drivers::ConnectionCapability::connection_status(&*device),
                    );
                    lua.to_value(&status)
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
                "get_equalizer",
                lua.create_function(move |lua, _self: Table| {
                    let equalizer = handle
                        .block_on(crate::drivers::EqualizerCapability::get_equalizer(&*device))
                        .map_err(mlua_err)?;
                    lua.to_value(&equalizer)
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
                "set_eq_bands",
                lua.create_function(move |_, (_self, values): (Table, Table)| {
                    let values = values
                        .sequence_values::<f32>()
                        .collect::<mlua::Result<Vec<_>>>()?;
                    handle
                        .block_on(crate::drivers::EqualizerCapability::set_eq_bands(
                            &*device, &values,
                        ))
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
                "set_choice",
                lua.create_function(move |_, (_self, key, selected): (Table, String, usize)| {
                    handle
                        .block_on(crate::drivers::ChoiceCapability::set_choice(
                            &*device, &key, selected,
                        ))
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
                "enumerate_controllers",
                lua.create_function(move |lua, _self: Table| {
                    let controllers =
                        handle.block_on(crate::drivers::Controller::discover_children(&*device));
                    let out = lua.create_table()?;
                    for (i, controller) in controllers.iter().enumerate() {
                        let item = lua.create_table()?;
                        item.set("id", controller.id())?;
                        item.set("name", controller.name())?;
                        item.set("device_type", lua.to_value(&controller.wire_device_type())?)?;
                        out.set(i + 1, item)?;
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
                "set_dpi",
                lua.create_function(move |_, (_self, dpi): (Table, u16)| {
                    handle
                        .block_on(crate::drivers::DpiCapability::set_dpi_direct(&*device, dpi))
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
                "get_batteries",
                lua.create_function(move |lua, _self: Table| {
                    let batteries = handle
                        .block_on(crate::drivers::BatteryCapability::get_batteries(&*device))
                        .map_err(mlua_err)?;
                    lua.to_value(&batteries)
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
                "set_range",
                lua.create_function(move |_, (_self, key, value): (Table, String, i32)| {
                    handle
                        .block_on(crate::drivers::RangeCapability::set_range(
                            &*device, &key, value,
                        ))
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
                    for (i, rec) in written.iter().enumerate() {
                        let entry = lua.create_table()?;
                        entry.set("data", lua.create_sequence_from(rec.data.iter().copied())?)?;
                        entry.set("endpoint", rec.endpoint)?;
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
                "poll_sensors",
                lua.create_function(move |lua, _self: Table| {
                    handle.block_on(device.poll_once()).map_err(mlua_err)?;
                    let sensors = handle
                        .block_on(crate::drivers::SensorCapability::get_sensors(&*device))
                        .map_err(mlua_err)?;
                    lua.to_value(&sensors)
                })
                .anyhow()?,
            )
            .anyhow()?;
    }

    {
        let recording = recording.clone();
        dev_table
            .set(
                "queue_read",
                lua.create_function(move |_, (_self, bytes): (Table, Table)| {
                    let data = bytes
                        .sequence_values::<u8>()
                        .collect::<mlua::Result<Vec<_>>>()?;
                    recording
                        .reads
                        .lock()
                        .expect("recording poisoned")
                        .push_back(data);
                    Ok(())
                })
                .anyhow()?,
            )
            .anyhow()?;
    }

    if let Some(usb) = usb_recording.clone() {
        dev_table
            .set(
                "usb_writes",
                lua.create_function(move |lua, _self: Table| {
                    let written = usb.written.lock().expect("recording USB poisoned");
                    let out = lua.create_table()?;
                    for (i, rec) in written.iter().enumerate() {
                        let entry = lua.create_table()?;
                        entry.set("device", rec.device.clone())?;
                        entry.set("endpoint", rec.endpoint)?;
                        entry.set("request_type", rec.request_type)?;
                        entry.set("request", rec.request)?;
                        entry.set("data", lua.create_sequence_from(rec.data.iter().copied())?)?;
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
        let usb = usb_recording.clone();
        dev_table
            .set(
                "clear",
                lua.create_function(move |_, _self: Table| {
                    recording
                        .written
                        .lock()
                        .expect("recording poisoned")
                        .clear();
                    if let Some(usb) = &usb {
                        usb.written.lock().expect("recording USB poisoned").clear();
                    }
                    Ok(())
                })
                .anyhow()?,
            )
            .anyhow()?;
    }

    // Push an unsolicited report onto the event queue (as a reader thread
    // would), then drain it through `event()` — exercising the push-based
    // notification path without real hardware.
    {
        let recording = recording.clone();
        dev_table
            .set(
                "queue_event",
                lua.create_function(move |_, (_self, bytes): (Table, Table)| {
                    let data: Vec<u8> =
                        bytes.sequence_values::<u8>().collect::<mlua::Result<_>>()?;
                    recording.queue_event(data);
                    Ok(())
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
                "pump_events",
                lua.create_function(move |lua, _self: Table| {
                    let outcomes = handle.block_on(device.pump_events()).map_err(mlua_err)?;
                    let out = lua.create_table()?;
                    for (i, outcome) in outcomes.iter().enumerate() {
                        let item = lua.create_table()?;
                        item.set(
                            "pressed",
                            lua.create_sequence_from(
                                outcome.button_events.pressed.iter().copied(),
                            )?,
                        )?;
                        item.set(
                            "released",
                            lua.create_sequence_from(
                                outcome.button_events.released.iter().copied(),
                            )?,
                        )?;
                        if let Some(index) = outcome.child_index {
                            item.set("child_index", index)?;
                        }
                        item.set("state_changed", outcome.state_changed)?;
                        item.set("children_changed", outcome.children_changed)?;
                        out.set(i + 1, item)?;
                    }
                    Ok(out)
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
            "id: fixture\npermissions: [hid]\ndevices:\n  - vendor: x\n    model: y\n    match:\n      hid: { vid: 1, pid: 2 }\ntransports:\n  hid: { report_size: 64, timeout_ms: 1000 }\n",
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
    fn harness_injects_inspects_and_invalidates_shared_data() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: fixture\npermissions: [hid]\nprovides:\n  - { key: fixture.current, stale_after_ms: 60000, min_notify_interval_ms: 250 }\nconsumes: [host.sensors.*]\ndevices:\n  - vendor: x\n    model: y\n    match: { hid: { vid: 1, pid: 2 } }\ntransports:\n  hid: {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("main.lua"),
            "return { initialize = function(_) halod.publish('fixture.current', { ok = true }); return true end }",
        )
        .unwrap();
        std::fs::write(
            dir.join("test.lua"),
            "return function(h)\n  h:inject_data('host.sensors.aa', { value = 42 })\n  local dev = h:open(); h:assert(dev:initialize(), 'initialized')\n  h:assert_eq(h:data_record('fixture.current').value.ok, true, 'published record')\n  h:assert_eq(h:data_record('host.sensors.aa').value.value, 42, 'injected record')\n  h:invalidate_data('host.sensors.aa')\n  h:assert_eq(h:data_record('host.sensors.aa').status, 'unavailable', 'invalidated record')\nend",
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(run(runtime.handle().clone(), &dir).unwrap(), 0);
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
            "id: fixture\npermissions: [hid]\ndevices:\n  - vendor: x\n    model: y\n    match:\n      hid: { vid: 1, pid: 2 }\ntransports:\n  hid: { report_size: 64, timeout_ms: 1000 }\n",
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

    #[test]
    fn command_fixture_provides_structured_results_to_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("command_fixture");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: command_fixture\ntype: integration\npermissions: [command]\ntransports:\n  command:\n    commands: [fixture-tool]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("main.lua"),
            r#"return {
                enumerate_controllers = function(_dev)
                    local result = command.run("fixture-tool", { "arg" })
                    return {{
                        index = result.exit_code,
                        id = tostring(result.success),
                        key = result.stdout,
                        name = result.stderr,
                        location = tostring(result.timed_out),
                    }}
                end,
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("test.lua"),
            r#"return function(h)
                local dev = h:open_integration({ command_results = {{
                    success = false, exit_code = 23, stdout = "partial",
                    stderr = "failure detail", timed_out = true,
                }} })
                local result = dev:enumerate_controllers()[1]
                h:assert_eq(result.id, "false", "success is structured")
                h:assert_eq(result.index, 23, "exit code is structured")
                h:assert_eq(result.key, "partial", "stdout is structured")
                h:assert_eq(result.name, "failure detail", "stderr is structured")
                h:assert_eq(result.location, "true", "timeout is structured")
            end"#,
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(run(runtime.handle().clone(), &dir).unwrap(), 0);
    }
}
