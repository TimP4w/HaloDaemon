// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-device worker thread. It owns the Lua VM + transport (both `!Send`), so
//! the `Send + Sync` `LuaDevice` talks to it over a channel. Capability calls
//! arrive as [`Call`]s and are answered on a `oneshot`. Transport I/O the script
//! triggers is synchronous from Lua's view; the worker drives the async
//! transport via a captured runtime handle.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use mlua::{Function, Lua, LuaSerdeExt, Table, Value};
use serde::Deserialize;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

use halod_shared::types::{RgbColor, RgbState, Sensor};

use super::bytebuf::ByteBuf;
use super::sandbox;
use super::transport::{AddrScope, PluginIo, RegisterBus};
use super::transport_api::TransportApi;
use crate::drivers::transports::smbus::SmBusDevice;

/// One accessory the plugin's `detect_accessories` reports.
#[derive(Debug, Clone, Deserialize)]
pub struct DetectedAccessory {
    pub channel: u8,
    pub accessory: u8,
}

/// Identifying context injected into the plugin's `dev.match` table, so a
/// callback can branch on which declared spec matched (e.g. an SMBus plugin
/// reading its own bus address).
#[derive(Debug, Clone, Default)]
pub struct DevMatch {
    pub transport: String,
    pub bus: Option<String>,
    pub addr: Option<u8>,
    /// HID product id, so a callback can branch on device variant (e.g. an LCD
    /// panel picking its native resolution). `None` for non-HID transports.
    pub pid: Option<u16>,
}

/// One RGB zone a plugin's `initialize` reports for dynamic LED counts.
#[derive(Debug, Clone, Deserialize)]
pub struct InitZone {
    pub id: String,
    pub name: String,
    #[serde(default = "default_zone_topology")]
    pub topology: String,
    pub led_count: u32,
    #[serde(default)]
    pub rings: u8,
}

fn default_zone_topology() -> String {
    "linear".to_owned()
}

/// The LCD panel an `initialize` reports (resolution is per-device, e.g. varies
/// by HID pid), converted into an `LcdDescriptor` by the device layer.
#[derive(Debug, Clone, Deserialize)]
pub struct InitLcd {
    /// `"circle"` or `"square"`.
    #[serde(default)]
    pub shape: String,
    pub width: u32,
    pub height: u32,
    /// Supported rotation angles in degrees (e.g. `{0, 90, 180, 270}`).
    #[serde(default)]
    pub rotations: Vec<u32>,
    /// Accepted upload MIME types (e.g. `"image/png"`).
    #[serde(default)]
    pub image_types: Vec<String>,
    /// The panel latches the last frame, so unchanged content isn't re-streamed.
    #[serde(default)]
    pub latches: bool,
    /// Start in the raw (uncompressed 24-bit) streaming path instead of Q565.
    #[serde(default)]
    pub raw_streaming: bool,
    /// Current panel brightness (0–100), typically read back from the device.
    #[serde(default = "default_lcd_brightness")]
    pub brightness: u8,
    /// Current rotation in degrees, typically read back from the device.
    #[serde(default)]
    pub rotation: u32,
}

fn default_lcd_brightness() -> u8 {
    80
}

/// What `initialize` returns: a bare bool, or a table with dynamic device info
/// discovered from the hardware (firmware/model, RGB zones, LCD panel).
#[derive(Debug, Default)]
pub struct InitOutcome {
    pub ok: bool,
    pub model: Option<String>,
    pub zones: Option<Vec<InitZone>>,
    pub lcd: Option<InitLcd>,
}

/// The shape `initialize` may return as a table (bool short-circuits before this).
#[derive(Debug, Deserialize)]
struct InitTable {
    #[serde(default = "default_true")]
    ok: bool,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    zones: Option<Vec<InitZone>>,
    #[serde(default)]
    lcd: Option<InitLcd>,
}

fn default_true() -> bool {
    true
}

