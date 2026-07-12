// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-device worker thread. It owns the Lua VM + transport (both `!Send`), so
//! the `Send + Sync` `LuaDevice` talks to it over a channel. Each capability call
//! is a boxed *job* the device side builds and the worker runs against the VM,
//! answering on a `oneshot`. Transport I/O the script triggers is synchronous
//! from Lua's view; the worker drives the async transport via a captured runtime
//! handle.

use std::cell::Cell;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use mlua::{Function, Lua, LuaSerdeExt, Table, Value};
use serde::Deserialize;
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use super::lua_worker::LuaWorker;
use halod_shared::types::{
    Battery, Boolean, ButtonMapping, ConnectionStatus, Equalizer, OnboardProfiles, PairingStatus,
    Permission, RgbColor, RgbDescriptor, RgbState, RgbZone, Sensor, ZoneTopology,
};

use super::bytebuf::ByteBuf;
use super::manifest::{
    topology_from, ActionManifest, BatteryManifest, BooleanManifest, ChainManifest, ChoiceManifest,
    ConnectionManifest, DpiManifest, EqualizerManifest, FanManifest, KeyRemapManifest, LcdManifest,
    OnboardProfilesManifest, PairingManifest, RangeManifest, RgbManifest, SensorManifest,
};
use super::sandbox;
use super::transport::{AddrScope, PluginIo, RegisterBus};
use super::transport_api::TransportApi;
use crate::drivers::transports::smbus::SmBusDevice;
use crate::drivers::vendors::generic::devices::common::{linear_rgb_zone, ring_led_positions};

/// Instruction budget per capability callback. Generous — a callback
/// legitimately loops over every LED and builds whole frames — but bounds a
/// runaway `while true do end` from hanging the device's worker thread. Reset
/// before each job so it's per-call, not per-VM-lifetime.
const WORKER_INSTRUCTION_BUDGET: u64 = 50_000_000;

/// Heap cap for a device worker VM. Ample for frame/image buffers, but stops a
/// script from exhausting host RAM (e.g. `string.rep("A", 2^30)`).
const WORKER_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// One accessory the plugin's `detect_accessories` reports.
#[derive(Debug, Clone, Deserialize)]
pub struct DetectedAccessory {
    pub channel: u8,
    pub accessory: u8,
}

/// One RGB zone of a controller an integration plugin's `enumerate_controllers`
/// reports. Mirrors `manifest::AccessoryManifest`'s topology fields, but comes
/// from a live callback return rather than the static manifest table.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DetectedControllerZone {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub topology: String,
    /// Ring count for `topology = "rings"`.
    #[serde(default)]
    pub rings: u8,
    pub led_count: u32,
}

/// One controller the plugin's `enumerate_controllers` reports — becomes one
/// top-level `LuaDevice` child. Each optional capability section mirrors the
/// static manifest's (`RgbManifest`, `FanManifest`, …) but is reported live per
/// controller, so a single integration can bridge RGB *and* fans/sensors/etc.
#[derive(Debug, Clone, Deserialize)]
pub struct DetectedController {
    pub index: u32,
    pub name: String,
    /// RGB-topology shorthand: computed into an `RgbManifest` when no explicit
    /// `rgb` section is given (see `child_manifest_for`).
    #[serde(default)]
    pub zones: Vec<DetectedControllerZone>,
    #[serde(default)]
    pub rgb: Option<RgbManifest>,
    #[serde(default)]
    pub fan: Option<FanManifest>,
    #[serde(default)]
    pub sensor: Option<SensorManifest>,
    #[serde(default)]
    pub lcd: Option<LcdManifest>,
    #[serde(default)]
    pub dpi: Option<DpiManifest>,
    #[serde(default)]
    pub choice: Option<ChoiceManifest>,
    #[serde(default)]
    pub range: Option<RangeManifest>,
    #[serde(default)]
    pub boolean: Option<BooleanManifest>,
    #[serde(default)]
    pub action: Option<ActionManifest>,
    #[serde(default)]
    pub battery: Option<BatteryManifest>,
    #[serde(default)]
    pub connection: Option<ConnectionManifest>,
    #[serde(default)]
    pub equalizer: Option<EqualizerManifest>,
    #[serde(default)]
    pub pairing: Option<PairingManifest>,
    #[serde(default)]
    pub onboard_profiles: Option<OnboardProfilesManifest>,
    #[serde(default)]
    pub key_remap: Option<KeyRemapManifest>,
    #[serde(default)]
    pub chain: Option<ChainManifest>,
}

