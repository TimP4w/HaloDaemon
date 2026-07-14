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

use anyhow::{anyhow, bail, Result};
use mlua::{Function, Lua, LuaSerdeExt, Table, Value};
use serde::Deserialize;
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use super::lua_worker::LuaWorker;
use halod_shared::types::{
    Battery, Boolean, ButtonMapping, ConnectionStatus, Equalizer, NativeEffect, OnboardProfiles,
    PairingStatus, Permission, RgbColor, RgbDescriptor, RgbState, RgbZone, Sensor, ZoneTopology,
};

use super::bytebuf::ByteBuf;
use super::ffi::to_lua_err;
use super::manifest::{
    check_lcd_dims, check_led_count, check_zone_count, topology_from, AccessoryManifest, ActionDef,
    ActionManifest, BatteryManifest, BooleanDef, BooleanManifest, ChainManifest, ChoiceDef,
    ChoiceManifest, ConnectionManifest, DpiManifest, EqualizerManifest, FanManifest,
    KeyRemapManifest, LcdManifest, OnboardProfilesManifest, PairingManifest, RangeDef,
    RangeManifest, RgbManifest, SensorManifest,
};
use super::sandbox;
use super::transport::{AddrScope, CommandExecutor, PluginIo, RegisterBus};
use super::transport_api::TransportApi;
use crate::drivers::transports::smbus::SmBusDevice;
use crate::drivers::vendors::generic::devices::common::{linear_rgb_zone, ring_led_positions};

use super::{PLUGIN_INSTRUCTION_BUDGET, PLUGIN_VM_MEMORY_BYTES};

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
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
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
    /// USB vendor id, so device-scoped host APIs (e.g. `dev.audio`) can bind to
    /// the right hardware. `None` for non-USB transports.
    pub vid: Option<u16>,
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

/// One chainable output channel a plugin's `initialize` reports for dynamic chain
/// discovery (e.g. an ARGB header whose count/capacity is only known after reading
/// the device's config table). Mirrors the static `manifest::ChannelManifest`.
#[derive(Debug, Clone, Deserialize)]
pub struct InitChainChannel {
    pub id: String,
    pub name: String,
    pub max_leds: u32,
}

/// Runtime control descriptors. They replace the removed static manifest
/// control sections and are validated independently from the rest of an
/// initialize result.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InitControls {
    #[serde(default)]
    pub choices: Vec<ChoiceDef>,
    #[serde(default)]
    pub ranges: Vec<RangeDef>,
    #[serde(default)]
    pub booleans: Vec<BooleanDef>,
    #[serde(default)]
    pub actions: Vec<ActionDef>,
}