/// A request to the worker. Each carries its own reply channel.
pub enum Call {
    Initialize(oneshot::Sender<Result<InitOutcome>>),
    Close(oneshot::Sender<()>),
    RgbApply(RgbState, oneshot::Sender<Result<()>>),
    RgbWriteFrame {
        zone: String,
        colors: Vec<RgbColor>,
        reply: oneshot::Sender<Result<()>>,
    },
    FanGetDuty(oneshot::Sender<Result<u8>>),
    FanSetDuty {
        duty: u8,
        reply: oneshot::Sender<Result<()>>,
    },
    FanGetRpm(oneshot::Sender<Option<u32>>),
    GetSensors(oneshot::Sender<Result<Vec<Sensor>>>),
    /// Run the `read_status` callback and cache the result as `dev.status`.
    Poll(oneshot::Sender<()>),
    // ── chain / children ────────────────────────────────────────────────
    DetectAccessories(oneshot::Sender<Result<Vec<DetectedAccessory>>>),
    WriteExtFrame {
        channel: String,
        colors: Vec<RgbColor>,
        reply: oneshot::Sender<Result<()>>,
    },
    HubFanRpm {
        channel: u8,
        reply: oneshot::Sender<Result<u32>>,
    },
    HubFanDuty {
        channel: u8,
        reply: oneshot::Sender<Result<u8>>,
    },
    HubFanControllable {
        channel: u8,
        reply: oneshot::Sender<Result<bool>>,
    },
    HubSetFanDuty {
        channel: u8,
        duty: u8,
        reply: oneshot::Sender<Result<()>>,
    },
    // ── LCD ──────────────────────────────────────────────────────────────
    LcdStreamFrame {
        rgba: Vec<u8>,
        width: u32,
        height: u32,
        rotation: u32,
        raw: bool,
        brightness: u8,
        reply: oneshot::Sender<Result<()>>,
    },
    LcdSetImage {
        data: Vec<u8>,
        rotation: u32,
        reply: oneshot::Sender<Result<()>>,
    },
    LcdSetBrightness {
        brightness: u8,
        rotation: u32,
        reply: oneshot::Sender<Result<()>>,
    },
    LcdSetRotation {
        brightness: u8,
        degrees: u32,
        reply: oneshot::Sender<Result<()>>,
    },
    LcdReset(oneshot::Sender<Result<()>>),
    // ── DPI / choice ─────────────────────────────────────────────────────
    DpiSet {
        dpi: u16,
        reply: oneshot::Sender<Result<()>>,
    },
    ChoiceSet {
        key: String,
        selected: usize,
        reply: oneshot::Sender<Result<()>>,
    },
}

/// Handle the `LuaDevice` holds. `UnboundedSender` is `Send + Sync`, so the
/// device stays `Send + Sync`. Dropping it ends the worker (channel closes).
#[derive(Clone)]
pub struct PluginHandle {
    tx: mpsc::UnboundedSender<Call>,
}

