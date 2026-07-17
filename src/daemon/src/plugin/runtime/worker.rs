// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-device worker thread. It owns the Lua VM + transport (both `!Send`), so
//! the `Send + Sync` `LuaDevice` talks to it over a channel. Each capability call
//! is a boxed *job* the device side builds and the worker runs against the VM,
//! answering on a `oneshot`. Transport I/O the script triggers is synchronous
//! from Lua's view; the worker drives the async transport via a captured runtime
//! handle.

use std::cell::{Cell, RefCell};
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
use halod_shared::keyboard::{KeyId, StandardLayout};
use halod_shared::types::{
    Battery, Boolean, ButtonDescriptor, ButtonMapping, ConnectionStatus, DeviceType, Equalizer,
    KeyboardFormFactor, KeyboardLayout, LedPosition, NativeEffect, OnboardProfiles, PairingStatus,
    Permission, RgbColor, RgbState, RgbZone, Sensor,
};

use super::bytebuf::ByteBuf;
use super::ffi::to_lua_err;
use super::manifest::{
    check_lcd_dims, check_led_count, check_zone_count, validate_accessories, validate_component,
    validate_short_text, AccessoryManifest, ActionDef, BooleanDef, ChoiceDef, RangeDef,
};
use super::sandbox;
use super::transport::{command_result_table, AddrScope, CommandExecutor, PluginIo, RegisterBus};
use super::transport_api::TransportApi;
use crate::drivers::transports::smbus::SmBusDevice;

/// Maximum HID reports handled by one serialized Lua-worker job. Event input,
/// RGB frames, status polling, and capability commands share that worker; a
/// complete 256-report queue would otherwise monopolize it long enough to
/// reject or visibly stall animation frames. The transport re-wakes the event
/// task when a bounded drain leaves reports behind.
const EVENT_DISPATCH_BATCH: usize = 32;

use super::{PLUGIN_INSTRUCTION_BUDGET, PLUGIN_VM_MEMORY_BYTES};

/// Optional input transitions returned by a plugin's `read_status` callback.
/// The daemon, rather than the sandboxed script, owns delivery to the remap
/// engine. Plugins only decode their transport-specific notification packets.
#[derive(Debug, Default, Deserialize)]
pub struct PollOutcome {
    #[serde(default)]
    pub child_index: Option<u32>,
    #[serde(default)]
    pub state_changed: bool,
    #[serde(default)]
    pub children_changed: bool,
    #[serde(default)]
    pub button_events: PollButtonEvents,
}

#[derive(Debug, Default, Deserialize)]
pub struct PollButtonEvents {
    #[serde(default)]
    pub pressed: Vec<u16>,
    #[serde(default)]
    pub released: Vec<u16>,
}

/// One accessory the plugin's `detect_accessories` reports.
#[derive(Debug, Clone, Deserialize)]
pub struct DetectedAccessory {
    pub channel: u8,
    pub accessory: u8,
}

/// One RGB zone of a controller an integration plugin's `enumerate_controllers`
/// reports for plugin-test inspection.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DetectedControllerZone {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub led_count: u32,
}

/// One controller the plugin's `enumerate_controllers` reports. It supplies
/// identity and routing only; capability descriptors come from that child's
/// own `initialize` result.
#[derive(Debug, Clone, Deserialize)]
pub struct DetectedController {
    pub index: u32,
    pub name: String,
    /// Physical class reported by the integration/root plugin. Dynamic
    /// children otherwise have no static manifest entry from which to inherit
    /// mouse/keyboard/headset identity.
    #[serde(default)]
    pub device_type: DeviceType,
    /// Stable host device id. Must be unique among this root's children.
    #[serde(default)]
    pub id: Option<String>,
    /// Opaque stable route key, passed to the child's `dev.match.key`.
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    /// Transport-specific match fields inherited by a dynamic child (for
    /// example an LPCIO chip id/revision and HWM base).
    #[serde(default)]
    pub extra: HashMap<String, u64>,
    /// RGB topology used by the plugin-test harness to validate enumerated
    /// controller output. Runtime descriptors come from `initialize`.
    #[serde(default)]
    pub zones: Vec<DetectedControllerZone>,
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
    /// Opaque stable child key (for example a GPU UUID) supplied by Lua.
    pub key: Option<String>,
    /// Display name supplied by a dynamically enumerated controller.  This is
    /// routing metadata only; packages may use it as a model fallback.
    pub name: Option<String>,
    pub extra: HashMap<String, u64>,
}