impl DetectedController {
    /// Build the `RgbDescriptor` for the `zones` shorthand, computing LED
    /// positions from each zone's declared topology + count — the same
    /// approach `initialize`-reported dynamic zones use (`build_dynamic_descriptor`).
    pub fn rgb_descriptor(&self) -> RgbDescriptor {
        let zones = self
            .zones
            .iter()
            .map(|z| {
                let topology = topology_from(&z.topology, z.rings);
                if matches!(topology, ZoneTopology::Linear) {
                    linear_rgb_zone(&z.id, &z.name, z.led_count as usize)
                } else {
                    RgbZone {
                        leds: ring_led_positions(&topology, z.led_count),
                        id: z.id.clone(),
                        name: z.name.clone(),
                        topology,
                    }
                }
            })
            .collect();
        RgbDescriptor {
            zones,
            native_effects: Vec::new(),
        }
    }
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
    /// The controller index for an integration child, so the shared script can
    /// route a capability call to the right remote controller. `None` for a
    /// directly-matched device (there's only one).
    pub index: Option<u32>,
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
/// discovered from the hardware (firmware/model, RGB zones, LCD panel, and the
/// live range/choice values read back from the device to seed the host caches).
#[derive(Debug, Default)]
pub struct InitOutcome {
    pub ok: bool,
    pub model: Option<String>,
    pub zones: Option<Vec<InitZone>>,
    pub lcd: Option<InitLcd>,
    /// Current range-control values keyed by control key, seeding the host's
    /// range cache so the UI reflects the device instead of manifest defaults.
    pub ranges: Option<HashMap<String, i32>>,
    /// Current choice selections keyed by control key (selected option index).
    pub choices: Option<HashMap<String, usize>>,
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
    #[serde(default)]
    ranges: Option<HashMap<String, i32>>,
    #[serde(default)]
    choices: Option<HashMap<String, usize>>,
}

fn default_true() -> bool {
    true
}

/// The Lua VM plus the two tables every job operates on, owned by the worker
/// thread. Jobs borrow it on that thread; it never crosses the channel — only
/// the boxed [`Job`] (which is `Send`) does, so the `!Send` VM stays put.
struct WorkerCtx {
    lua: Lua,
    /// The `dev` argument every callback receives: exposes the transport and the
    /// matched-spec identity (`dev.match`), and caches `read_status` as `dev.status`.
    dev: Table,
    /// The plugin's returned table, holding its callback functions.
    manifest: Table,
    /// Instruction counter for the runaway-guard hook; reset before each job.
    budget: Rc<Cell<u64>>,
}

/// A unit of work the device side sends to the worker thread. It runs against
/// the [`WorkerCtx`], sends its own reply, and tells the loop whether to keep
/// going (`close` returns `Break`).
type Job = Box<dyn FnOnce(&WorkerCtx) -> ControlFlow<()> + Send>;

/// Look up a plugin callback by name, or `None` if the plugin didn't declare it.
fn func(manifest: &Table, name: &str) -> Option<Function> {
    match manifest.get::<Value>(name) {
        Ok(Value::Function(f)) => Some(f),
        _ => None,
    }
}

/// A callback the operation requires; errors with a uniform message if absent.
fn required(manifest: &Table, name: &str) -> Result<Function> {
    func(manifest, name).ok_or_else(|| anyhow!("plugin has no {name}()"))
}

fn lua_err(context: &str, e: mlua::Error) -> anyhow::Error {
    anyhow!("plugin {context}: {e}")
}

/// Handle the `LuaDevice` holds. The inner [`LuaWorker`] is `Send + Sync`, so the
/// device stays `Send + Sync`. Dropping it ends the worker (channel closes).
#[derive(Clone)]
pub struct PluginHandle(LuaWorker<Job>);

impl PluginHandle {
    /// Spawn the worker thread. `source` is the full script; the worker builds
    /// its own VM from it (no live VM crosses threads). `granted` is the
    /// plugin's currently-granted permission set, and `config` its resolved
    /// config values (including decrypted secrets if `SecureStorage` is
    /// granted) — both snapshotted at spawn time.
    pub fn spawn(
        source: String,
        transport: PluginIo,
        dev_match: DevMatch,
        granted: Vec<Permission>,
        config: HashMap<String, String>,
        handle: Handle,
        zones: Vec<RgbZone>,
    ) -> Self {
        Self(LuaWorker::spawn(
            "halod-plugin",
            "plugin",
            // Generous: a capability callback may legitimately do timed transfer
            // gaps (DDC/CI) or a bounded sleep. Past this the worker is presumed
            // wedged (e.g. a `pcall`-catching runaway) and the device is dropped.
            std::time::Duration::from_secs(30),
            move || {
                build_ctx(
                    &source, transport, dev_match, &granted, &config, handle, &zones,
                )
            },
            |job: Job, ctx: &WorkerCtx| {
                ctx.budget.set(0);
                job(ctx)
            },
        ))
    }