impl PluginHandle {
    /// Spawn the worker thread. `source` is the full script; the worker builds
    /// its own VM from it (no live VM crosses threads).
    pub fn spawn(source: String, transport: PluginIo, dev_match: DevMatch, handle: Handle) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("halod-plugin".into())
            .spawn(move || {
                if let Err(e) = worker_main(&source, transport, dev_match, handle, rx) {
                    log::error!("plugin worker stopped: {e:#}");
                }
            })
            .expect("spawn plugin worker thread");
        Self { tx }
    }

    async fn request<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Call) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .map_err(|_| anyhow!("plugin worker is gone"))?;
        rx.await
            .map_err(|_| anyhow!("plugin worker dropped the reply"))
    }

    pub async fn initialize(&self) -> Result<InitOutcome> {
        self.request(Call::Initialize).await?
    }

    pub async fn close(&self) {
        let _ = self.request(Call::Close).await;
    }

    pub async fn rgb_apply(&self, state: RgbState) -> Result<()> {
        self.request(|r| Call::RgbApply(state, r)).await?
    }

    pub async fn rgb_write_frame(&self, zone: &str, colors: &[RgbColor]) -> Result<()> {
        let zone = zone.to_owned();
        let colors = colors.to_vec();
        self.request(|reply| Call::RgbWriteFrame {
            zone,
            colors,
            reply,
        })
        .await?
    }

    pub async fn fan_get_duty(&self) -> Result<u8> {
        self.request(Call::FanGetDuty).await?
    }

    pub async fn fan_set_duty(&self, duty: u8) -> Result<()> {
        self.request(|reply| Call::FanSetDuty { duty, reply })
            .await?
    }

    pub async fn fan_get_rpm(&self) -> Option<u32> {
        self.request(Call::FanGetRpm).await.ok().flatten()
    }

    pub async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        self.request(Call::GetSensors).await?
    }

    pub async fn poll(&self) -> Result<()> {
        self.request(Call::Poll).await
    }

    pub async fn detect_accessories(&self) -> Result<Vec<DetectedAccessory>> {
        self.request(Call::DetectAccessories).await?
    }

    pub async fn write_ext_frame(&self, channel: &str, colors: &[RgbColor]) -> Result<()> {
        let channel = channel.to_owned();
        let colors = colors.to_vec();
        self.request(|reply| Call::WriteExtFrame {
            channel,
            colors,
            reply,
        })
        .await?
    }

    pub async fn hub_fan_rpm(&self, channel: u8) -> Result<u32> {
        self.request(|reply| Call::HubFanRpm { channel, reply })
            .await?
    }

    pub async fn hub_fan_duty(&self, channel: u8) -> Result<u8> {
        self.request(|reply| Call::HubFanDuty { channel, reply })
            .await?
    }

    pub async fn hub_fan_controllable(&self, channel: u8) -> Result<bool> {
        self.request(|reply| Call::HubFanControllable { channel, reply })
            .await?
    }

    pub async fn hub_set_fan_duty(&self, channel: u8, duty: u8) -> Result<()> {
        self.request(|reply| Call::HubSetFanDuty {
            channel,
            duty,
            reply,
        })
        .await?
    }

    pub async fn lcd_stream_frame(
        &self,
        rgba: Vec<u8>,
        width: u32,
        height: u32,
        rotation: u32,
        raw: bool,
        brightness: u8,
    ) -> Result<()> {
        self.request(|reply| Call::LcdStreamFrame {
            rgba,
            width,
            height,
            rotation,
            raw,
            brightness,
            reply,
        })
        .await?
    }

    pub async fn lcd_set_image(&self, data: Vec<u8>, rotation: u32) -> Result<()> {
        self.request(|reply| Call::LcdSetImage {
            data,
            rotation,
            reply,
        })
        .await?
    }

    pub async fn lcd_set_brightness(&self, brightness: u8, rotation: u32) -> Result<()> {
        self.request(|reply| Call::LcdSetBrightness {
            brightness,
            rotation,
            reply,
        })
        .await?
    }

    pub async fn lcd_set_rotation(&self, brightness: u8, degrees: u32) -> Result<()> {
        self.request(|reply| Call::LcdSetRotation {
            brightness,
            degrees,
            reply,
        })
        .await?
    }

    pub async fn lcd_reset(&self) -> Result<()> {
        self.request(Call::LcdReset).await?
    }

    pub async fn dpi_set(&self, dpi: u16) -> Result<()> {
        self.request(|reply| Call::DpiSet { dpi, reply }).await?
    }

    pub async fn choice_set(&self, key: &str, selected: usize) -> Result<()> {
        let key = key.to_owned();
        self.request(|reply| Call::ChoiceSet {
            key,
            selected,
            reply,
        })
        .await?
    }
}

/// The plugin's callback functions, looked up once by name.
struct Callbacks {
    initialize: Option<Function>,
    close: Option<Function>,
    apply: Option<Function>,
    write_frame: Option<Function>,
    get_duty: Option<Function>,
    set_duty: Option<Function>,
    get_rpm: Option<Function>,
    get_sensors: Option<Function>,
    read_status: Option<Function>,
    detect_accessories: Option<Function>,
    write_ext_frame: Option<Function>,
    fan_rpm: Option<Function>,
    fan_duty: Option<Function>,
    fan_controllable: Option<Function>,
    set_fan_duty: Option<Function>,
    lcd_stream_frame: Option<Function>,
    lcd_set_image: Option<Function>,
    lcd_set_brightness: Option<Function>,
    lcd_set_rotation: Option<Function>,
    lcd_reset: Option<Function>,
    set_dpi: Option<Function>,
    set_choice: Option<Function>,
}

