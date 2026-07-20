// SPDX-License-Identifier: GPL-3.0-or-later
//! `halod plugin-test <package-dir>` — drives a plugin package's declared
//! `test.lua` against a recording mock transport, so the *official plugin
//! repo*'s own CI can validate a driver change without real hardware. The
//! daemon owns the Lua worker + transport machinery here; the test *cases*
//! live in the plugin repo, one `test.lua` per package.
//!
//! Covers HID/TCP streams, SMBus registers, and scoped USB endpoint/control
//! collections against the first declared device without opening host hardware.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use mlua::{Function, Lua, LuaSerdeExt, Table, Value};

use crate::domain::device::chain::LightingDivisionAdapter;
use crate::domain::device::LightingCapability;
use crate::infrastructure::drivers::transports::usb::{UsbCollection, UsbControlResult};
use crate::infrastructure::drivers::transports::{HidTransport, Transport, TransportEvent};
use crate::infrastructure::drivers::Metered;
use crate::infrastructure::http::{
    HttpBackend, HttpPolicy, HttpRequest, HttpResponse, HttpRuntime,
};
use std::collections::HashMap;

use halod_shared::types::{
    EffectParamValue, LightingState, RgbColor, WriteRateLimit, WriteRateStatus,
};

use super::engine::device::{LuaDevice, LuaDeviceParts, LuaDeviceSpawnParts, LuaDeviceWorker};
use super::engine::transport::{AddrScope, RegisterBus};
use super::engine::transport::{CommandExecutor, CommandRunResult, PluginIo};
use super::engine::worker::{DevMatch, PluginHandle};
use super::manifest::{parse_manifest_from_dir, PluginManifest, UsbConfig};

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

#[derive(Clone)]
struct SmbusWriteRecord {
    operation: &'static str,
    addr: u8,
    cmd: Option<u8>,
    value: Option<u16>,
    data: Vec<u8>,
}

struct RecordingSmbus {
    reads: std::collections::VecDeque<u8>,
    written: Arc<Mutex<Vec<SmbusWriteRecord>>>,
}

impl crate::infrastructure::drivers::transports::smbus::SmBusSyncOps for RecordingSmbus {
    fn read_byte(&mut self, _addr: u8) -> Result<u8> {
        self.reads
            .pop_front()
            .context("no more scripted SMBus reads")
    }
    fn read_byte_data(&mut self, _addr: u8, _cmd: u8) -> Result<u8> {
        self.reads
            .pop_front()
            .context("no more scripted SMBus reads")
    }
    fn write_quick(&mut self, addr: u8) -> Result<bool> {
        self.written.lock().unwrap().push(SmbusWriteRecord {
            operation: "write_quick",
            addr,
            cmd: None,
            value: None,
            data: vec![],
        });
        Ok(true)
    }
    fn write_byte_data(&mut self, addr: u8, cmd: u8, value: u8) -> Result<()> {
        self.written.lock().unwrap().push(SmbusWriteRecord {
            operation: "write_byte_data",
            addr,
            cmd: Some(cmd),
            value: Some(value.into()),
            data: vec![],
        });
        Ok(())
    }
    fn write_word_data(&mut self, addr: u8, cmd: u8, value: u16) -> Result<()> {
        self.written.lock().unwrap().push(SmbusWriteRecord {
            operation: "write_word_data",
            addr,
            cmd: Some(cmd),
            value: Some(value),
            data: vec![],
        });
        Ok(())
    }
    fn write_block_data(&mut self, addr: u8, cmd: u8, data: &[u8]) -> Result<()> {
        self.written.lock().unwrap().push(SmbusWriteRecord {
            operation: "write_block_data",
            addr,
            cmd: Some(cmd),
            value: None,
            data: data.to_vec(),
        });
        Ok(())
    }
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
    write_error: Option<String>,
    /// Serial line-control calls (`set_dtr`/`set_rts`/`send_break`/`flush_input`)
    /// recorded in order, so a serial package's `test.lua` can drive them without
    /// erroring and a future assertion helper can inspect them.
    serial_ops: Mutex<Vec<String>>,
    rate: Metered<()>,
}