/// What `initialize` returns: a bare bool, or a table with dynamic device info
/// discovered from the hardware (firmware/model, RGB zones, LCD panel, and the
/// live range/choice values read back from the device to seed the host caches).
#[derive(Debug, Default)]
pub struct InitOutcome {
    pub ok: bool,
    pub model: Option<String>,
    pub zones: Option<Vec<InitZone>>,
    /// Device-native RGB effect metadata. This is runtime information because
    /// supported effects may differ by firmware and controller revision.
    pub native_effects: Option<Vec<NativeEffect>>,
    pub lcd: Option<InitLcd>,
    /// Chainable channels discovered at runtime (e.g. ARGB headers whose capacity
    /// is read from the device's config table), for a plugin that declares a
    /// `chain` capability but reports its channels dynamically rather than statically.
    pub chain: Option<Vec<InitChainChannel>>,
    pub accessories: Option<Vec<AccessoryManifest>>,
    pub controls: Option<InitControls>,
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
    native_effects: Option<Vec<NativeEffect>>,
    #[serde(default)]
    lcd: Option<InitLcd>,
    #[serde(default)]
    chain: Option<Vec<InitChainChannel>>,
    #[serde(default)]
    accessories: Option<Vec<AccessoryManifest>>,
    #[serde(default)]
    controls: Option<InitControls>,
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
    /// Integration-controller index. When present, RGB calls use the
    /// controller-aware callbacks instead of ordinary device-plugin callbacks.
    controller_index: Option<u32>,
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
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        source: String,
        transport: PluginIo,
        dev_match: DevMatch,
        granted: Vec<Permission>,
        config: HashMap<String, String>,
        handle: Handle,
        zones: Vec<RgbZone>,
        audio_registry: super::audio_api::SinkRegistry,
    ) -> Self {
        let worker = LuaWorker::spawn(
            "halod-plugin",
            "plugin",
            // Generous: a capability callback may legitimately do timed transfer
            // gaps (DDC/CI) or a bounded sleep. Past this the worker is presumed
            // wedged (e.g. a `pcall`-catching runaway) and the device is dropped.
            std::time::Duration::from_secs(30),
            move || {
                build_ctx(
                    &source,
                    transport,
                    dev_match,
                    &granted,
                    &config,
                    handle,
                    &zones,
                    audio_registry,
                )
            },
            |job: Job, ctx: &WorkerCtx| {
                ctx.budget.set(0);
                super::sandbox::set_call_deadline(&ctx.lua, std::time::Duration::from_secs(30));
                job(ctx)
            },
        )
        .unwrap_or_else(|e| {
            log::error!("plugin worker not started: {e:#}");
            LuaWorker::dead("plugin")
        });
        Self(worker)
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

    /// Call a **required** callback `name(dev, args…)` and return its result.
    /// `args` is everything after the implicit `dev` (a plain primitive, a
    /// tuple, or `()` for none); `R` is anything mlua can build from the return
    /// (`()`, `u8`, `bool`, …). Collapses the `required → call → lua_err` trio
    /// that every simple capability op repeats.
    async fn call<A, R>(&self, name: &'static str, args: A) -> Result<R>
    where
        A: mlua::IntoLuaMulti + Send + 'static,
        R: mlua::FromLuaMulti + Send + 'static,
    {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, name)?;
            let mut mv = args
                .into_lua_multi(&ctx.lua)
                .map_err(|e| lua_err(name, e))?;
            mv.push_front(mlua::Value::Table(ctx.dev.clone()));
            f.call::<R>(mv).map_err(|e| lua_err(name, e))
        })
        .await
    }

    /// Like [`call`](Self::call) but deserializes the callback's returned Lua
    /// value into `R` via serde (for structured results like `Vec<Sensor>`).
    async fn call_ret<A, R>(&self, name: &'static str, args: A) -> Result<R>
    where
        A: mlua::IntoLuaMulti + Send + 'static,
        R: serde::de::DeserializeOwned + Send + 'static,
    {
        self.run(move |ctx| {
            let f = required(&ctx.manifest, name)?;
            let mut mv = args
                .into_lua_multi(&ctx.lua)
                .map_err(|e| lua_err(name, e))?;
            mv.push_front(mlua::Value::Table(ctx.dev.clone()));
            let value: Value = f.call(mv).map_err(|e| lua_err(name, e))?;
            ctx.lua.from_value(value).map_err(|e| lua_err(name, e))
        })
        .await
    }

    /// Call an **optional** callback `name(dev)` returning a serde value, or
    /// `R::default()` (e.g. an empty `Vec`) when the plugin didn't declare it.
    async fn call_opt<R>(&self, name: &'static str) -> Result<R>
    where
        R: serde::de::DeserializeOwned + Default + Send + 'static,
    {
        self.run(move |ctx| {
            let Some(f) = func(&ctx.manifest, name) else {
                return Ok(R::default());
            };
            let value: Value = f.call(ctx.dev.clone()).map_err(|e| lua_err(name, e))?;
            ctx.lua.from_value(value).map_err(|e| lua_err(name, e))
        })
        .await
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
                    if let Some(zones) = &t.zones {
                        check_zone_count(zones.len())?;
                        for z in zones {
                            check_led_count(&z.id, z.led_count)?;
                        }
                    }
                    if let Some(lcd) = &t.lcd {
                        check_lcd_dims(lcd.width, lcd.height)?;
                    }
                    if let Some(chain) = &t.chain {
                        check_zone_count(chain.len())?;
                        for c in chain {
                            check_led_count(&c.id, c.max_leds)?;
                        }
                    }
                    if let Some(controls) = &t.controls {
                        validate_runtime_controls(controls)?;
                    }
                    Ok(InitOutcome {
                        ok: t.ok,
                        model: t.model,
                        zones: t.zones,
                        native_effects: t.native_effects,
                        lcd: t.lcd,
                        chain: t.chain,
                        accessories: t.accessories,
                        controls: t.controls,
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
            let (callback, index) = match ctx.controller_index {
                Some(index) => ("apply_controller", Some(index)),
                None => ("apply", None),
            };
            let f = required(&ctx.manifest, callback)?;
            let state_v = ctx
                .lua
                .to_value(&state)
                .map_err(|e| lua_err("apply arg", e))?;
            match index {
                Some(index) => f.call::<()>((ctx.dev.clone(), index, state_v)),
                None => f.call::<()>((ctx.dev.clone(), state_v)),
            }
            .map_err(|e| lua_err(callback, e))
        })
        .await
    }

    pub async fn rgb_write_frame(&self, zone: &str, colors: &[RgbColor]) -> Result<()> {
        let zone = zone.to_owned();
        let colors = colors.to_vec();
        self.run(move |ctx| {
            let (callback, index) = match ctx.controller_index {
                Some(index) => ("write_controller_frame", Some(index)),
                None => ("write_frame", None),
            };
            let f = required(&ctx.manifest, callback)?;
            let colors_v = ctx
                .lua
                .to_value(&colors)
                .map_err(|e| lua_err("write_frame arg", e))?;
            match index {
                Some(index) => f.call::<()>((ctx.dev.clone(), index, zone, colors_v)),
                None => f.call::<()>((ctx.dev.clone(), zone, colors_v)),
            }
            .map_err(|e| lua_err(callback, e))
        })
        .await
    }

    pub async fn rgb_write_frame_batch(&self, zones: &[(String, Vec<RgbColor>)]) -> Result<()> {
        let zones = zones.to_vec();
        self.run(move |ctx| {
            let index = ctx.controller_index;
            let batch_callback = if index.is_some() {
                "write_controller_frame_batch"
            } else {
                "write_frame_batch"
            };

            if let Some(f) = func(&ctx.manifest, batch_callback) {
                let frames = ctx
                    .lua
                    .create_table()
                    .map_err(|e| lua_err("write_frame_batch arg", e))?;
                for (i, (zone_id, colors)) in zones.iter().enumerate() {
                    let frame = ctx
                        .lua
                        .create_table()
                        .map_err(|e| lua_err("write_frame_batch arg", e))?;
                    frame
                        .set("zone_id", zone_id.as_str())
                        .map_err(|e| lua_err("write_frame_batch arg", e))?;
                    frame
                        .set(
                            "colors",
                            ctx.lua
                                .to_value(colors)
                                .map_err(|e| lua_err("write_frame_batch arg", e))?,
                        )
                        .map_err(|e| lua_err("write_frame_batch arg", e))?;
                    frames
                        .set(i + 1, frame)
                        .map_err(|e| lua_err("write_frame_batch arg", e))?;
                }
                return match index {
                    Some(index) => f.call::<()>((ctx.dev.clone(), index, frames)),
                    None => f.call::<()>((ctx.dev.clone(), frames)),
                }
                .map_err(|e| lua_err(batch_callback, e));
            }

            let (callback, index) = match index {
                Some(index) => ("write_controller_frame", Some(index)),
                None => ("write_frame", None),
            };
            let f = required(&ctx.manifest, callback)?;
            for (zone, colors) in &zones {
                let colors_v = ctx
                    .lua
                    .to_value(colors)
                    .map_err(|e| lua_err("write_frame arg", e))?;
                match index {
                    Some(index) => f.call::<()>((ctx.dev.clone(), index, zone.as_str(), colors_v)),
                    None => f.call::<()>((ctx.dev.clone(), zone.as_str(), colors_v)),
                }
                .map_err(|e| lua_err(callback, e))?;
            }
            Ok(())
        })
        .await
    }

    pub async fn fan_get_duty(&self) -> Result<u8> {
        self.call("get_duty", ()).await
    }

    pub async fn fan_set_duty(&self, duty: u8) -> Result<()> {
        self.call("set_duty", duty).await
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
        self.call_ret("get_sensors", ()).await
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
        self.call_opt("detect_accessories").await
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
        let controllers: Vec<DetectedController> = self.call_opt("enumerate_controllers").await?;
        if controllers.len() > super::manifest::MAX_PLUGIN_CONTROLLERS {
            return Err(anyhow!(
                "plugin enumerated {} controllers, exceeding the {} limit",
                controllers.len(),
                super::manifest::MAX_PLUGIN_CONTROLLERS
            ));
        }
        for c in &controllers {
            check_zone_count(c.zones.len())?;
            for z in &c.zones {
                check_led_count(&z.id, z.led_count)?;
            }
        }
        Ok(controllers)
    }

    pub async fn hub_fan_rpm(&self, channel: u8) -> Result<u32> {
        self.call("fan_rpm", channel).await
    }

    pub async fn hub_fan_duty(&self, channel: u8) -> Result<u8> {
        self.call("fan_duty", channel).await
    }

    pub async fn hub_fan_controllable(&self, channel: u8) -> Result<bool> {
        self.call("fan_controllable", channel).await
    }

    pub async fn hub_set_fan_duty(&self, channel: u8, duty: u8) -> Result<()> {
        self.call("set_fan_duty", (channel, duty)).await
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
        self.call("lcd_set_brightness", (brightness, rotation))
            .await
    }

    pub async fn lcd_set_rotation(&self, brightness: u8, degrees: u32) -> Result<()> {
        self.call("lcd_set_rotation", (brightness, degrees)).await
    }

    pub async fn lcd_reset(&self) -> Result<()> {
        self.call("lcd_reset", ()).await
    }

    pub async fn dpi_set(&self, dpi: u16) -> Result<()> {
        self.call("set_dpi", dpi).await
    }

    pub async fn choice_set(&self, key: &str, selected: usize) -> Result<()> {
        self.call("set_choice", (key.to_owned(), selected)).await
    }

    pub async fn range_set(&self, key: &str, value: i32) -> Result<()> {
        self.call("set_range", (key.to_owned(), value)).await
    }

    pub async fn boolean_get(&self) -> Result<Vec<Boolean>> {
        self.call_opt("get_booleans").await
    }

    pub async fn boolean_set(&self, key: &str, value: bool) -> Result<()> {
        self.call("set_boolean", (key.to_owned(), value)).await
    }

    pub async fn action_trigger(&self, key: &str) -> Result<()> {
        self.call("trigger_action", key.to_owned()).await
    }

    pub async fn battery_get(&self) -> Result<Vec<Battery>> {
        self.call_opt("get_batteries").await
    }

    pub async fn connection_get(&self) -> Result<Option<ConnectionStatus>> {
        // Serde maps a missing callback and a Lua `nil` return both to `None`.
        self.call_opt("connection_status").await
    }

    pub async fn equalizer_get(&self) -> Result<Equalizer> {
        self.call_ret("get_equalizer", ()).await
    }

    pub async fn equalizer_set_preset(&self, preset: usize) -> Result<()> {
        self.call("set_eq_preset", preset).await
    }

    pub async fn equalizer_set_bands(&self, values: &[f32]) -> Result<()> {
        self.call("set_eq_bands", values.to_vec()).await
    }

    pub async fn pairing_start(&self, timeout_secs: u8) -> Result<()> {
        self.call("start_pairing", timeout_secs).await
    }

    pub async fn pairing_stop(&self) -> Result<()> {
        self.call("stop_pairing", ()).await
    }

    pub async fn pairing_unpair(&self, slot: u8) -> Result<()> {
        self.call("unpair", slot).await
    }

    pub async fn pairing_status(&self) -> Result<PairingStatus> {
        self.call_ret("pairing_status", ()).await
    }

    pub async fn onboard_switch_profile(&self, slot: u8) -> Result<()> {
        self.call("switch_profile", slot).await
    }

    pub async fn onboard_restore_profile(&self, slot: u8) -> Result<()> {
        self.call("restore_profile", slot).await
    }

    pub async fn onboard_set_profile_enabled(&self, slot: u8, enabled: bool) -> Result<()> {
        self.call("set_profile_enabled", (slot, enabled)).await
    }

    pub async fn onboard_profiles_get(&self) -> Result<OnboardProfiles> {
        self.call_ret("onboard_profiles_status", ()).await
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
        self.call("reset_button_mapping", cid).await
    }

    pub async fn key_remap_reset_all(&self) -> Result<()> {
        self.call("reset_all_button_mappings", ()).await
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

fn validate_runtime_controls(controls: &InitControls) -> Result<()> {
    let mut keys = std::collections::HashSet::new();
    for key in controls
        .choices
        .iter()
        .map(|control| &control.key)
        .chain(controls.ranges.iter().map(|control| &control.key))
        .chain(controls.booleans.iter().map(|control| &control.key))
        .chain(controls.actions.iter().map(|control| &control.key))
    {
        if key.is_empty() || !keys.insert(key) {
            bail!("runtime control keys must be non-empty and unique");
        }
    }
    for range in &controls.ranges {
        if range.min > range.max
            || range.step <= 0
            || range.default < range.min
            || range.default > range.max
        {
            bail!("runtime range '{}' has invalid bounds", range.key);
        }
    }
    Ok(())
}

/// Build the worker's VM context on the worker thread. Runs once at spawn; the
/// [`LuaWorker`] loop then drives jobs against the returned [`WorkerCtx`].
#[allow(clippy::too_many_arguments)]
fn build_ctx(
    source: &str,
    transport: PluginIo,
    dev_match: DevMatch,
    granted: &[Permission],
    config: &HashMap<String, String>,
    handle: Handle,
    zones: &[RgbZone],
    audio_registry: super::audio_api::SinkRegistry,
) -> Result<WorkerCtx> {
    let controller_index = dev_match.index;
    let (lua, budget) = sandbox::bootstrap_vm(
        sandbox::InjectSurface::FullRuntime { granted, config },
        PLUGIN_VM_MEMORY_BYTES,
        PLUGIN_INSTRUCTION_BUDGET,
    )
    .map_err(|e| lua_err("sandbox setup", e))?;

    let manifest: Table = lua
        .load(source)
        .eval()
        .map_err(|e| lua_err("script evaluation", e))?;

    // The `dev` argument every callback receives: exposes the transport and the
    // matched-spec identity (`dev.match`).
    let dev = lua.create_table().map_err(|e| lua_err("dev table", e))?;
    let command = match &transport {
        PluginIo::Command(command) => Some(command.clone()),
        _ => None,
    };
    let api = TransportApi::new(transport, handle.clone());
    let api_ud = lua
        .create_userdata(api)
        .map_err(|e| lua_err("transport userdata", e))?;
    dev.set("transport", api_ud)
        .map_err(|e| lua_err("dev.transport", e))?;
    if let Some(command) = command {
        install_command_api(&lua, command)?;
    }
    dev.set("match", build_match_table(&lua, &dev_match)?)
        .map_err(|e| lua_err("dev.match", e))?;

    // `dev.audio`: device-scoped virtual audio sinks, only when granted the
    // audio-routing permission. The registry is owned by the device side.
    if granted.contains(&Permission::AudioRouting) {
        let audio_ud = super::audio_api::build(
            &lua,
            dev_match.vid,
            dev_match.pid,
            handle.clone(),
            audio_registry.clone(),
        )
        .map_err(|e| lua_err("dev.audio", e))?;
        dev.set("audio", audio_ud)
            .map_err(|e| lua_err("dev.audio", e))?;
    }
    if !zones.is_empty() {
        let zones_v = lua.to_value(zones).map_err(|e| lua_err("dev.zones", e))?;
        dev.set("zones", zones_v)
            .map_err(|e| lua_err("dev.zones", e))?;
    }

    Ok(WorkerCtx {
        lua,
        dev,
        manifest,
        controller_index,
        budget,
    })
}

fn install_command_api(lua: &Lua, command: CommandExecutor) -> Result<()> {
    let api = lua
        .create_table()
        .map_err(|e| lua_err("command table", e))?;
    let run = lua
        .create_function(move |lua, (executable, args): (String, Vec<String>)| {
            let output = command.run(&executable, &args).map_err(to_lua_err)?;
            lua.create_string(&output)
        })
        .map_err(|e| lua_err("command.run", e))?;
    api.set("run", run).map_err(|e| lua_err("command.run", e))?;
    lua.globals()
        .set("command", api)
        .map_err(|e| lua_err("command", e))?;
    Ok(())
}

/// Run a plugin's `pre_scan(dev)` callback against a freshly opened SMBus bus,
/// before the scanner probes addresses. Used for one-time bus preparation whose
/// control flow depends on live reads (e.g. the ENE DRAM broadcast remap). The
/// transport is a register bus scoped to `scope_addrs` (declared + extras), so
/// pre_scan can never reach an address the plugin didn't declare. Runs on the
/// calling thread (a `spawn_blocking` worker), so register batches block inline.
/// Wall-clock ceiling on one `pre_scan` run. The instruction budget catches an
/// *uncaught* runaway, but a `pcall`-catching loop stays on the thread; since
/// `pre_scan` runs during SMBus discovery before any device is matched, so a
/// wedged one would otherwise hang the scanner. Scan entries are activation-gated;
/// on timeout the eval thread is abandoned (memory-capped, so bounded).
const PRE_SCAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn run_pre_scan(
    source: &str,
    bus: Arc<SmBusDevice>,
    scope_addrs: Vec<u8>,
    granted: &[Permission],
    handle: Handle,
) -> Result<()> {
    let source = source.to_owned();
    let granted = granted.to_vec();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("halod-pre-scan".into())
        .spawn(move || {
            let _ = tx.send(run_pre_scan_inner(
                &source,
                bus,
                scope_addrs,
                &granted,
                handle,
            ));
        })
        .map_err(|e| anyhow!("spawning pre_scan thread failed: {e}"))?;
    match rx.recv_timeout(PRE_SCAN_TIMEOUT) {
        Ok(res) => res,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            bail!("pre_scan exceeded its {PRE_SCAN_TIMEOUT:?} deadline")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => bail!("pre_scan thread died"),
    }
}

fn run_pre_scan_inner(
    source: &str,
    bus: Arc<SmBusDevice>,
    scope_addrs: Vec<u8>,
    granted: &[Permission],
    handle: Handle,
) -> Result<()> {
    if !granted.contains(&Permission::Smbus) {
        bail!("pre_scan requires the `smbus` permission");
    }

    // `pre_scan` is one-time bus preparation before a device is even matched,
    // not general plugin logic — it gets no `halod.config` (an empty map).
    let (lua, _budget) = sandbox::bootstrap_vm(
        sandbox::InjectSurface::FullRuntime {
            granted,
            config: &HashMap::new(),
        },
        PLUGIN_VM_MEMORY_BYTES,
        PLUGIN_INSTRUCTION_BUDGET,
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
    if let Some(vid) = m.vid {
        t.set("vid", vid).map_err(|e| lua_err("match.vid", e))?;
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

    #[test]
    fn runtime_controls_reject_duplicate_keys_and_invalid_ranges() {
        let duplicate = InitControls {
            choices: vec![ChoiceDef {
                key: "mode".to_owned(),
                label: "Mode".to_owned(),
                category: String::new(),
                display: Default::default(),
                options: vec![],
                default: 0,
            }],
            actions: vec![ActionDef {
                key: "mode".to_owned(),
                label: "Reset".to_owned(),
                category: String::new(),
            }],
            ..Default::default()
        };
        assert!(validate_runtime_controls(&duplicate).is_err());

        let invalid_range = InitControls {
            ranges: vec![RangeDef {
                key: "brightness".to_owned(),
                label: "Brightness".to_owned(),
                min: 10,
                max: 1,
                step: 1,
                read_only: false,
                category: String::new(),
                start_label: None,
                end_label: None,
                display: Default::default(),
                default: 1,
            }],
            ..Default::default()
        };
        assert!(validate_runtime_controls(&invalid_range).is_err());
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
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        )
    }

    fn spawn_controller(source: &str, index: u32) -> PluginHandle {
        PluginHandle::spawn(
            source.to_owned(),
            stream_io(),
            DevMatch {
                transport: "tcp".to_owned(),
                index: Some(index),
                ..Default::default()
            },
            vec![],
            HashMap::new(),
            Handle::current(),
            Vec::new(),
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
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
    async fn integration_controller_routes_rgb_callbacks_with_its_index() {
        let source = r#"return {
            apply_controller = function(dev, index, state)
                assert(index == 7)
            end,
            write_controller_frame = function(dev, index, zone, colors)
                assert(index == 7)
                assert(zone == "zone-1")
                assert(#colors == 1)
            end,
        }"#;
        let h = spawn_controller(source, 7);

        h.rgb_apply(static_state()).await.unwrap();
        h.rgb_write_frame("zone-1", &[RgbColor { r: 1, g: 2, b: 3 }])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn integration_controller_batches_all_zones_in_one_callback() {
        let source = r#"return {
            write_controller_frame_batch = function(dev, index, frames)
                assert(index == 7)
                assert(#frames == 2)
                assert(frames[1].zone_id == "zone-1")
                assert(#frames[1].colors == 1)
                assert(frames[1].colors[1].r == 1)
                assert(frames[2].zone_id == "zone-2")
                assert(frames[2].colors[1].b == 6)
            end,
        }"#;
        let h = spawn_controller(source, 7);

        h.rgb_write_frame_batch(&[
            ("zone-1".into(), vec![RgbColor { r: 1, g: 2, b: 3 }]),
            ("zone-2".into(), vec![RgbColor { r: 4, g: 5, b: 6 }]),
        ])
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn frame_batch_falls_back_to_per_zone_callback() {
        let source = r#"local calls = 0
        return {
            write_controller_frame = function(dev, index, zone, colors)
                calls = calls + 1
                assert(index == 7)
                assert(zone == "zone-" .. calls)
            end,
        }"#;
        let h = spawn_controller(source, 7);

        h.rgb_write_frame_batch(&[
            ("zone-1".into(), vec![RgbColor { r: 1, g: 2, b: 3 }]),
            ("zone-2".into(), vec![RgbColor { r: 4, g: 5, b: 6 }]),
        ])
        .await
        .unwrap();
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