impl Callbacks {
    fn load(table: &Table) -> Self {
        let f = |key: &str| match table.get::<Value>(key) {
            Ok(Value::Function(func)) => Some(func),
            _ => None,
        };
        Self {
            initialize: f("initialize"),
            close: f("close"),
            apply: f("apply"),
            write_frame: f("write_frame"),
            get_duty: f("get_duty"),
            set_duty: f("set_duty"),
            get_rpm: f("get_rpm"),
            get_sensors: f("get_sensors"),
            read_status: f("read_status"),
            detect_accessories: f("detect_accessories"),
            write_ext_frame: f("write_ext_frame"),
            fan_rpm: f("fan_rpm"),
            fan_duty: f("fan_duty"),
            fan_controllable: f("fan_controllable"),
            set_fan_duty: f("set_fan_duty"),
            lcd_stream_frame: f("lcd_stream_frame"),
            lcd_set_image: f("set_image"),
            lcd_set_brightness: f("lcd_set_brightness"),
            lcd_set_rotation: f("lcd_set_rotation"),
            lcd_reset: f("lcd_reset"),
            set_dpi: f("set_dpi"),
            set_choice: f("set_choice"),
        }
    }
}

fn lua_err(context: &str, e: mlua::Error) -> anyhow::Error {
    anyhow!("plugin {context}: {e}")
}

fn worker_main(
    source: &str,
    transport: PluginIo,
    dev_match: DevMatch,
    handle: Handle,
    mut rx: mpsc::UnboundedReceiver<Call>,
) -> Result<()> {
    let lua = Lua::new();
    sandbox::apply(&lua).map_err(|e| lua_err("sandbox setup", e))?;

    let manifest: Table = lua
        .load(source)
        .eval()
        .map_err(|e| lua_err("script evaluation", e))?;
    let cb = Callbacks::load(&manifest);

    // The `dev` argument every callback receives: exposes the transport and the
    // matched-spec identity (`dev.match`).
    let dev = lua.create_table().map_err(|e| lua_err("dev table", e))?;
    let api = TransportApi::new(transport, handle);
    let api_ud = lua
        .create_userdata(api)
        .map_err(|e| lua_err("transport userdata", e))?;
    dev.set("transport", api_ud)
        .map_err(|e| lua_err("dev.transport", e))?;
    dev.set("match", build_match_table(&lua, &dev_match)?)
        .map_err(|e| lua_err("dev.match", e))?;

    while let Some(call) = rx.blocking_recv() {
        match call {
            Call::Initialize(reply) => {
                let _ = reply.send(run_initialize(&lua, &cb, &dev));
            }
            Call::Close(reply) => {
                if let Some(f) = &cb.close {
                    if let Err(e) = f.call::<()>(dev.clone()) {
                        log::debug!("plugin close: {e}");
                    }
                }
                let _ = reply.send(());
                break;
            }
            Call::RgbApply(state, reply) => {
                let _ = reply.send(run_apply(&lua, &cb, &dev, &state));
            }
            Call::RgbWriteFrame {
                zone,
                colors,
                reply,
            } => {
                let _ = reply.send(run_write_frame(&lua, &cb, &dev, &zone, &colors));
            }
            Call::FanGetDuty(reply) => {
                let _ = reply.send(run_get_duty(&cb, &dev));
            }
            Call::FanSetDuty { duty, reply } => {
                let _ = reply.send(run_set_duty(&cb, &dev, duty));
            }
            Call::FanGetRpm(reply) => {
                let _ = reply.send(run_get_rpm(&cb, &dev));
            }
            Call::GetSensors(reply) => {
                let _ = reply.send(run_get_sensors(&lua, &cb, &dev));
            }
            Call::Poll(reply) => {
                run_poll(&cb, &dev);
                let _ = reply.send(());
            }
            Call::DetectAccessories(reply) => {
                let _ = reply.send(run_detect(&lua, &cb, &dev));
            }
            Call::WriteExtFrame {
                channel,
                colors,
                reply,
            } => {
                let _ = reply.send(run_write_ext_frame(&lua, &cb, &dev, &channel, &colors));
            }
            Call::HubFanRpm { channel, reply } => {
                let _ = reply.send(call_u32(&cb.fan_rpm, "fan_rpm", &dev, channel));
            }
            Call::HubFanDuty { channel, reply } => {
                let _ = reply.send(call_u8(&cb.fan_duty, "fan_duty", &dev, channel));
            }
            Call::HubFanControllable { channel, reply } => {
                let _ = reply.send(call_bool(
                    &cb.fan_controllable,
                    "fan_controllable",
                    &dev,
                    channel,
                ));
            }
            Call::HubSetFanDuty {
                channel,
                duty,
                reply,
            } => {
                let _ = reply.send(run_set_fan_duty(&cb, &dev, channel, duty));
            }
            Call::LcdStreamFrame {
                rgba,
                width,
                height,
                rotation,
                raw,
                brightness,
                reply,
            } => {
                let _ = reply.send(run_lcd_stream_frame(
                    &lua, &cb, &dev, rgba, width, height, rotation, raw, brightness,
                ));
            }
            Call::LcdSetImage {
                data,
                rotation,
                reply,
            } => {
                let _ = reply.send(run_lcd_set_image(&lua, &cb, &dev, data, rotation));
            }
            Call::LcdSetBrightness {
                brightness,
                rotation,
                reply,
            } => {
                let _ = reply.send(call_lcd_config(
                    &cb.lcd_set_brightness,
                    "lcd_set_brightness",
                    &dev,
                    brightness,
                    rotation,
                ));
            }
            Call::LcdSetRotation {
                brightness,
                degrees,
                reply,
            } => {
                let _ = reply.send(call_lcd_config(
                    &cb.lcd_set_rotation,
                    "lcd_set_rotation",
                    &dev,
                    brightness,
                    degrees,
                ));
            }
            Call::LcdReset(reply) => {
                let _ = reply.send(run_lcd_reset(&cb, &dev));
            }
            Call::DpiSet { dpi, reply } => {
                let _ = reply.send(run_dpi_set(&cb, &dev, dpi));
            }
            Call::ChoiceSet {
                key,
                selected,
                reply,
            } => {
                let _ = reply.send(run_choice_set(&cb, &dev, &key, selected));
            }
        }
    }
    Ok(())
}