    /// Run `f` on the worker thread and await its result. `f` gets the VM, the
    /// `dev` table and the manifest table; only its owned captures + reply
    /// sender cross the channel, so `f` must be `Send`.
    async fn run<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&WorkerCtx) -> Result<R> + Send + 'static,
    {
        self.0
            .request(|reply| {
                Box::new(move |ctx: &WorkerCtx| {
                    let _ = reply.send(f(ctx));
                    ControlFlow::Continue(())
                })
            })
            .await?
    }

    /// Run `initialize`, accepting either a bare bool or a table with dynamic
    /// device info (`{ ok, model, zones, lcd }`). A missing callback means
    /// "present, no info".
    pub async fn initialize(&self) -> Result<InitOutcome> {
        self.run(|ctx| {
            let Some(f) = func(&ctx.manifest, "initialize") else {
                return Ok(InitOutcome {
                    ok: true,
                    ..Default::default()
                });
            };
            let value: Value = f
                .call(ctx.dev.clone())
                .map_err(|e| lua_err("initialize", e))?;
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
                    let t: InitTable = ctx
                        .lua
                        .from_value(other)
                        .map_err(|e| lua_err("initialize result", e))?;
                    Ok(InitOutcome {
                        ok: t.ok,
                        model: t.model,
                        zones: t.zones,
                        lcd: t.lcd,
                        ranges: t.ranges,
                        choices: t.choices,
                    })
                }
            }
        })
        .await
    }

    pub async fn close(&self) {
        // The job returns `Break` to end the worker loop after running the
        // plugin's `close` callback; the reply confirms it finished.
        let _ = self
            .0
            .request(|reply: oneshot::Sender<()>| {
                Box::new(move |ctx: &WorkerCtx| {
                    if let Some(f) = func(&ctx.manifest, "close") {
                        if let Err(e) = f.call::<()>(ctx.dev.clone()) {
                            log::debug!("plugin close: {e}");
                        }
                    }
                    let _ = reply.send(());
                    ControlFlow::Break(())
                })
            })
            .await;
    }

    pub async fn rgb_apply(&self, state: RgbState) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "apply")?;
            let state_v = ctx
                .lua
                .to_value(&state)
                .map_err(|e| lua_err("apply arg", e))?;
            f.call::<()>((ctx.dev.clone(), state_v))
                .map_err(|e| lua_err("apply", e))
        })
        .await
    }

    pub async fn rgb_write_frame(&self, zone: &str, colors: &[RgbColor]) -> Result<()> {
        let zone = zone.to_owned();
        let colors = colors.to_vec();
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "write_frame")?;
            let colors_v = ctx
                .lua
                .to_value(&colors)
                .map_err(|e| lua_err("write_frame arg", e))?;
            f.call::<()>((ctx.dev.clone(), zone, colors_v))
                .map_err(|e| lua_err("write_frame", e))
        })
        .await
    }

    pub async fn fan_get_duty(&self) -> Result<u8> {
        self.run(|ctx| {
            let f = required(&ctx.manifest, "get_duty")?;
            f.call::<u8>(ctx.dev.clone())
                .map_err(|e| lua_err("get_duty", e))
        })
        .await
    }

    pub async fn fan_set_duty(&self, duty: u8) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "set_duty")?;
            f.call::<()>((ctx.dev.clone(), duty))
                .map_err(|e| lua_err("set_duty", e))
        })
        .await
    }

    pub async fn fan_get_rpm(&self) -> Option<u32> {
        self.run(|ctx| {
            let Some(f) = func(&ctx.manifest, "get_rpm") else {
                return Ok(None);
            };
            Ok(match f.call::<Option<u32>>(ctx.dev.clone()) {
                Ok(v) => v,
                Err(e) => {
                    log::debug!("plugin get_rpm: {e}");
                    None
                }
            })
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        self.run(|ctx| {
            let f = required(&ctx.manifest, "get_sensors")?;
            let value: Value = f
                .call(ctx.dev.clone())
                .map_err(|e| lua_err("get_sensors", e))?;
            ctx.lua
                .from_value(value)
                .map_err(|e| lua_err("get_sensors result", e))
        })
        .await
    }

    /// Run `read_status(dev)` and cache the returned table as `dev.status`.
    /// Errors (e.g. a non-blocking read with nothing pending) are logged, not
    /// fatal — the loop keeps ticking.
    pub async fn poll(&self) -> Result<()> {
        self.run(|ctx| {
            if let Some(f) = func(&ctx.manifest, "read_status") {
                match f.call::<Value>(ctx.dev.clone()) {
                    Ok(status) => {
                        if let Err(e) = ctx.dev.set("status", status) {
                            log::debug!("plugin poll: caching status failed: {e}");
                        }
                    }
                    Err(e) => log::debug!("plugin read_status: {e}"),
                }
            }
            Ok(())
        })
        .await
    }

    pub async fn detect_accessories(&self) -> Result<Vec<DetectedAccessory>> {
        self.run(|ctx| {
            let Some(f) = func(&ctx.manifest, "detect_accessories") else {
                return Ok(Vec::new());
            };
            let value: Value = f
                .call(ctx.dev.clone())
                .map_err(|e| lua_err("detect_accessories", e))?;
            ctx.lua
                .from_value(value)
                .map_err(|e| lua_err("detect_accessories result", e))
        })
        .await
    }

    pub async fn write_ext_frame(&self, channel: &str, colors: &[RgbColor]) -> Result<()> {
        let channel = channel.to_owned();
        let colors = colors.to_vec();
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "write_ext_frame")?;
            let colors_v = ctx
                .lua
                .to_value(&colors)
                .map_err(|e| lua_err("write_ext_frame arg", e))?;
            f.call::<()>((ctx.dev.clone(), channel, colors_v))
                .map_err(|e| lua_err("write_ext_frame", e))
        })
        .await
    }

    pub async fn enumerate_controllers(&self) -> Result<Vec<DetectedController>> {
        self.run(|ctx| {
            let Some(f) = func(&ctx.manifest, "enumerate_controllers") else {
                return Ok(Vec::new());
            };
            let value: Value = f
                .call(ctx.dev.clone())
                .map_err(|e| lua_err("enumerate_controllers", e))?;
            ctx.lua
                .from_value(value)
                .map_err(|e| lua_err("enumerate_controllers result", e))
        })
        .await
    }

    pub async fn write_controller_frame(
        &self,
        index: u32,
        zone: &str,
        colors: &[RgbColor],
    ) -> Result<()> {
        let zone = zone.to_owned();
        let colors = colors.to_vec();
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "write_controller_frame")?;
            let colors_v = ctx
                .lua
                .to_value(&colors)
                .map_err(|e| lua_err("write_controller_frame arg", e))?;
            f.call::<()>((ctx.dev.clone(), index, zone, colors_v))
                .map_err(|e| lua_err("write_controller_frame", e))
        })
        .await
    }

    pub async fn hub_fan_rpm(&self, channel: u8) -> Result<u32> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "fan_rpm")?;
            f.call::<u32>((ctx.dev.clone(), channel))
                .map_err(|e| lua_err("fan_rpm", e))
        })
        .await
    }

    pub async fn hub_fan_duty(&self, channel: u8) -> Result<u8> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "fan_duty")?;
            f.call::<u8>((ctx.dev.clone(), channel))
                .map_err(|e| lua_err("fan_duty", e))
        })
        .await
    }

    pub async fn hub_fan_controllable(&self, channel: u8) -> Result<bool> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "fan_controllable")?;
            f.call::<bool>((ctx.dev.clone(), channel))
                .map_err(|e| lua_err("fan_controllable", e))
        })
        .await
    }

    pub async fn hub_set_fan_duty(&self, channel: u8, duty: u8) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "set_fan_duty")?;
            f.call::<()>((ctx.dev.clone(), channel, duty))
                .map_err(|e| lua_err("set_fan_duty", e))
        })
        .await
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
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "lcd_stream_frame")?;
            let buf = ctx
                .lua
                .create_userdata(ByteBuf::from_bytes(rgba))
                .map_err(|e| lua_err("lcd_stream_frame arg", e))?;
            f.call::<()>((
                ctx.dev.clone(),
                buf,
                width,
                height,
                rotation,
                raw,
                brightness,
            ))
            .map_err(|e| lua_err("lcd_stream_frame", e))
        })
        .await
    }

    pub async fn lcd_set_image(&self, data: Vec<u8>, rotation: u32) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "set_image")?;
            let buf = ctx
                .lua
                .create_userdata(ByteBuf::from_bytes(data))
                .map_err(|e| lua_err("set_image arg", e))?;
            f.call::<()>((ctx.dev.clone(), buf, rotation))
                .map_err(|e| lua_err("set_image", e))
        })
        .await
    }

    pub async fn lcd_set_brightness(&self, brightness: u8, rotation: u32) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "lcd_set_brightness")?;
            f.call::<()>((ctx.dev.clone(), brightness, rotation))
                .map_err(|e| lua_err("lcd_set_brightness", e))
        })
        .await
    }

    pub async fn lcd_set_rotation(&self, brightness: u8, degrees: u32) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "lcd_set_rotation")?;
            f.call::<()>((ctx.dev.clone(), brightness, degrees))
                .map_err(|e| lua_err("lcd_set_rotation", e))
        })
        .await
    }

    pub async fn lcd_reset(&self) -> Result<()> {
        self.run(|ctx| {
            let f = required(&ctx.manifest, "lcd_reset")?;
            f.call::<()>(ctx.dev.clone())
                .map_err(|e| lua_err("lcd_reset", e))
        })
        .await
    }

    pub async fn dpi_set(&self, dpi: u16) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "set_dpi")?;
            f.call::<()>((ctx.dev.clone(), dpi))
                .map_err(|e| lua_err("set_dpi", e))
        })
        .await
    }

    pub async fn choice_set(&self, key: &str, selected: usize) -> Result<()> {
        let key = key.to_owned();
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "set_choice")?;
            f.call::<()>((ctx.dev.clone(), key, selected))
                .map_err(|e| lua_err("set_choice", e))
        })
        .await
    }

    pub async fn range_set(&self, key: &str, value: i32) -> Result<()> {
        let key = key.to_owned();
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "set_range")?;
            f.call::<()>((ctx.dev.clone(), key, value))
                .map_err(|e| lua_err("set_range", e))
        })
        .await
    }

    pub async fn boolean_get(&self) -> Result<Vec<Boolean>> {
        self.run(|ctx| {
            let Some(f) = func(&ctx.manifest, "get_booleans") else {
                return Ok(Vec::new());
            };
            let value: Value = f
                .call(ctx.dev.clone())
                .map_err(|e| lua_err("get_booleans", e))?;
            ctx.lua
                .from_value(value)
                .map_err(|e| lua_err("get_booleans result", e))
        })
        .await
    }

    pub async fn boolean_set(&self, key: &str, value: bool) -> Result<()> {
        let key = key.to_owned();
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "set_boolean")?;
            f.call::<()>((ctx.dev.clone(), key, value))
                .map_err(|e| lua_err("set_boolean", e))
        })
        .await
    }

    pub async fn action_trigger(&self, key: &str) -> Result<()> {
        let key = key.to_owned();
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "trigger_action")?;
            f.call::<()>((ctx.dev.clone(), key))
                .map_err(|e| lua_err("trigger_action", e))
        })
        .await
    }

    pub async fn battery_get(&self) -> Result<Vec<Battery>> {
        self.run(|ctx| {
            let Some(f) = func(&ctx.manifest, "get_batteries") else {
                return Ok(Vec::new());
            };
            let value: Value = f
                .call(ctx.dev.clone())
                .map_err(|e| lua_err("get_batteries", e))?;
            ctx.lua
                .from_value(value)
                .map_err(|e| lua_err("get_batteries result", e))
        })
        .await
    }

    pub async fn connection_get(&self) -> Result<Option<ConnectionStatus>> {
        self.run(|ctx| {
            let Some(f) = func(&ctx.manifest, "connection_status") else {
                return Ok(None);
            };
            let value: Value = f
                .call(ctx.dev.clone())
                .map_err(|e| lua_err("connection_status", e))?;
            if matches!(value, Value::Nil) {
                return Ok(None);
            }
            ctx.lua
                .from_value(value)
                .map_err(|e| lua_err("connection_status result", e))
        })
        .await
    }

    pub async fn equalizer_get(&self) -> Result<Equalizer> {
        self.run(|ctx| {
            let f = required(&ctx.manifest, "get_equalizer")?;
            let value: Value = f
                .call(ctx.dev.clone())
                .map_err(|e| lua_err("get_equalizer", e))?;
            ctx.lua
                .from_value(value)
                .map_err(|e| lua_err("get_equalizer result", e))
        })
        .await
    }

    pub async fn equalizer_set_preset(&self, preset: usize) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "set_eq_preset")?;
            f.call::<()>((ctx.dev.clone(), preset))
                .map_err(|e| lua_err("set_eq_preset", e))
        })
        .await
    }

    pub async fn equalizer_set_bands(&self, values: &[f32]) -> Result<()> {
        let values = values.to_vec();
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "set_eq_bands")?;
            f.call::<()>((ctx.dev.clone(), values))
                .map_err(|e| lua_err("set_eq_bands", e))
        })
        .await
    }

    pub async fn pairing_start(&self, timeout_secs: u8) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "start_pairing")?;
            f.call::<()>((ctx.dev.clone(), timeout_secs))
                .map_err(|e| lua_err("start_pairing", e))
        })
        .await
    }

    pub async fn pairing_stop(&self) -> Result<()> {
        self.run(|ctx| {
            let f = required(&ctx.manifest, "stop_pairing")?;
            f.call::<()>(ctx.dev.clone())
                .map_err(|e| lua_err("stop_pairing", e))
        })
        .await
    }

    pub async fn pairing_unpair(&self, slot: u8) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "unpair")?;
            f.call::<()>((ctx.dev.clone(), slot))
                .map_err(|e| lua_err("unpair", e))
        })
        .await
    }

    pub async fn pairing_status(&self) -> Result<PairingStatus> {
        self.run(|ctx| {
            let f = required(&ctx.manifest, "pairing_status")?;
            let value: Value = f
                .call(ctx.dev.clone())
                .map_err(|e| lua_err("pairing_status", e))?;
            ctx.lua
                .from_value(value)
                .map_err(|e| lua_err("pairing_status result", e))
        })
        .await
    }

    pub async fn onboard_switch_profile(&self, slot: u8) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "switch_profile")?;
            f.call::<()>((ctx.dev.clone(), slot))
                .map_err(|e| lua_err("switch_profile", e))
        })
        .await
    }

    pub async fn onboard_restore_profile(&self, slot: u8) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "restore_profile")?;
            f.call::<()>((ctx.dev.clone(), slot))
                .map_err(|e| lua_err("restore_profile", e))
        })
        .await
    }

    pub async fn onboard_set_profile_enabled(&self, slot: u8, enabled: bool) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "set_profile_enabled")?;
            f.call::<()>((ctx.dev.clone(), slot, enabled))
                .map_err(|e| lua_err("set_profile_enabled", e))
        })
        .await
    }

    pub async fn onboard_profiles_get(&self) -> Result<OnboardProfiles> {
        self.run(|ctx| {
            let f = required(&ctx.manifest, "onboard_profiles_status")?;
            let value: Value = f
                .call(ctx.dev.clone())
                .map_err(|e| lua_err("onboard_profiles_status", e))?;
            ctx.lua
                .from_value(value)
                .map_err(|e| lua_err("onboard_profiles_status result", e))
        })
        .await
    }

    pub async fn key_remap_set_mapping(&self, mapping: ButtonMapping) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "set_button_mapping")?;
            let mapping_v = ctx
                .lua
                .to_value(&mapping)
                .map_err(|e| lua_err("set_button_mapping arg", e))?;
            f.call::<()>((ctx.dev.clone(), mapping_v))
                .map_err(|e| lua_err("set_button_mapping", e))
        })
        .await
    }

    pub async fn key_remap_reset(&self, cid: u16) -> Result<()> {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, "reset_button_mapping")?;
            f.call::<()>((ctx.dev.clone(), cid))
                .map_err(|e| lua_err("reset_button_mapping", e))
        })
        .await
    }

    pub async fn key_remap_reset_all(&self) -> Result<()> {
        self.run(|ctx| {
            let f = required(&ctx.manifest, "reset_all_button_mappings")?;
            f.call::<()>(ctx.dev.clone())
                .map_err(|e| lua_err("reset_all_button_mappings", e))
        })
        .await
    }

    /// Whether the device is currently in the host mode remapping requires.
    /// Devices that don't declare `key_remap_host_mode` are assumed always active
    /// (the common case: remapping doesn't depend on a device-side mode toggle).
    pub async fn key_remap_host_mode_active(&self) -> bool {
        self.run(|ctx| {
            let Some(f) = func(&ctx.manifest, "key_remap_host_mode") else {
                return Ok(true);
            };
            Ok(match f.call::<bool>(ctx.dev.clone()) {
                Ok(v) => v,
                Err(e) => {
                    log::debug!("plugin key_remap_host_mode: {e}");
                    true
                }
            })
        })
        .await
        .unwrap_or(false)
    }
}