impl RecordingStream {
    fn new(reads: Vec<Vec<u8>>) -> Self {
        Self::with_options(reads, false, None)
    }

    fn with_options(reads: Vec<Vec<u8>>, companion: bool, write_error: Option<String>) -> Self {
        Self {
            written: Mutex::new(Vec::new()),
            reads: Mutex::new(reads.into()),
            deferred: Mutex::new(std::collections::VecDeque::new()),
            companion,
            write_error,
            serial_ops: Mutex::new(Vec::new()),
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
        if let Some(error) = &self.write_error {
            anyhow::bail!(error.clone());
        }
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

    fn as_serial(
        &self,
    ) -> Option<&dyn crate::infrastructure::drivers::transports::serial::SerialControl> {
        Some(self)
    }

    fn rate_status(&self) -> WriteRateStatus {
        self.rate.status()
    }

    fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.rate.set_limit(limit);
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
}

#[async_trait]
impl HidTransport for RecordingStream {
    async fn feature_exchange(&self, data: &[u8], size: usize) -> Result<Vec<u8>> {
        self.write_then_read(data, size).await
    }

    async fn send_feature_report(&self, data: &[u8]) -> Result<()> {
        if let Some(error) = &self.write_error {
            anyhow::bail!(error.clone());
        }
        self.rate.write_access(data.len()).await?;
        self.record("feature", data);
        Ok(())
    }

    async fn get_feature_report(&self, _report_id: u8, size: usize) -> Result<Vec<u8>> {
        self.read(size).await
    }

    async fn get_input_report(&self, _report_id: u8, size: usize) -> Result<Vec<u8>> {
        self.read(size).await
    }

    async fn write_companion(&self, data: &[u8]) -> Result<()> {
        if !self.companion {
            anyhow::bail!("companion collection not available on this recording stream");
        }
        if let Some(error) = &self.write_error {
            anyhow::bail!(error.clone());
        }
        self.rate.write_access(data.len()).await?;
        self.record("companion", data);
        Ok(())
    }

    async fn defer_event(&self, data: &[u8]) -> Result<()> {
        self.queue_event(data.to_vec());
        Ok(())
    }

    fn has_companion(&self) -> bool {
        self.companion
    }

    async fn read_companion(&self, size: usize) -> Result<Vec<u8>> {
        self.read(size).await
    }
}

impl crate::infrastructure::drivers::transports::serial::SerialControl for RecordingStream {
    fn set_dtr(&self, level: bool) -> Result<()> {
        self.serial_ops
            .lock()
            .expect("recording stream poisoned")
            .push(format!("set_dtr({level})"));
        Ok(())
    }

    fn set_rts(&self, level: bool) -> Result<()> {
        self.serial_ops
            .lock()
            .expect("recording stream poisoned")
            .push(format!("set_rts({level})"));
        Ok(())
    }

    fn send_break(&self, duration_ms: u64) -> Result<()> {
        self.serial_ops
            .lock()
            .expect("recording stream poisoned")
            .push(format!("send_break({duration_ms})"));
        Ok(())
    }

    fn flush_input(&self) -> Result<()> {
        self.serial_ops
            .lock()
            .expect("recording stream poisoned")
            .push("flush_input".to_owned());
        Ok(())
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
    fn primary_location(
        &self,
    ) -> Option<crate::infrastructure::drivers::transports::usb::UsbLocation> {
        None
    }
}

/// Records the `halod.http` requests a plugin makes and replays queued responses,
/// so `test.lua` can drive an http integration without touching the network.
#[derive(Default)]
struct RecordingHttp {
    requests: Mutex<Vec<HttpRequest>>,
    responses: Mutex<std::collections::VecDeque<HttpResponse>>,
}

impl HttpBackend for RecordingHttp {
    fn request(
        &self,
        req: &HttpRequest,
        _max_response_bytes: usize,
    ) -> anyhow::Result<HttpResponse> {
        self.requests
            .lock()
            .expect("recording http poisoned")
            .push(req.clone());
        Ok(self
            .responses
            .lock()
            .expect("recording http poisoned")
            .pop_front()
            .unwrap_or(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
            }))
    }
}

/// Records the datagrams a plugin sends and replays queued inbound datagrams, so
/// `test.lua` can drive a udp integration without touching the network.
#[derive(Default)]
struct RecordingUdp {
    sent: Mutex<Vec<Vec<u8>>>,
    inbound: Mutex<std::collections::VecDeque<Vec<u8>>>,
}

impl crate::infrastructure::udp::UdpBackend for RecordingUdp {
    fn send(&self, data: &[u8]) -> Result<usize> {
        self.sent
            .lock()
            .expect("recording udp poisoned")
            .push(data.to_vec());
        Ok(data.len())
    }
    fn receive(&self, _timeout: std::time::Duration, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        Ok(self
            .inbound
            .lock()
            .expect("recording udp poisoned")
            .pop_front()
            .map(|mut d| {
                d.truncate(max_bytes);
                d
            }))
    }
}

/// Build a `halod.udp` runtime over the recording backend when the manifest
/// declares a udp transport, so the harness never opens a real socket.
fn recording_udp_runtime(
    manifest: &PluginManifest,
    recording: &Option<Arc<RecordingUdp>>,
) -> Option<crate::infrastructure::udp::UdpRuntime> {
    let udp = manifest.transports.udp.as_ref()?;
    let recording = recording.clone()?;
    Some(crate::infrastructure::udp::UdpRuntime::new(
        recording,
        udp.max_datagram_bytes,
        std::time::Duration::from_millis(udp.recv_timeout_ms),
    ))
}

/// Attach `dev:queue_udp_datagram(bytes)` and `dev:udp_sent()` so a udp
/// package's `test.lua` can inject inbound datagrams and assert what was sent.
fn attach_udp_methods(lua: &Lua, dev: &Table, recording: Option<Arc<RecordingUdp>>) -> Result<()> {
    let Some(recording) = recording else {
        return Ok(());
    };
    let inbound = recording.clone();
    dev.set(
        "queue_udp_datagram",
        lua.create_function(move |_, (_self, data): (Table, mlua::LuaString)| {
            inbound
                .inbound
                .lock()
                .expect("recording udp poisoned")
                .push_back(data.as_bytes().to_vec());
            Ok(())
        })
        .anyhow()?,
    )
    .anyhow()?;
    let sent = recording;
    dev.set(
        "udp_sent",
        lua.create_function(move |lua, _self: Table| {
            let out = lua.create_table()?;
            for (i, datagram) in sent
                .sent
                .lock()
                .expect("recording udp poisoned")
                .iter()
                .enumerate()
            {
                out.set(i + 1, lua.create_string(datagram)?)?;
            }
            Ok(out)
        })
        .anyhow()?,
    )
    .anyhow()?;
    Ok(())
}

/// Build a `halod.http` runtime over the recording backend when the manifest
/// declares an http transport, so the harness never opens a real socket.
fn recording_http_runtime(
    manifest: &PluginManifest,
    config: &crate::domain::plugin::ResolvedConfig,
    recording: &Option<Arc<RecordingHttp>>,
) -> Option<HttpRuntime> {
    let http = manifest.transports.http.as_ref()?;
    let recording = recording.clone()?;
    let host = http.host_key.as_ref().and_then(|key| {
        config
            .get(key)
            .map(crate::domain::plugin::ResolvedConfigValue::to_config_string)
    });
    let port = http.port_key.as_ref().and_then(|key| {
        config
            .get(key)
            .map(crate::domain::plugin::ResolvedConfigValue::to_config_string)
    });
    let identity = http
        .tls
        .as_ref()
        .and_then(|tls| tls.verify_identity.as_ref())
        .and_then(|key| config.get(key))
        .map(crate::domain::plugin::ResolvedConfigValue::to_config_string)
        .filter(|value| !value.trim().is_empty())
        // The recording backend performs no TLS. Falling back to the resolved
        // host keeps its captured URL stable when an identity has no fixture.
        .or_else(|| host.clone());
    Some(HttpRuntime::new(
        HttpPolicy::from_config(http, host.as_deref(), port.as_deref(), identity.as_deref()),
        recording,
        http.max_concurrency,
    ))
}

/// A resolved config for the harness worker. Secure fields receive a non-empty
/// test-only placeholder when their manifest default is blank, so a plugin can
/// exercise its authenticated path without exposing or persisting a secret.
fn default_resolved_config(manifest: &PluginManifest) -> crate::domain::plugin::ResolvedConfig {
    manifest
        .config_fields()
        .iter()
        .filter_map(|field| {
            if field.secure && field.default.is_empty() {
                Some((
                    field.key.clone(),
                    crate::domain::plugin::ResolvedConfigValue::String("plugin-test-secret".into()),
                ))
            } else {
                super::resolved_config_value(field.kind, &field.default)
                    .map(|value| (field.key.clone(), value))
            }
        })
        .collect()
}

/// Attach `dev:queue_http_response{…}` and `dev:http_requests()` to a device the
/// harness opened, when it has a recording http backend.
fn attach_http_methods(
    lua: &Lua,
    dev: &Table,
    recording: Option<Arc<RecordingHttp>>,
) -> Result<()> {
    let Some(recording) = recording else {
        return Ok(());
    };
    let queue = recording.clone();
    dev.set(
        "queue_http_response",
        lua.create_function(move |_, (_self, args): (Table, Table)| {
            let status: u16 = args.get::<Option<u16>>("status")?.unwrap_or(200);
            let body = match args.get::<Value>("body")? {
                Value::Nil => Vec::new(),
                Value::String(s) => s.as_bytes().to_vec(),
                _ => {
                    return Err(mlua::Error::RuntimeError(
                        "queue_http_response 'body' must be a string".into(),
                    ))
                }
            };
            queue
                .responses
                .lock()
                .expect("recording http poisoned")
                .push_back(HttpResponse {
                    status,
                    headers: Vec::new(),
                    body,
                });
            Ok(())
        })
        .anyhow()?,
    )
    .anyhow()?;
    let inspect = recording;
    dev.set(
        "http_requests",
        lua.create_function(move |lua, _self: Table| {
            let requests = inspect.requests.lock().expect("recording http poisoned");
            let out = lua.create_table()?;
            for (i, req) in requests.iter().enumerate() {
                let item = lua.create_table()?;
                item.set("method", req.method.clone())?;
                item.set("origin", req.origin.clone())?;
                item.set("path", req.path.clone())?;
                item.set("body", lua.create_string(&req.body)?)?;
                out.set(i + 1, item)?;
            }
            Ok(out)
        })
        .anyhow()?,
    )
    .anyhow()?;
    Ok(())
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
    let halod = lua.create_table().anyhow()?;
    lua.globals().set("halod", halod).anyhow()?;
    super::engine::sandbox::install_package_modules(&lua, &manifest.module_sources)
        .anyhow()
        .context("installing package-local modules for test.lua")?;
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
    let worker = super::engine::widget_worker::PluginWidgetHandle::spawn_with_data(
        manifest.script_source.clone(),
        manifest.module_sources.clone(),
        ids,
        manifest.permissions.clone(),
        HashMap::new(),
        super::engine::data_api::DataRuntime::new(
            Arc::new(crate::application::bus::data_bus::DataBus::default()),
            manifest.plugin_id.clone(),
            &manifest.provides,
            manifest.consumes.clone(),
        ),
    );
    let font = ab_glyph::FontArc::try_from_slice(include_bytes!(
        "../../../../assets/fonts/NotoSans-Regular.ttf"
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
            super::engine::widget_worker::WidgetRenderInput {
                widget_id: widget.id.clone(),
                width: 128,
                height: 128,
                time: 0.0,
                dt: 0.0,
                locale: "en".to_owned(),
                translations: Default::default(),
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
    let data_bus = Arc::new(crate::application::bus::data_bus::DataBus::default());

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
                let value = crate::application::bus::data_bus::DataValue::from_lua(value)
                    .map_err(mlua_err)?;
                inject_bus
                    .publish(
                        "test-host",
                        &key,
                        value,
                        crate::application::bus::data_bus::host_policy(
                            std::time::Duration::from_millis(stale_ms.unwrap_or(60_000)),
                        ),
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
            crate::application::bus::data_bus::snapshot_to_lua(lua, &inspect_bus.read(&key))
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
    data_bus: Arc<crate::application::bus::data_bus::DataBus>,
    spec_table: Option<Table>,
) -> Result<Table> {
    if manifest.plugin_type != halod_shared::types::PluginKind::Integration {
        anyhow::bail!("open_integration requires an integration plugin");
    }
    let recording = Arc::new(RecordingStream::new(reads_from_spec(&spec_table)));
    #[cfg(target_os = "linux")]
    let hwmon = if manifest.transports.hwmon.is_some() {
        Some(Arc::new(
            crate::infrastructure::drivers::transports::hwmon::HwmonTransport::from_fixture(
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
    let http_recording = manifest
        .transports
        .http
        .as_ref()
        .map(|_| Arc::new(RecordingHttp::default()));
    let udp_recording = manifest
        .transports
        .udp
        .as_ref()
        .map(|_| Arc::new(RecordingUdp::default()));
    let resolved_config = default_resolved_config(manifest);
    let http = recording_http_runtime(manifest, &resolved_config, &http_recording);
    let udp = recording_udp_runtime(manifest, &udp_recording);
    let worker = PluginHandle::spawn_with_data(
        manifest.script_source.clone(),
        manifest.module_sources.clone(),
        io,
        DevMatch {
            transport: transport_kind.into(),
            ..Default::default()
        },
        manifest.permissions.clone(),
        resolved_config,
        handle.clone(),
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
        super::engine::data_api::DataRuntime::new(
            data_bus,
            manifest.plugin_id.clone(),
            &manifest.provides,
            manifest.consumes.clone(),
        ),
        http,
        udp,
    );
    let dev = lua.create_table().anyhow()?;
    attach_http_methods(lua, &dev, http_recording)?;
    attach_udp_methods(lua, &dev, udp_recording)?;
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
                    let channels = lua.create_table()?;
                    for (z, zone) in controller.channels.iter().enumerate() {
                        let zone_t = lua.create_table()?;
                        zone_t.set("id", zone.id.clone())?;
                        zone_t.set("name", zone.name.clone())?;
                        zone_t.set("led_count", zone.led_count)?;
                        channels.set(z + 1, zone_t)?;
                    }
                    item.set("channels", channels)?;
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
                let channels = lua.create_table()?;
                for (i, zone) in outcome.channels.unwrap_or_default().iter().enumerate() {
                    let item = lua.create_table()?;
                    item.set("id", zone.id.clone())?;
                    item.set("name", zone.name.clone())?;
                    item.set("led_count", zone.led_count)?;
                    item.set("topology", zone.topology.clone())?;
                    let leds = lua.create_table()?;
                    for (l, led) in zone.leds.iter().enumerate() {
                        let led_t = lua.create_table()?;
                        led_t.set("id", led.id)?;
                        led_t.set("x", led.x)?;
                        led_t.set("y", led.y)?;
                        leds.set(l + 1, led_t)?;
                    }
                    item.set("leds", leds)?;
                    channels.set(i + 1, item)?;
                }
                Ok((outcome.ok, channels))
            })?,
        )?;
    }
    {
        let child = child.clone();
        let handle = handle.clone();
        table.set(
            "write_frame",
            lua.create_function(move |_, (_self, channel, bytes): (Table, String, Table)| {
                let bytes: Vec<u8> = bytes
                    .sequence_values::<u8>()
                    .filter_map(Result::ok)
                    .collect();
                handle
                    .block_on(child.write_lighting_frame(&channel, &bytes))
                    .map_err(mlua_err)
            })?,
        )?;
    }
    {
        let child = child.clone();
        let handle = handle.clone();
        table.set(
            "apply",
            lua.create_function(move |_, (_self, state): (Table, Table)| {
                let rgb_state = table_to_rgb_state(&state).map_err(mlua_err)?;
                handle
                    .block_on(child.rgb_apply(rgb_state))
                    .map_err(mlua_err)
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

fn smbus_reads_from_spec(spec: &Option<Table>) -> Vec<u8> {
    spec.as_ref()
        .and_then(|table| table.get::<Table>("smbus_reads").ok())
        .map(|reads| {
            reads
                .sequence_values::<u8>()
                .filter_map(Result::ok)
                .collect()
        })
        .unwrap_or_default()
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
            let stdout = result.get::<mlua::LuaString>("stdout").anyhow()?;
            let stderr = result.get::<mlua::LuaString>("stderr").anyhow()?;
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
    data_bus: Arc<crate::application::bus::data_bus::DataBus>,
    spec_table: Option<Table>,
) -> Result<Table> {
    let spec = manifest
        .devices
        .first()
        .context("plugin declares no devices")?;
    if !matches!(
        spec.transport.as_str(),
        "hid" | "tcp" | "usb" | "command" | "smbus"
    ) {
        anyhow::bail!(
            "plugin-test harness only supports hid/tcp/usb/command/smbus transports today (got '{}')",
            spec.transport
        );
    }

    let companion = spec_table
        .as_ref()
        .and_then(|table| table.get::<Option<bool>>("companion").ok().flatten())
        .unwrap_or(false);
    let write_error = spec_table
        .as_ref()
        .and_then(|table| table.get::<Option<String>>("write_error").ok().flatten());
    let recording = Arc::new(RecordingStream::with_options(
        reads_from_spec(&spec_table),
        companion,
        write_error,
    ));
    let smbus_written = Arc::new(Mutex::new(Vec::new()));
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
    let smbus_addr = spec
        .addresses
        .as_ref()
        .and_then(|addresses| addresses.first())
        .copied();
    let dev_match = DevMatch {
        transport: spec.transport.clone(),
        bus: spec.bus.clone(),
        addr: smbus_addr,
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
        "smbus" => {
            let ops = RecordingSmbus {
                reads: smbus_reads_from_spec(&spec_table).into(),
                written: smbus_written.clone(),
            };
            let bus = crate::infrastructure::drivers::transports::smbus::SmBusDevice::recording(
                Box::new(ops),
            );
            PluginIo::Register(RegisterBus::new(
                bus,
                AddrScope::single(smbus_addr.context("SMBus fixture has no address")?),
            ))
        }
        _ => PluginIo::Stream {
            transport: recording.clone() as Arc<dyn Transport>,
            usb: usb_recording
                .clone()
                .map(|usb| usb as Arc<dyn UsbCollection>),
        },
    };
    let device = Arc::new_cyclic(|weak| {
        let mut device = LuaDevice::new(LuaDeviceParts {
            id: "plugin-test".to_owned(),
            manifest,
            spec: Some(spec),
            notify: std::sync::Weak::new(),
            runtime: Some(Arc::new(std::sync::Mutex::new(
                super::engine::device::RuntimeState::OpeningTransport,
            ))),
            worker: LuaDeviceWorker::Spawn(Box::new(LuaDeviceSpawnParts {
                dev_match,
                transport,
                handle: handle.clone(),
                // The harness grants every declared permission (so gated transports
                // open), uses no config, and has no `AppState` to notify.
                granted: manifest.permissions.clone(),
                config: std::collections::HashMap::new(),
                data: super::engine::data_api::DataRuntime::new(
                    data_bus,
                    manifest.plugin_id.clone(),
                    &manifest.provides,
                    manifest.consumes.clone(),
                ),
            })),
        });
        device.set_self_ref(weak.clone());
        device
    });
    if manifest
        .capabilities
        .iter()
        .any(|capability| capability == "lighting_division")
    {
        let adapter: Arc<dyn crate::domain::device::chain::LightingDivisionAdapter> =
            device.clone();
        let host = crate::domain::device::chain::LightingDivisionHost::new(adapter);
        device.install_chain_host(host);
    }

    let dev_table = lua.create_table().anyhow()?;

    {
        let device = device.clone();
        let handle = handle.clone();
        dev_table
            .set(
                "initialize",
                lua.create_function(move |_, _self: Table| {
                    let initialized = handle
                        .block_on(crate::domain::device::Device::initialize(&*device))
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
                            .block_on(crate::domain::device::LcdCapability::stream_frame(
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
                        crate::domain::device::KeyboardLayoutCapability::keyboard_layout_status(
                            &*device,
                        ),
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
                    let wire = handle.block_on(crate::domain::device::Device::serialize(&*device));
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
                "lighting_descriptor",
                lua.create_function(move |lua, _self: Table| {
                    lua.to_value(crate::domain::device::LightingCapability::descriptor(
                        &*device,
                    ))
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
                "set_cooling_duty",
                lua.create_function(move |_, (_self, channel, duty): (Table, String, u8)| {
                    handle
                        .block_on(crate::domain::device::CoolingCapability::set_cooling_duty(
                            &*device, &channel, duty,
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
                "write_frame",
                lua.create_function(move |_, (_self, channel, bytes): (Table, String, Table)| {
                    let bytes: Vec<u8> = bytes
                        .sequence_values::<u8>()
                        .filter_map(Result::ok)
                        .collect();
                    handle
                        .block_on(crate::domain::device::LightingCapability::write_frame(
                            &*device, &channel, &bytes,
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
                        handle.block_on(crate::domain::device::DpiCapability::dpi_status(&*device));
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
                        crate::domain::device::KeyRemapCapability::get_key_remap_status(&*device),
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
                        crate::domain::device::ConnectionCapability::connection_status(&*device),
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
                        .block_on(crate::domain::device::EqualizerCapability::get_equalizer(
                            &*device,
                        ))
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
                        .block_on(crate::domain::device::EqualizerCapability::set_eq_bands(
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
                "set_boolean",
                lua.create_function(move |_, (_self, key, value): (Table, String, bool)| {
                    handle
                        .block_on(crate::domain::device::BooleanCapability::set_boolean(
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
                "set_button_mapping",
                lua.create_function(move |lua, (_self, mapping): (Table, Table)| {
                    let mapping = lua.from_value(Value::Table(mapping))?;
                    handle
                        .block_on(
                            crate::domain::device::KeyRemapCapability::set_button_mapping(
                                &*device, mapping,
                            ),
                        )
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
                        .block_on(crate::domain::device::ChoiceCapability::set_choice(
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
                    let controllers = handle.block_on(
                        crate::domain::device::Controller::discover_children(&*device),
                    );
                    let out = lua.create_table()?;
                    for (i, controller) in controllers.iter().enumerate() {
                        let item = lua.create_table()?;
                        item.set("id", controller.id())?;
                        item.set("name", controller.name())?;
                        item.set("device_type", lua.to_value(&controller.wire_device_type())?)?;
                        item.set("has_cooling", controller.as_cooling().is_some())?;
                        item.set("has_lighting", controller.as_lighting().is_some())?;
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
                        .block_on(crate::domain::device::DpiCapability::set_dpi_direct(
                            &*device, dpi,
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
                "get_batteries",
                lua.create_function(move |lua, _self: Table| {
                    let batteries = handle
                        .block_on(crate::domain::device::BatteryCapability::get_batteries(
                            &*device,
                        ))
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
                        .block_on(crate::domain::device::RangeCapability::set_range(
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
        let device = device.clone();
        dev_table
            .set(
                "cached_cooling",
                lua.create_function(move |lua, _self: Table| {
                    lua.to_value(
                        &crate::domain::device::CoolingCapability::cached_cooling_status(&*device),
                    )
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
        let written = smbus_written.clone();
        dev_table
            .set(
                "smbus_writes",
                lua.create_function(move |lua, _self: Table| {
                    let records = written.lock().expect("recording SMBus poisoned");
                    let out = lua.create_table()?;
                    for (i, record) in records.iter().enumerate() {
                        let entry = lua.create_table()?;
                        entry.set("operation", record.operation)?;
                        entry.set("addr", record.addr)?;
                        entry.set("cmd", record.cmd)?;
                        entry.set("value", record.value)?;
                        entry.set(
                            "data",
                            lua.create_sequence_from(record.data.iter().copied())?,
                        )?;
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
                        .block_on(crate::domain::device::SensorCapability::get_sensors(
                            &*device,
                        ))
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
                "write_divided_frame",
                lua.create_function(move |_, (_self, channel, bytes): (Table, String, Table)| {
                    let composed: Vec<u8> = bytes
                        .sequence_values::<u8>()
                        .filter_map(Result::ok)
                        .collect();
                    handle
                        .block_on(device.write_divided_frame(&channel, &composed))
                        .map_err(mlua_err)
                })
                .anyhow()?,
            )
            .anyhow()?;
    }

    {
        let recording = recording.clone();
        let usb = usb_recording.clone();
        let smbus_written = smbus_written.clone();
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
                    smbus_written
                        .lock()
                        .expect("recording SMBus poisoned")
                        .clear();
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

fn table_to_rgb_state(state: &Table) -> Result<LightingState> {
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
            Ok(LightingState::Static {
                color: table_to_color(&color),
            })
        }
        "per_led" => {
            let zones_t: Table = state
                .get("channels")
                .anyhow()
                .context("per_led state needs a channels table")?;
            let mut channels = HashMap::new();
            for pair in zones_t.pairs::<String, Table>() {
                let (channel_id, leds_t) = pair.anyhow()?;
                let mut leds = HashMap::new();
                for led in leds_t.pairs::<String, Table>() {
                    let (led_id, c) = led.anyhow()?;
                    leds.insert(led_id, table_to_color(&c));
                }
                channels.insert(channel_id, leds);
            }
            Ok(LightingState::PerLed { channels })
        }
        "native_effect" | "direct_effect" => {
            let id: String = state
                .get("id")
                .anyhow()
                .with_context(|| format!("{mode} state needs an id"))?;
            let mut params = HashMap::new();
            if let Ok(params_t) = state.get::<Table>("params") {
                for pair in params_t.pairs::<String, Value>() {
                    let (key, v) = pair.anyhow()?;
                    if let Some(pv) = value_to_param(&v) {
                        params.insert(key, pv);
                    }
                }
            }
            if mode == "native_effect" {
                Ok(LightingState::NativeEffect { id, params })
            } else {
                Ok(LightingState::DirectEffect { id, params })
            }
        }
        "engine" => Ok(LightingState::Engine),
        other => anyhow::bail!("plugin-test harness does not support state.mode = '{other}' yet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_to_rgb_state_supports_direct_effects() {
        let lua = Lua::new();
        let state = lua.create_table().unwrap();
        state.set("mode", "direct_effect").unwrap();
        state.set("id", "breathing").unwrap();
        let params = lua.create_table().unwrap();
        params.set("speed", 2.5).unwrap();
        state.set("params", params).unwrap();

        let parsed = table_to_rgb_state(&state).unwrap();
        assert!(matches!(
            parsed,
            LightingState::DirectEffect { id, params }
                if id == "breathing"
                    && params.get("speed") == Some(&EffectParamValue::Float(2.5))
        ));
    }

    #[test]
    fn table_to_rgb_state_supports_engine_mode() {
        let lua = Lua::new();
        let state = lua.create_table().unwrap();
        state.set("mode", "engine").unwrap();

        assert!(matches!(
            table_to_rgb_state(&state).unwrap(),
            LightingState::Engine
        ));
    }

    #[tokio::test]
    async fn recording_stream_injects_write_failures_without_recording_data() {
        let stream =
            RecordingStream::with_options(Vec::new(), true, Some("simulated write failure".into()));

        assert!(Transport::write(&stream, &[1]).await.is_err());
        assert!(HidTransport::send_feature_report(&stream, &[2])
            .await
            .is_err());
        assert!(HidTransport::write_companion(&stream, &[3]).await.is_err());
        assert!(stream.written.lock().unwrap().is_empty());
    }

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
            "id: command_fixture\ntype: integration\npermissions: [command]\ntransports:\n  command:\n    commands: [nvidia-smi]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("main.lua"),
            r#"return {
                enumerate_controllers = function(_dev)
                    local result = command.run("nvidia-smi", { "arg" })
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