fn run_dpi_set(cb: &Callbacks, dev: &Table, dpi: u16) -> Result<()> {
    let f = cb
        .set_dpi
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no set_dpi()"))?;
    f.call::<()>((dev.clone(), dpi))
        .map_err(|e| lua_err("set_dpi", e))
}

fn run_choice_set(cb: &Callbacks, dev: &Table, key: &str, selected: usize) -> Result<()> {
    let f = cb
        .set_choice
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no set_choice()"))?;
    f.call::<()>((dev.clone(), key.to_owned(), selected))
        .map_err(|e| lua_err("set_choice", e))
}

fn run_lcd_stream_frame(
    lua: &Lua,
    cb: &Callbacks,
    dev: &Table,
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    rotation: u32,
    raw: bool,
    brightness: u8,
) -> Result<()> {
    let f = cb
        .lcd_stream_frame
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no lcd_stream_frame()"))?;
    let buf = lua
        .create_userdata(ByteBuf::from_bytes(rgba))
        .map_err(|e| lua_err("lcd_stream_frame arg", e))?;
    f.call::<()>((dev.clone(), buf, width, height, rotation, raw, brightness))
        .map_err(|e| lua_err("lcd_stream_frame", e))
}

fn run_lcd_set_image(
    lua: &Lua,
    cb: &Callbacks,
    dev: &Table,
    data: Vec<u8>,
    rotation: u32,
) -> Result<()> {
    let f = cb
        .lcd_set_image
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no set_image()"))?;
    let buf = lua
        .create_userdata(ByteBuf::from_bytes(data))
        .map_err(|e| lua_err("set_image arg", e))?;
    f.call::<()>((dev.clone(), buf, rotation))
        .map_err(|e| lua_err("set_image", e))
}

/// Drives `lcd_set_brightness`/`lcd_set_rotation`, which both take
/// `(dev, brightness, rotation_degrees)` since the panel config carries both.
fn call_lcd_config(
    f: &Option<Function>,
    name: &str,
    dev: &Table,
    brightness: u8,
    rotation: u32,
) -> Result<()> {
    let f = f
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no {name}()"))?;
    f.call::<()>((dev.clone(), brightness, rotation))
        .map_err(|e| lua_err(name, e))
}