/// Build the worker's VM context on the worker thread. Runs once at spawn; the
/// [`LuaWorker`] loop then drives jobs against the returned [`WorkerCtx`].
fn build_ctx(
    source: &str,
    transport: PluginIo,
    dev_match: DevMatch,
    granted: &[Permission],
    config: &HashMap<String, String>,
    handle: Handle,
    zones: &[RgbZone],
) -> Result<WorkerCtx> {
    let (lua, budget) = sandbox::bootstrap_vm(
        granted,
        config,
        WORKER_MEMORY_LIMIT,
        WORKER_INSTRUCTION_BUDGET,
    )
    .map_err(|e| lua_err("sandbox setup", e))?;

    let manifest: Table = lua
        .load(source)
        .eval()
        .map_err(|e| lua_err("script evaluation", e))?;

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
    if !zones.is_empty() {
        let zones_v = lua.to_value(zones).map_err(|e| lua_err("dev.zones", e))?;
        dev.set("zones", zones_v)
            .map_err(|e| lua_err("dev.zones", e))?;
    }

    Ok(WorkerCtx {
        lua,
        dev,
        manifest,
        budget,
    })
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
    granted: &[Permission],
    handle: Handle,
) -> Result<()> {
    // `pre_scan` is one-time bus preparation before a device is even matched,
    // not general plugin logic — it gets no `halod.config` (an empty map).
    let (lua, _budget) = sandbox::bootstrap_vm(
        granted,
        &HashMap::new(),
        WORKER_MEMORY_LIMIT,
        WORKER_INSTRUCTION_BUDGET,
    )
    .map_err(|e| lua_err("sandbox setup", e))?;
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
    if let Some(index) = m.index {
        t.set("index", index)
            .map_err(|e| lua_err("match.index", e))?;
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::transports::mock::test_transport::MockTransport;

    fn stream_io() -> PluginIo {
        PluginIo::Stream {
            transport: Arc::new(MockTransport::empty()),
            bulk: None,
        }
    }

    /// Spawn a worker from inline Lua `source`, with `granted` permissions. The
    /// script never touches the transport, so a null MockTransport suffices.
    fn spawn(source: &str, granted: Vec<Permission>) -> PluginHandle {
        PluginHandle::spawn(
            source.to_owned(),
            stream_io(),
            DevMatch {
                transport: "hid".to_owned(),
                ..Default::default()
            },
            granted,
            HashMap::new(),
            Handle::current(),
            Vec::new(),
        )
    }

    fn static_state() -> RgbState {
        RgbState::Static {
            color: RgbColor { r: 1, g: 2, b: 3 },
        }
    }

    #[tokio::test]
    async fn lifecycle_initialize_apply_poll_then_close() {
        // A plugin that reports dynamic info, accepts frames, polls, and runs a
        // close hook — the full spawn → call → close round trip on one worker.
        let src = r#"
            local applied = false
            return {
                initialize = function(dev) return { ok = true, model = "M-1" } end,
                apply = function(dev, state) applied = true end,
                read_status = function(dev) return { ok = applied } end,
                close = function(dev) end,
            }
        "#;
        let h = spawn(src, vec![]);

        let init = h.initialize().await.unwrap();
        assert!(init.ok);
        assert_eq!(init.model.as_deref(), Some("M-1"));

        h.rgb_apply(static_state()).await.unwrap();
        h.poll().await.unwrap();
        h.close().await;

        // After close the worker loop has ended, so a further call fails rather
        // than hanging.
        assert!(h.rgb_apply(static_state()).await.is_err());
    }

    #[tokio::test]
    async fn runtime_sandbox_blocks_escape_hatches_inside_a_running_callback() {
        // The sandbox must hold at runtime, not just when `apply` is called in
        // isolation: a callback reaching for os/io/load fails because the global
        // is nil in the worker's VM.
        for hatch in ["os.execute('x')", "io.open('x')", "load('return 1')()"] {
            let src = format!("return {{ apply = function(dev, state) {hatch} end }}");
            let h = spawn(&src, vec![]);
            assert!(
                h.rgb_apply(static_state()).await.is_err(),
                "escape hatch '{hatch}' was reachable at runtime"
            );
        }
    }

    #[tokio::test]
    async fn permission_gated_global_is_absent_until_granted() {
        let src = r#"return {
            apply = function(dev, state) assert(os.time() > 0) end,
        }"#;

        // Without Permission::Os the `os` table is stripped, so the callback errors.
        assert!(spawn(src, vec![]).rgb_apply(static_state()).await.is_err());

        // Granting it re-injects the read-only clock, so the same callback succeeds.
        spawn(src, vec![Permission::Os])
            .rgb_apply(static_state())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn missing_required_callback_errors_uniformly() {
        // A plugin with no `apply` yields the shared "plugin has no apply()" error.
        let h = spawn("return {}", vec![]);
        let err = h.rgb_apply(static_state()).await.unwrap_err();
        assert!(
            err.to_string().contains("plugin has no apply()"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn callback_error_propagates_to_the_caller() {
        let h = spawn(
            r#"return { apply = function(dev, state) error("boom") end }"#,
            vec![],
        );
        let err = h.rgb_apply(static_state()).await.unwrap_err();
        assert!(err.to_string().contains("boom"), "unexpected error: {err}");
    }
}