/// One RGB zone a plugin's `initialize` reports for dynamic LED counts.
#[derive(Debug, Clone, Deserialize)]
pub struct InitZone {
    pub id: String,
    pub name: String,
    #[serde(default = "default_zone_topology")]
    pub topology: String,
    pub led_count: u32,
    /// Optional firmware LED ids, in display order. When absent, ids are
    /// synthesized from 0..led_count as before.
    #[serde(default)]
    pub led_ids: Vec<u32>,
    /// Optional explicit normalized LED geometry. This takes precedence over
    /// `led_ids` and topology-derived layouts when supplied.
    #[serde(default)]
    pub leds: Vec<LedPosition>,
    #[serde(default)]
    pub rings: u8,
    #[serde(default)]
    pub keyboard_form_factor: Option<KeyboardFormFactor>,
    #[serde(default)]
    pub keyboard_layout: Option<KeyboardLayout>,
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
    /// Reapply the RGB state after uploading an LCD frame when the device's
    /// firmware resets its lighting engine as a side effect.
    #[serde(default)]
    pub needs_rgb_restore: bool,
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

/// Runtime DPI descriptor. DPI bounds and stored steps belong to the physical
/// device/profile, not the inert package catalog.
#[derive(Debug, Clone, Deserialize)]
pub struct InitDpi {
    pub min: u16,
    pub max: u16,
    #[serde(default)]
    pub steps: Vec<u16>,
    /// Exact values accepted by the hardware. This is distinct from the
    /// active host/onboard step list.
    #[serde(default)]
    pub available_dpis: Vec<u16>,
    #[serde(default)]
    pub onboard: bool,
    /// Boolean control selecting host (`true`) versus onboard (`false`) mode.
    /// Keeping the key in the descriptor makes this reusable by any plugin.
    #[serde(default)]
    pub mode_control: Option<String>,
    /// Current firmware DPI.  Unlike the step table this must be read from
    /// the device: choosing a midpoint causes a visible, incorrect UI state.
    #[serde(default)]
    pub current: Option<u16>,
}

/// Runtime fan descriptor. The physical channel is device-specific and is
/// therefore supplied by initialize rather than the catalog.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InitFan {
    #[serde(default)]
    pub channel: u8,
}

/// Runtime key-remap descriptor. Button CIDs and host-mode policy are reported
/// by the device after its active profile has been discovered.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InitKeyRemap {
    #[serde(default)]
    pub buttons: Vec<ButtonDescriptor>,
    #[serde(default)]
    pub requires_host_mode: bool,
    #[serde(default)]
    pub default_mappings: Vec<ButtonMapping>,
}

/// One firmware LED/key mapping in a runtime keyboard layout. Standard keys
/// resolve their geometry from `base`; device-specific keys provide `cell` and
/// receive a stable `KeyId::Custom(led_id)` identity.
#[derive(Debug, Clone, Deserialize)]
pub struct InitKeyCell {
    pub col: f32,
    pub row: f32,
    #[serde(default = "default_key_size")]
    pub w: f32,
    #[serde(default = "default_key_size")]
    pub h: f32,
}

fn default_key_size() -> f32 {
    1.0
}

#[derive(Debug, Clone, Deserialize)]
pub struct InitKeyboardKey {
    pub led_id: u32,
    #[serde(default)]
    pub key: Option<KeyId>,
    #[serde(default)]
    pub cell: Option<InitKeyCell>,
    #[serde(default)]
    pub remap_cid: Option<u16>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InitKeyboardVariant {
    pub base: StandardLayout,
    #[serde(default)]
    pub keys: Vec<InitKeyboardKey>,
}

/// Runtime keyboard geometry. The host owns standard key templates and user
/// layout selection; Lua supplies only model-specific LED mappings/additions.
#[derive(Debug, Clone, Deserialize)]
pub struct InitKeyboard {
    pub ansi: InitKeyboardVariant,
    #[serde(default)]
    pub iso: Option<InitKeyboardVariant>,
    #[serde(default = "unknown_keyboard_layout")]
    pub detected_language: KeyboardLayout,
    #[serde(default)]
    pub languages: Vec<KeyboardLayout>,
}

fn unknown_keyboard_layout() -> KeyboardLayout {
    KeyboardLayout::Unknown
}

/// What `initialize` returns: a bare bool, or a table with dynamic device info
/// discovered from the hardware (firmware/model, RGB zones, LCD panel, and the
/// live range/choice values read back from the device to seed the host caches).
#[derive(Debug, Default)]
pub struct InitOutcome {
    pub ok: bool,
    pub model: Option<String>,
    /// Device-specific subset of the package's advertised capability union.
    /// `None` preserves the advertised set for older plugins.
    pub capabilities: Option<Vec<String>>,
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
    pub dpi: Option<InitDpi>,
    pub fan: Option<InitFan>,
    pub key_remap: Option<InitKeyRemap>,
    pub keyboard: Option<InitKeyboard>,
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
    capabilities: Option<Vec<String>>,
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
    dpi: Option<InitDpi>,
    #[serde(default)]
    fan: Option<InitFan>,
    #[serde(default)]
    key_remap: Option<InitKeyRemap>,
    #[serde(default)]
    keyboard: Option<InitKeyboard>,
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
    transport: PluginIo,
    handle: Handle,
    /// The `dev` argument every callback receives: exposes the transport and the
    /// matched-spec identity (`dev.match`), and caches `read_status` as `dev.status`.
    dev: Table,
    /// Persistent child tables owned by this root VM. Dynamic children route
    /// through the same worker while retaining isolated Lua-side state.
    child_devs: RefCell<HashMap<u32, Table>>,
    /// The plugin's returned table, holding its callback functions.
    manifest: Table,
    /// Integration-controller index. When present, RGB calls use the
    /// controller-aware callbacks instead of ordinary device-plugin callbacks.
    controller_index: Option<u32>,
    /// Instruction counter for the runaway-guard hook; reset before each job.
    budget: Rc<Cell<u64>>,
}

impl WorkerCtx {
    fn routed_dev(&self, route: Option<&DevMatch>) -> Result<Table> {
        let Some(route) = route else {
            return Ok(self.dev.clone());
        };
        let index = route
            .index
            .ok_or_else(|| anyhow!("dynamic child route has no controller index"))?;
        if let Some(dev) = self.child_devs.borrow().get(&index) {
            return Ok(dev.clone());
        }

        let dev = self
            .lua
            .create_table()
            .map_err(|e| lua_err("child dev table", e))?;
        let transport: Value = self
            .dev
            .get("transport")
            .map_err(|e| lua_err("child dev.transport", e))?;
        dev.set("transport", transport)
            .map_err(|e| lua_err("child dev.transport", e))?;
        dev.set("match", build_match_table(&self.lua, route)?)
            .map_err(|e| lua_err("child dev.match", e))?;
        if let Ok(audio) = self.dev.get::<Value>("audio") {
            if !matches!(audio, Value::Nil) {
                dev.set("audio", audio)
                    .map_err(|e| lua_err("child dev.audio", e))?;
            }
        }
        self.child_devs.borrow_mut().insert(index, dev.clone());
        Ok(dev)
    }
}

/// A unit of work the device side sends to the worker thread. It runs against
/// the [`WorkerCtx`], sends its own reply, and tells the loop whether to keep
/// going (`close` returns `Break`).
type Job = Box<dyn FnOnce(&WorkerCtx) -> ControlFlow<()> + Send>;

/// Look up a plugin callback by name, or `None` if the plugin didn't declare it.
fn func(manifest: &Table, name: &str) -> Option<Function> {
    debug_assert!(
        super::contract::callback(name).is_some(),
        "uncatalogued plugin callback: {name}"
    );
    match manifest.get::<Value>(name) {
        Ok(Value::Function(f)) => Some(f),
        _ => None,
    }
}

/// A callback the operation requires; errors with a uniform message if absent.
fn required(manifest: &Table, name: &str) -> Result<Function> {
    debug_assert!(
        super::contract::callback(name).is_some(),
        "uncatalogued plugin callback: {name}"
    );
    func(manifest, name).ok_or_else(|| {
        anyhow!(
            "plugin API {} has no {name}()",
            super::contract::active().version
        )
    })
}

fn lua_err(context: &str, e: mlua::Error) -> anyhow::Error {
    anyhow!("plugin {context}: {e}")
}

/// Handle the `LuaDevice` holds. The inner [`LuaWorker`] is `Send + Sync`, so the
/// device stays `Send + Sync`. Dropping it ends the worker (channel closes).
#[derive(Clone)]
pub struct PluginHandle {
    worker: LuaWorker<Job>,
    route: Option<DevMatch>,
}

impl PluginHandle {
    /// Whether this handle addresses a dynamic child inside another device's
    /// worker. Child teardown must not close the shared worker or transport.
    pub fn is_child(&self) -> bool {
        self.route.is_some()
    }