fn run_lcd_reset(cb: &Callbacks, dev: &Table) -> Result<()> {
    let f = cb
        .lcd_reset
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no lcd_reset()"))?;
    f.call::<()>(dev.clone())
        .map_err(|e| lua_err("lcd_reset", e))
}

fn run_detect(lua: &Lua, cb: &Callbacks, dev: &Table) -> Result<Vec<DetectedAccessory>> {
    let Some(f) = &cb.detect_accessories else {
        return Ok(Vec::new());
    };
    let value: Value = f
        .call(dev.clone())
        .map_err(|e| lua_err("detect_accessories", e))?;
    lua.from_value(value)
        .map_err(|e| lua_err("detect_accessories result", e))
}

fn run_write_ext_frame(
    lua: &Lua,
    cb: &Callbacks,
    dev: &Table,
    channel: &str,
    colors: &[RgbColor],
) -> Result<()> {
    let f = cb
        .write_ext_frame
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no write_ext_frame()"))?;
    let colors_v = lua
        .to_value(colors)
        .map_err(|e| lua_err("write_ext_frame arg", e))?;
    f.call::<()>((dev.clone(), channel.to_owned(), colors_v))
        .map_err(|e| lua_err("write_ext_frame", e))
}

fn call_u32(f: &Option<Function>, name: &str, dev: &Table, channel: u8) -> Result<u32> {
    let f = f
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no {name}()"))?;
    f.call::<u32>((dev.clone(), channel))
        .map_err(|e| lua_err(name, e))
}

fn call_u8(f: &Option<Function>, name: &str, dev: &Table, channel: u8) -> Result<u8> {
    let f = f
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no {name}()"))?;
    f.call::<u8>((dev.clone(), channel))
        .map_err(|e| lua_err(name, e))
}

fn call_bool(f: &Option<Function>, name: &str, dev: &Table, channel: u8) -> Result<bool> {
    let f = f
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no {name}()"))?;
    f.call::<bool>((dev.clone(), channel))
        .map_err(|e| lua_err(name, e))
}

fn run_set_fan_duty(cb: &Callbacks, dev: &Table, channel: u8, duty: u8) -> Result<()> {
    let f = cb
        .set_fan_duty
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no set_fan_duty()"))?;
    f.call::<()>((dev.clone(), channel, duty))
        .map_err(|e| lua_err("set_fan_duty", e))
}

/// Run `read_status(dev)` and cache the returned table as `dev.status`. Errors
/// (e.g. a non-blocking read with nothing pending) are logged, not fatal — the
/// loop keeps ticking.
fn run_poll(cb: &Callbacks, dev: &Table) {
    let Some(f) = &cb.read_status else { return };
    match f.call::<Value>(dev.clone()) {
        Ok(status) => {
            if let Err(e) = dev.set("status", status) {
                log::debug!("plugin poll: caching status failed: {e}");
            }
        }
        Err(e) => log::debug!("plugin read_status: {e}"),
    }
}

/// Run a plugin's `pre_scan(dev)` callback against a freshly opened SMBus bus,
/// before the scanner probes addresses. Used for one-time bus preparation whose
/// control flow depends on live reads (e.g. the ENE DRAM broadcast remap). The
/// transport is a register bus scoped to `scope_addrs` (declared + extras), so
/// pre_scan can never reach an address the plugin didn't declare. Runs on the
/// calling thread (a `spawn_blocking` worker), so register batches block inline.
pub fn run_pre_scan(
    source: &str,
    bus: Arc<SmBusDevice>,
    scope_addrs: Vec<u8>,
    handle: Handle,
) -> Result<()> {
    let lua = Lua::new();
    sandbox::apply(&lua).map_err(|e| lua_err("sandbox setup", e))?;
    let manifest: Table = lua
        .load(source)
        .eval()
        .map_err(|e| lua_err("script evaluation", e))?;
    let Ok(Value::Function(pre_scan)) = manifest.get::<Value>("pre_scan") else {
        return Ok(()); // no pre_scan declared: nothing to do
    };

    let io = PluginIo::Register(RegisterBus::new(bus, AddrScope::new(scope_addrs)));
    let dev = lua.create_table().map_err(|e| lua_err("dev table", e))?;
    let api_ud = lua
        .create_userdata(TransportApi::new(io, handle))
        .map_err(|e| lua_err("transport userdata", e))?;
    dev.set("transport", api_ud)
        .map_err(|e| lua_err("dev.transport", e))?;
    pre_scan.call::<()>(dev).map_err(|e| lua_err("pre_scan", e))
}

fn build_match_table(lua: &Lua, m: &DevMatch) -> Result<Table> {
    let t = lua.create_table().map_err(|e| lua_err("match table", e))?;
    t.set("transport", m.transport.clone())
        .map_err(|e| lua_err("match.transport", e))?;
    if let Some(bus) = &m.bus {
        t.set("bus", bus.clone())
            .map_err(|e| lua_err("match.bus", e))?;
    }
    if let Some(addr) = m.addr {
        t.set("addr", addr).map_err(|e| lua_err("match.addr", e))?;
    }
    if let Some(pid) = m.pid {
        t.set("pid", pid).map_err(|e| lua_err("match.pid", e))?;
    }
    Ok(t)
}

/// Run `initialize`, accepting either a bare bool or a table with dynamic device
/// info (`{ ok, model, zones }`). A missing callback means "present, no info".
fn run_initialize(lua: &Lua, cb: &Callbacks, dev: &Table) -> Result<InitOutcome> {
    let Some(f) = &cb.initialize else {
        return Ok(InitOutcome {
            ok: true,
            ..Default::default()
        });
    };
    let value: Value = f.call(dev.clone()).map_err(|e| lua_err("initialize", e))?;
    match value {
        Value::Boolean(ok) => Ok(InitOutcome {
            ok,
            ..Default::default()
        }),
        Value::Nil => Ok(InitOutcome {
            ok: true,
            ..Default::default()
        }),
        other => {
            let t: InitTable = lua
                .from_value(other)
                .map_err(|e| lua_err("initialize result", e))?;
            Ok(InitOutcome {
                ok: t.ok,
                model: t.model,
                zones: t.zones,
                lcd: t.lcd,
            })
        }
    }
}

fn run_apply(lua: &Lua, cb: &Callbacks, dev: &Table, state: &RgbState) -> Result<()> {
    let f = cb
        .apply
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no apply()"))?;
    let state_v = lua.to_value(state).map_err(|e| lua_err("apply arg", e))?;
    f.call::<()>((dev.clone(), state_v))
        .map_err(|e| lua_err("apply", e))
}

fn run_write_frame(
    lua: &Lua,
    cb: &Callbacks,
    dev: &Table,
    zone: &str,
    colors: &[RgbColor],
) -> Result<()> {
    let f = cb
        .write_frame
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no write_frame()"))?;
    let colors_v = lua
        .to_value(colors)
        .map_err(|e| lua_err("write_frame arg", e))?;
    f.call::<()>((dev.clone(), zone.to_owned(), colors_v))
        .map_err(|e| lua_err("write_frame", e))
}

fn run_get_duty(cb: &Callbacks, dev: &Table) -> Result<u8> {
    let f = cb
        .get_duty
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no get_duty()"))?;
    f.call::<u8>(dev.clone())
        .map_err(|e| lua_err("get_duty", e))
}

fn run_set_duty(cb: &Callbacks, dev: &Table, duty: u8) -> Result<()> {
    let f = cb
        .set_duty
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no set_duty()"))?;
    f.call::<()>((dev.clone(), duty))
        .map_err(|e| lua_err("set_duty", e))
}

fn run_get_rpm(cb: &Callbacks, dev: &Table) -> Option<u32> {
    let f = cb.get_rpm.as_ref()?;
    match f.call::<Option<u32>>(dev.clone()) {
        Ok(v) => v,
        Err(e) => {
            log::debug!("plugin get_rpm: {e}");
            None
        }
    }
}

fn run_get_sensors(lua: &Lua, cb: &Callbacks, dev: &Table) -> Result<Vec<Sensor>> {
    let f = cb
        .get_sensors
        .as_ref()
        .ok_or_else(|| anyhow!("plugin has no get_sensors()"))?;
    let value: Value = f.call(dev.clone()).map_err(|e| lua_err("get_sensors", e))?;
    lua.from_value(value)
        .map_err(|e| lua_err("get_sensors result", e))
}