    /// Spawn the worker thread. `source` is the full script; the worker builds
    /// its own VM from it (no live VM crosses threads). `granted` is the
    /// plugin's currently-granted permission set, and `config` its resolved
    /// config values (including decrypted secrets if `SecureStorage` is
    /// granted) — both snapshotted at spawn time.
    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn spawn(
        source: String,
        module_sources: std::collections::BTreeMap<String, String>,
        transport: PluginIo,
        dev_match: DevMatch,
        granted: Vec<Permission>,
        config: crate::plugin::ResolvedConfig,
        handle: Handle,
        zones: Vec<RgbZone>,
        audio_registry: super::audio_api::SinkRegistry,
    ) -> Self {
        Self::spawn_with_data(
            source,
            module_sources,
            transport,
            dev_match,
            granted,
            config,
            handle,
            zones,
            audio_registry,
            Default::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_data(
        source: String,
        module_sources: std::collections::BTreeMap<String, String>,
        transport: PluginIo,
        dev_match: DevMatch,
        granted: Vec<Permission>,
        config: crate::plugin::ResolvedConfig,
        handle: Handle,
        zones: Vec<RgbZone>,
        audio_registry: super::audio_api::SinkRegistry,
        data: super::data_api::DataRuntime,
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
                    &module_sources,
                    transport,
                    dev_match,
                    &granted,
                    &config,
                    handle,
                    &zones,
                    audio_registry,
                    data,
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
        Self {
            worker,
            route: None,
        }
    }

    /// Return a child route into this root worker. No VM, thread, or transport
    /// is created; every call uses the root's serialized command queue.
    pub fn child(&self, route: DevMatch) -> Self {
        Self {
            worker: self.worker.clone(),
            route: Some(route),
        }
    }

    /// Run `f` on the worker thread and await its result. `f` gets the VM, the
    /// `dev` table and the manifest table; only its owned captures + reply
    /// sender cross the channel, so `f` must be `Send`.
    async fn run<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&WorkerCtx, Table, Option<u32>) -> Result<R> + Send + 'static,
    {
        let route = self.route.clone();
        self.worker
            .request(|reply| {
                Box::new(move |ctx: &WorkerCtx| {
                    let result = ctx.routed_dev(route.as_ref()).and_then(|dev| {
                        let index = route
                            .as_ref()
                            .and_then(|route| route.index)
                            .or(ctx.controller_index);
                        f(ctx, dev, index)
                    });
                    let _ = reply.send(result);
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
        self.run(move |ctx, dev, _| {
            let f = required(&ctx.manifest, name)?;
            let mut mv = args
                .into_lua_multi(&ctx.lua)
                .map_err(|e| lua_err(name, e))?;
            mv.push_front(mlua::Value::Table(dev));
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
        self.run(move |ctx, dev, _| {
            let f = required(&ctx.manifest, name)?;
            let mut mv = args
                .into_lua_multi(&ctx.lua)
                .map_err(|e| lua_err(name, e))?;
            mv.push_front(mlua::Value::Table(dev));
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
        self.run(move |ctx, dev, _| {
            let Some(f) = func(&ctx.manifest, name) else {
                return Ok(R::default());
            };
            let value: Value = f.call(dev).map_err(|e| lua_err(name, e))?;
            ctx.lua.from_value(value).map_err(|e| lua_err(name, e))
        })
        .await
    }

    /// Run `initialize`, accepting either a bare bool or a table with dynamic
    /// device info (`{ ok, model, zones, lcd }`). A missing callback means
    /// "present, no info".
    pub async fn initialize(&self) -> Result<InitOutcome> {
        self.run(|ctx, dev, _| {
            let Some(f) = func(&ctx.manifest, "initialize") else {
                return Ok(InitOutcome {
                    ok: true,
                    ..Default::default()
                });
            };
            let value: Value = f.call(dev).map_err(|e| lua_err("initialize", e))?;
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
                            let count = if !z.leds.is_empty() {
                                u32::try_from(z.leds.len()).unwrap_or(u32::MAX)
                            } else if !z.led_ids.is_empty() {
                                u32::try_from(z.led_ids.len()).unwrap_or(u32::MAX)
                            } else {
                                z.led_count
                            };
                            check_led_count(&z.id, count)?;
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
                    if let Some(accessories) = &t.accessories {
                        validate_accessories(accessories)?;
                    }
                    if let Some(controls) = &t.controls {
                        validate_runtime_controls(controls)?;
                    }
                    if let Some(dpi) = &t.dpi {
                        anyhow::ensure!(dpi.min <= dpi.max, "DPI minimum exceeds maximum");
                        anyhow::ensure!(
                            dpi.steps
                                .iter()
                                .all(|step| *step >= dpi.min && *step <= dpi.max),
                            "DPI steps must stay within the declared bounds"
                        );
                        if let Some(current) = dpi.current {
                            anyhow::ensure!(
                                current >= dpi.min && current <= dpi.max,
                                "current DPI must stay within the declared bounds"
                            );
                        }
                    }
                    if let Some(key_remap) = &t.key_remap {
                        crate::input::validate::validate_button_mappings(
                            &key_remap.buttons,
                            &key_remap.default_mappings,
                        )?;
                    }
                    Ok(InitOutcome {
                        ok: t.ok,
                        model: t.model,
                        capabilities: t.capabilities,
                        zones: t.zones,
                        native_effects: t.native_effects,
                        lcd: t.lcd,
                        chain: t.chain,
                        accessories: t.accessories,
                        controls: t.controls,
                        dpi: t.dpi,
                        fan: t.fan,
                        key_remap: t.key_remap,
                        keyboard: t.keyboard,
                        ranges: t.ranges,
                        choices: t.choices,
                    })
                }
            }
        })
        .await
    }

    pub async fn close(&self) -> Result<()> {
        if let Some(route) = self.route.clone() {
            self.worker
                .request_terminal(|reply: oneshot::Sender<Result<()>>| {
                    Box::new(move |ctx: &WorkerCtx| {
                        let hook_result = ctx.routed_dev(Some(&route)).and_then(|dev| {
                            func(&ctx.manifest, "close_child").map_or(Ok(()), |f| {
                                f.call::<()>(dev).map_err(|e| lua_err("close_child", e))
                            })
                        });
                        if let Some(index) = route.index {
                            ctx.child_devs.borrow_mut().remove(&index);
                        }
                        let _ = reply.send(hook_result);
                        ControlFlow::Continue(())
                    })
                })
                .await?
        } else {
            // The job returns `Break` to end the worker loop after running the
            // plugin's `close` callback; the reply confirms it finished.
            self.worker
                .request_terminal(|reply: oneshot::Sender<Result<()>>| {
                    Box::new(move |ctx: &WorkerCtx| {
                        let hook_result = func(&ctx.manifest, "close").map_or(Ok(()), |f| {
                            f.call::<()>(ctx.dev.clone())
                                .map_err(|e| lua_err("close", e))
                        });
                        let _ = reply.send(hook_result);
                        ControlFlow::Break(())
                    })
                })
                .await?
        }
    }

    pub async fn rgb_apply(&self, state: RgbState) -> Result<()> {
        self.run(move |ctx, dev, _| {
            let f = required(&ctx.manifest, "apply")?;
            let state_v = ctx
                .lua
                .to_value(&state)
                .map_err(|e| lua_err("apply arg", e))?;
            f.call::<()>((dev, state_v))
                .map_err(|e| lua_err("apply", e))
        })
        .await
    }

    pub async fn rgb_write_frame(
        &self,
        zone: &str,
        colors: &[RgbColor],
        led_ids: &[u32],
    ) -> Result<()> {
        let zone = zone.to_owned();
        let colors = colors.to_vec();
        let led_ids = led_ids.to_vec();
        self.run(move |ctx, dev, _| {
            let f = required(&ctx.manifest, "write_frame")?;
            let colors_v = ctx
                .lua
                .to_value(&colors)
                .map_err(|e| lua_err("write_frame arg", e))?;
            let led_ids_v = ctx
                .lua
                .to_value(&led_ids)
                .map_err(|e| lua_err("write_frame arg", e))?;
            f.call::<()>((dev, zone, colors_v, led_ids_v))
                .map_err(|e| lua_err("write_frame", e))
        })
        .await
    }

    pub async fn rgb_write_frame_batch(
        &self,
        zones: &[(String, Vec<RgbColor>, Vec<u32>)],
    ) -> Result<()> {
        let zones = zones.to_vec();
        self.run(move |ctx, dev, _| {
            if let Some(f) = func(&ctx.manifest, "write_frame_batch") {
                let frames = ctx
                    .lua
                    .create_table()
                    .map_err(|e| lua_err("write_frame_batch arg", e))?;
                for (i, (zone_id, colors, led_ids)) in zones.iter().enumerate() {
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
                    frame
                        .set(
                            "led_ids",
                            ctx.lua
                                .to_value(led_ids)
                                .map_err(|e| lua_err("write_frame_batch arg", e))?,
                        )
                        .map_err(|e| lua_err("write_frame_batch arg", e))?;
                    frames
                        .set(i + 1, frame)
                        .map_err(|e| lua_err("write_frame_batch arg", e))?;
                }
                return f
                    .call::<()>((dev.clone(), frames))
                    .map_err(|e| lua_err("write_frame_batch", e));
            }

            let f = required(&ctx.manifest, "write_frame")?;
            for (zone, colors, led_ids) in &zones {
                let colors_v = ctx
                    .lua
                    .to_value(colors)
                    .map_err(|e| lua_err("write_frame arg", e))?;
                let led_ids_v = ctx
                    .lua
                    .to_value(led_ids)
                    .map_err(|e| lua_err("write_frame arg", e))?;
                f.call::<()>((dev.clone(), zone.as_str(), colors_v, led_ids_v))
                    .map_err(|e| lua_err("write_frame", e))?;
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

    pub async fn fan_get_rpm(&self) -> Result<Option<u32>> {
        self.run(|ctx, dev, _| {
            let Some(f) = func(&ctx.manifest, "get_rpm") else {
                return Ok(None);
            };
            f.call::<Option<u32>>(dev)
                .map_err(|e| lua_err("get_rpm", e))
        })
        .await
    }

    pub async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        self.call_ret("get_sensors", ()).await
    }

    /// Run `read_status(dev)`, cache its returned table as `dev.status`, and
    /// extract optional input transitions for the host-owned remap engine.
    /// Errors (e.g. a non-blocking read with nothing pending) are logged, not
    /// fatal — the loop keeps ticking.
    pub async fn poll(&self) -> Result<PollOutcome> {
        self.run(|ctx, dev, _| {
            if let Some(f) = func(&ctx.manifest, "read_status") {
                match f.call::<Value>(dev.clone()) {
                    Ok(status) => {
                        let outcome = match &status {
                            Value::Table(table) => ctx
                                .lua
                                .from_value::<PollOutcome>(Value::Table(table.clone()))
                                .unwrap_or_default(),
                            _ => PollOutcome::default(),
                        };
                        if let Err(e) = dev.set("status", status) {
                            log::debug!("plugin poll: caching status failed: {e}");
                        }
                        return Ok(outcome);
                    }
                    Err(e) => return Err(lua_err("read_status", e)),
                }
            }
            Ok(PollOutcome::default())
        })
        .await
    }

    pub async fn supports_events(&self) -> Result<bool> {
        self.run(|ctx, _, _| Ok(func(&ctx.manifest, "event").is_some()))
            .await
    }

    /// Drain dispatcher reports and deliver them to `event(dev, event)` on
    /// this same serialized worker. Each callback may return button transitions
    /// and an optional child index for receiver routing.
    pub async fn on_transport_events(&self) -> Result<Vec<PollOutcome>> {
        self.run(|ctx, dev, _| {
            let transport = match &ctx.transport {
                PluginIo::Stream { transport, .. } => match transport.as_hid() {
                    Some(hid) => hid,
                    None => return Ok(Vec::new()),
                },
                _ => return Ok(Vec::new()),
            };
            let events = ctx
                .handle
                .block_on(transport.drain_events(EVENT_DISPATCH_BATCH))
                .map_err(|e| anyhow!("draining transport events: {e:#}"))?;
            if !events.is_empty() {
                log::trace!(
                    "[plugin worker] drained {} transport event(s)",
                    events.len()
                );
            }
            let Some(callback) = func(&ctx.manifest, "event") else {
                return Ok(Vec::new());
            };
            let mut outcomes = Vec::new();
            for event in events {
                let event_table = ctx
                    .lua
                    .create_table()
                    .map_err(|e| lua_err("event arg", e))?;
                event_table
                    .set("transport", "hid")
                    .map_err(|e| lua_err("event arg", e))?;
                event_table
                    .set("endpoint", event.endpoint)
                    .map_err(|e| lua_err("event arg", e))?;
                event_table
                    .set(
                        "report",
                        ctx.lua
                            .create_string(&event.data)
                            .map_err(|e| lua_err("event arg", e))?,
                    )
                    .map_err(|e| lua_err("event arg", e))?;

                // A protocol may cheaply discard acknowledgements or select
                // one dynamic child before the heavier callback runs. `nil`
                // means the root/originating device when no finer source is
                // available.
                let route = match func(&ctx.manifest, "event_source") {
                    Some(router) => router
                        .call::<Value>(event_table.clone())
                        .map_err(|e| lua_err("event_source", e))?,
                    None => Value::Nil,
                };
                if matches!(route, Value::Boolean(false)) {
                    log::trace!("[plugin worker] event_source dropped a report (reply/ack)");
                    continue;
                }
                log::trace!("[plugin worker] event_source route={route:?}");
                let targets = match route {
                    Value::Integer(0) => vec![(None, dev.clone())],
                    Value::Integer(index) if index > 0 => u32::try_from(index)
                        .ok()
                        .and_then(|index| {
                            ctx.child_devs
                                .borrow()
                                .get(&index)
                                .cloned()
                                .map(|child| vec![(Some(index), child)])
                        })
                        .unwrap_or_default(),
                    _ => vec![(None, dev.clone())],
                };
                if targets.is_empty() {
                    log::trace!(
                        "[plugin worker] event route={route:?} matched no child dev; report dropped"
                    );
                }
                for (child_index, target) in targets {
                    let value: Value = match callback.call((target, event_table.clone())) {
                        Ok(value) => value,
                        Err(error) => {
                            log::warn!("plugin event callback failed: {error}");
                            continue;
                        }
                    };
                    if !matches!(value, Value::Nil) {
                        let mut outcome: PollOutcome = match ctx.lua.from_value(value) {
                            Ok(outcome) => outcome,
                            Err(error) => {
                                log::warn!("plugin event returned a malformed result: {error}");
                                continue;
                            }
                        };
                        outcome.child_index = outcome.child_index.or(child_index);
                        outcomes.push(outcome);
                    }
                }
            }
            Ok(outcomes)
        })
        .await
    }

    pub async fn detect_accessories(&self) -> Result<Vec<DetectedAccessory>> {
        self.call_opt("detect_accessories").await
    }

    pub async fn write_ext_frame(&self, channel: &str, colors: &[RgbColor]) -> Result<()> {
        let channel = channel.to_owned();
        let colors = colors.to_vec();
        self.run(move |ctx, dev, _| {
            let f = required(&ctx.manifest, "write_ext_frame")?;
            let colors_v = ctx
                .lua
                .to_value(&colors)
                .map_err(|e| lua_err("write_ext_frame arg", e))?;
            f.call::<()>((dev, channel, colors_v))
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
                validate_component("controller zone id", &z.id)?;
                validate_short_text("controller zone name", &z.name)?;
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
        self.run(move |ctx, dev, _| {
            let f = required(&ctx.manifest, "lcd_stream_frame")?;
            let buf = ctx
                .lua
                .create_userdata(ByteBuf::from_bytes(rgba))
                .map_err(|e| lua_err("lcd_stream_frame arg", e))?;
            f.call::<()>((dev, buf, width, height, rotation, raw, brightness))
                .map_err(|e| lua_err("lcd_stream_frame", e))
        })
        .await
    }

    pub async fn lcd_set_image(&self, data: Vec<u8>, rotation: u32) -> Result<()> {
        self.run(move |ctx, dev, _| {
            let f = required(&ctx.manifest, "set_image")?;
            let buf = ctx
                .lua
                .create_userdata(ByteBuf::from_bytes(data))
                .map_err(|e| lua_err("set_image arg", e))?;
            f.call::<()>((dev, buf, rotation))
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

    pub async fn dpi_set_steps(&self, steps: &[u16]) -> Result<()> {
        self.call("set_dpi_steps", steps.to_vec()).await
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
        self.run(move |ctx, dev, _| {
            let f = required(&ctx.manifest, "set_button_mapping")?;
            let mapping_v = ctx
                .lua
                .to_value(&mapping)
                .map_err(|e| lua_err("set_button_mapping arg", e))?;
            f.call::<()>((dev, mapping_v))
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
        self.run(|ctx, dev, _| {
            let Some(f) = func(&ctx.manifest, "key_remap_host_mode") else {
                return Ok(true);
            };
            Ok(match f.call::<bool>(dev) {
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
    module_sources: &std::collections::BTreeMap<String, String>,
    transport: PluginIo,
    dev_match: DevMatch,
    granted: &[Permission],
    config: &crate::plugin::ResolvedConfig,
    handle: Handle,
    zones: &[RgbZone],
    audio_registry: super::audio_api::SinkRegistry,
    data: super::data_api::DataRuntime,
) -> Result<WorkerCtx> {
    debug_assert!(!super::contract::active().tables.is_empty());
    let controller_index = dev_match.index;
    let (lua, budget) = sandbox::bootstrap_vm(
        granted,
        config,
        PLUGIN_VM_MEMORY_BYTES,
        PLUGIN_INSTRUCTION_BUDGET,
    )
    .map_err(|e| lua_err("sandbox setup", e))?;
    sandbox::install_package_modules(&lua, module_sources)
        .map_err(|e| lua_err("package modules", e))?;
    super::data_api::register(&lua, data).map_err(|e| lua_err("data API", e))?;

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
    let api = TransportApi::new(transport.clone(), handle.clone());
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
        transport,
        handle,
        dev,
        child_devs: RefCell::new(HashMap::new()),
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
            let result = command.run(&executable, &args).map_err(to_lua_err)?;
            command_result_table(lua, &result)
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
    module_sources: &std::collections::BTreeMap<String, String>,
    bus: Arc<SmBusDevice>,
    scope_addrs: Vec<u8>,
    granted: &[Permission],
    handle: Handle,
) -> Result<()> {
    let source = source.to_owned();
    let module_sources = module_sources.clone();
    let granted = granted.to_vec();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("halod-pre-scan".into())
        .spawn(move || {
            let _ = tx.send(run_pre_scan_inner(
                &source,
                &module_sources,
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
    module_sources: &std::collections::BTreeMap<String, String>,
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
        granted,
        &HashMap::new(),
        PLUGIN_VM_MEMORY_BYTES,
        PLUGIN_INSTRUCTION_BUDGET,
    )
    .map_err(|e| lua_err("sandbox setup", e))?;
    sandbox::install_package_modules(&lua, module_sources)
        .map_err(|e| lua_err("package modules", e))?;
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
    // Extension metadata is least authoritative: write it first so canonical
    // routing fields below cannot be overwritten by a colliding `extra` key.
    for (key, value) in &m.extra {
        t.set(key.as_str(), *value)
            .map_err(|e| lua_err("match extra", e))?;
    }
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
    if let Some(key) = &m.key {
        t.set("key", key.clone())
            .map_err(|e| lua_err("match.key", e))?;
    }
    if let Some(name) = &m.name {
        t.set("name", name.clone())
            .map_err(|e| lua_err("match.name", e))?;
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::transports::mock::test_transport::MockTransport;
    use crate::drivers::transports::{HidTransport, Transport, TransportEvent};

    struct TestEventTransport {
        events: std::sync::Mutex<Vec<TransportEvent>>,
    }

    #[async_trait::async_trait]
    impl Transport for TestEventTransport {
        async fn write(&self, _data: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn read(&self, _size: usize) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn as_hid(&self) -> Option<&dyn HidTransport> {
            Some(self)
        }
        fn rate_status(&self) -> halod_shared::types::WriteRateStatus {
            Default::default()
        }
        fn set_write_rate_limit(&self, _limit: Option<halod_shared::types::WriteRateLimit>) {}
    }

    #[async_trait::async_trait]
    impl HidTransport for TestEventTransport {
        async fn feature_exchange(&self, _data: &[u8], _size: usize) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn defer_event(&self, _data: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn write_companion(&self, _data: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn read_companion(&self, _size: usize) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn drain_events(&self, limit: usize) -> Result<Vec<TransportEvent>> {
            let mut events = self.events.lock().unwrap();
            let count = events.len().min(limit);
            Ok(events.drain(..count).collect())
        }
    }

    fn stream_io() -> PluginIo {
        PluginIo::Stream {
            transport: Arc::new(MockTransport::empty()),
            usb: None,
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
            Default::default(),
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
            Default::default(),
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
    async fn dynamic_children_share_one_vm_and_keep_distinct_dev_tables() {
        let root = spawn(
            r#"
                local initialized = 0
                return {
                    initialize = function(dev)
                        initialized = initialized + 1
                        dev.local_count = (dev.local_count or 0) + 1
                        return { ok = true, model = tostring(initialized) }
                    end,
                    get_duty = function(dev)
                        return dev.local_count + dev.match.index
                    end,
                }
            "#,
            vec![],
        );
        let child_one = root.child(DevMatch {
            transport: "hid".into(),
            index: Some(1),
            key: Some("one".into()),
            ..Default::default()
        });
        let child_two = root.child(DevMatch {
            transport: "hid".into(),
            index: Some(2),
            key: Some("two".into()),
            ..Default::default()
        });

        assert_eq!(root.initialize().await.unwrap().model.as_deref(), Some("1"));
        assert_eq!(
            child_one.initialize().await.unwrap().model.as_deref(),
            Some("2")
        );
        assert_eq!(
            child_two.initialize().await.unwrap().model.as_deref(),
            Some("3")
        );
        assert_eq!(child_one.fan_get_duty().await.unwrap(), 2);
        assert_eq!(child_two.fan_get_duty().await.unwrap(), 3);
        child_one.close().await.unwrap();
        assert_eq!(child_two.fan_get_duty().await.unwrap(), 3);
        root.close().await.unwrap();
    }

    #[tokio::test]
    async fn transport_events_route_to_a_child_on_the_root_worker() {
        let transport = Arc::new(TestEventTransport {
            events: std::sync::Mutex::new(vec![TransportEvent {
                endpoint: "primary",
                data: vec![0x10, 2, 0xaa],
            }]),
        });
        let root = PluginHandle::spawn(
            r#"
                return {
                    initialize = function(_) return true end,
                    event_source = function(event) return event.report:byte(2) end,
                    event = function(dev, event)
                        if dev.match.index == event.report:byte(2) then
                            return { button_events = { pressed = { 7 }, released = {} } }
                        end
                    end,
                }
            "#
            .to_owned(),
            Default::default(),
            PluginIo::Stream {
                transport,
                usb: None,
            },
            DevMatch {
                transport: "hid".into(),
                ..Default::default()
            },
            vec![],
            HashMap::new(),
            Handle::current(),
            Vec::new(),
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        );
        let child = root.child(DevMatch {
            transport: "hid".into(),
            index: Some(2),
            ..Default::default()
        });
        child.initialize().await.unwrap();

        let outcomes = root.on_transport_events().await.unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].child_index, Some(2));
        assert_eq!(outcomes[0].button_events.pressed, vec![7]);
        root.close().await.unwrap();
    }

    #[tokio::test]
    async fn event_source_outside_u32_is_dropped() {
        let transport = Arc::new(TestEventTransport {
            events: std::sync::Mutex::new(vec![TransportEvent {
                endpoint: "primary",
                data: vec![0x10],
            }]),
        });
        let root = PluginHandle::spawn(
            r#"return {
                event_source = function(_) return 4294967296 end,
                event = function(_) return { state_changed = true } end,
            }"#
            .to_owned(),
            Default::default(),
            PluginIo::Stream {
                transport,
                usb: None,
            },
            DevMatch {
                transport: "hid".into(),
                ..Default::default()
            },
            vec![],
            HashMap::new(),
            Handle::current(),
            Vec::new(),
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        );
        assert!(root.on_transport_events().await.unwrap().is_empty());
        root.close().await.unwrap();
    }

    #[tokio::test]
    async fn canonical_match_fields_override_colliding_extra_keys() {
        let h = PluginHandle::spawn(
            "return { initialize = function(dev) return { model = tostring(dev.match.index) } end }"
                .to_owned(),
            Default::default(),
            stream_io(),
            DevMatch {
                transport: "hid".into(),
                index: Some(7),
                extra: HashMap::from([("index".into(), 99)]),
                ..Default::default()
            },
            vec![],
            HashMap::new(),
            Handle::current(),
            Vec::new(),
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        );
        assert_eq!(h.initialize().await.unwrap().model.as_deref(), Some("7"));
        h.close().await.unwrap();
    }

    #[tokio::test]
    async fn poll_surfaces_read_status_errors() {
        let h = spawn(
            "return { read_status = function() error('broken') end }",
            vec![],
        );
        let error = h.poll().await.unwrap_err();
        assert!(error.to_string().contains("read_status"), "{error:#}");
        h.close().await.unwrap();
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
        h.close().await.unwrap();

        // After close the worker loop has ended, so a further call fails rather
        // than hanging.
        assert!(h.rgb_apply(static_state()).await.is_err());
    }

    #[tokio::test]
    async fn poll_extracts_button_events_without_exposing_host_input_to_lua() {
        let h = spawn(
            r#"return {
                read_status = function(dev)
                  return { button_events = { pressed = { 3, 7 }, released = { 2 } } }
                end,
            }"#,
            vec![],
        );
        let events = h.poll().await.unwrap().button_events;
        assert_eq!(events.pressed, vec![3, 7]);
        assert_eq!(events.released, vec![2]);
        h.close().await.unwrap();
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
        // A plugin with no `apply` identifies the active ABI in its error.
        let h = spawn("return {}", vec![]);
        let err = h.rgb_apply(static_state()).await.unwrap_err();
        assert!(
            err.to_string().contains(&format!(
                "plugin API {} has no apply()",
                crate::plugin::PLUGIN_API
            )),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn routed_child_uses_standard_rgb_callbacks_and_match_index() {
        let source = r#"return {
            apply = function(dev, state)
                assert(dev.match.index == 7)
            end,
            write_frame = function(dev, zone, colors, led_ids)
                assert(dev.match.index == 7)
                assert(zone == "zone-1")
                assert(#colors == 1)
                assert(led_ids[1] == 42)
            end,
        }"#;
        let h = spawn_controller(source, 7);

        h.rgb_apply(static_state()).await.unwrap();
        h.rgb_write_frame("zone-1", &[RgbColor { r: 1, g: 2, b: 3 }], &[42])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn integration_controller_batches_all_zones_in_one_callback() {
        let source = r#"return {
            write_frame_batch = function(dev, frames)
                assert(dev.match.index == 7)
                assert(#frames == 2)
                assert(frames[1].zone_id == "zone-1")
                assert(#frames[1].colors == 1)
                assert(frames[1].colors[1].r == 1)
                assert(frames[1].led_ids[1] == 42)
                assert(frames[2].zone_id == "zone-2")
                assert(frames[2].colors[1].b == 6)
                assert(frames[2].led_ids[1] == 84)
            end,
        }"#;
        let h = spawn_controller(source, 7);

        h.rgb_write_frame_batch(&[
            (
                "zone-1".into(),
                vec![RgbColor { r: 1, g: 2, b: 3 }],
                vec![42],
            ),
            (
                "zone-2".into(),
                vec![RgbColor { r: 4, g: 5, b: 6 }],
                vec![84],
            ),
        ])
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn frame_batch_falls_back_to_per_zone_callback() {
        let source = r#"local calls = 0
        return {
            write_frame = function(dev, zone, colors, led_ids)
                calls = calls + 1
                assert(dev.match.index == 7)
                assert(zone == "zone-" .. calls)
                assert(led_ids[1] == calls * 42)
            end,
        }"#;
        let h = spawn_controller(source, 7);

        h.rgb_write_frame_batch(&[
            (
                "zone-1".into(),
                vec![RgbColor { r: 1, g: 2, b: 3 }],
                vec![42],
            ),
            (
                "zone-2".into(),
                vec![RgbColor { r: 4, g: 5, b: 6 }],
                vec![84],
            ),
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

    #[tokio::test]
    async fn initialize_reports_runtime_dpi_descriptor() {
        let h = spawn(
            r#"return { initialize = function(dev)
                return { dpi = { min = 100, max = 3200, steps = { 400, 800, 1600 }, onboard = true } }
            end }"#,
            vec![],
        );
        let dpi = h.initialize().await.unwrap().dpi.expect("dpi descriptor");
        assert_eq!(
            (dpi.min, dpi.max, dpi.steps, dpi.onboard),
            (100, 3200, vec![400, 800, 1600], true)
        );
    }

    #[tokio::test]
    async fn dynamic_controller_preserves_transport_specific_extra_fields() {
        let h = spawn(
            r#"return { enumerate_controllers = function(dev)
                return {{ index = 0, name = "Fan 1", key = "0", extra = {
                  chip_id = 0xd4, revision = 0x51, hwm_base = 0x290, slot = 1,
                } }}
            end }"#,
            vec![],
        );
        let children = h.enumerate_controllers().await.unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].extra.get("chip_id"), Some(&0xd4));
        assert_eq!(children[0].extra.get("revision"), Some(&0x51));
        assert_eq!(children[0].extra.get("hwm_base"), Some(&0x290));
        assert_eq!(children[0].extra.get("slot"), Some(&1));
        h.close().await.unwrap();
    }
}
