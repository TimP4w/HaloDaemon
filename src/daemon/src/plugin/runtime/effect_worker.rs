// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-instance worker thread for a plugin-declared RGB effect. Mirrors
//! `worker.rs`'s per-device pattern, minus the transport: an effect is pure
//! compute, so its VM never gets a `TransportApi`. The Lua contract is
//! frame-batched — one render call per instance per frame (pixmap) or per
//! zone per frame (direct) — never per LED.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::ops::ControlFlow;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use mlua::{Function, Lua, LuaSerdeExt, Table};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use halod_shared::types::{EffectParamValue, Permission};

use super::bytebuf::ByteBuf;
use super::lua_worker::LuaWorker;
use super::sandbox;
use super::{PLUGIN_INSTRUCTION_BUDGET, PLUGIN_VM_MEMORY_BYTES};

pub const CANVAS_W: u32 = 400;
pub const CANVAS_H: u32 = 300;

/// One LED's stable identity and chain/spatial coordinates, passed to a direct
/// effect's `led_effect_<id>` callback in one batch.
#[derive(Debug, Clone, Serialize)]
pub struct LedCoord {
    pub id: u32,
    pub zone_id: String,
    pub p: f32,
    pub p_ring: f32,
    pub nx: f32,
    pub ny: f32,
}

#[derive(Clone)]
pub struct EffectFrameInput {
    pub time: f32,
    pub dt: f32,
    pub frame: u64,
    pub audio: crate::services::audio::SpectrumFrame,
    pub sensors: Arc<HashMap<String, f64>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectZoneInput {
    pub id: String,
    pub topology: String,
    pub led_count: usize,
    pub device_id: String,
}

impl EffectZoneInput {
    fn canvas() -> Self {
        Self {
            id: "canvas".into(),
            topology: "canvas".into(),
            led_count: 0,
            device_id: String::new(),
        }
    }
}

/// A linear-light RGB triple returned by a direct effect; clamped to
/// `0.0..=1.0` before it ever leaves this module.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct PluginLedColor {
    #[serde(default)]
    pub r: f32,
    #[serde(default)]
    pub g: f32,
    #[serde(default)]
    pub b: f32,
}

impl PluginLedColor {
    fn clamp(self) -> Self {
        Self {
            r: self.r.clamp(0.0, 1.0),
            g: self.g.clamp(0.0, 1.0),
            b: self.b.clamp(0.0, 1.0),
        }
    }
}

enum EffectCall {
    RenderPixmap {
        frame: EffectFrameInput,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    LedColors {
        leds: Vec<LedCoord>,
        frame: EffectFrameInput,
        zone: EffectZoneInput,
        reply: oneshot::Sender<Result<Vec<PluginLedColor>>>,
    },
}

/// The Lua VM plus the resolved callbacks and params, owned by the worker
/// thread. Effects are pure compute, so there is no transport here.
struct EffectCtx {
    lua: Lua,
    render_fn: Option<Function>,
    led_colors_fn: Option<Function>,
    params_v: mlua::Value,
    seed: u32,
    /// Instruction counter for the runaway-guard hook; reset before each call.
    budget: Rc<Cell<u64>>,
    data: super::data_api::DataRuntime,
}

/// Handle a live effect instance's engine passes hold. The inner [`LuaWorker`]
/// is `Send + Sync`, so it can sit in `LivePixmap`/`LiveDirect`. Dropping it
/// ends the worker (the channel closes).
#[derive(Clone)]
pub struct PluginEffectHandle(LuaWorker<EffectCall>);

impl PluginEffectHandle {
    /// Spawn the worker thread for one live effect instance. `effect_id` is
    /// the plugin-local id (not the namespaced catalog id) — it drives the
    /// `render_effect_<id>` / `led_effect_<id>` callback lookup.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn spawn(
        script_source: String,
        module_sources: std::collections::BTreeMap<String, String>,
        effect_id: String,
        params: HashMap<String, EffectParamValue>,
        granted: Vec<Permission>,
        config: crate::plugin::ResolvedConfig,
    ) -> Self {
        Self::spawn_with_data(
            script_source,
            module_sources,
            effect_id,
            params,
            granted,
            config,
            Default::default(),
        )
    }

    pub fn spawn_with_data(
        script_source: String,
        module_sources: std::collections::BTreeMap<String, String>,
        effect_id: String,
        params: HashMap<String, EffectParamValue>,
        granted: Vec<Permission>,
        config: crate::plugin::ResolvedConfig,
        data: super::data_api::DataRuntime,
    ) -> Self {
        let worker = LuaWorker::spawn(
            "halod-effect",
            "effect",
            // An effect render must finish well inside a frame; a wedged one is
            // killed fast so it can't stall the engine tick every frame.
            std::time::Duration::from_secs(2),
            move || {
                build_ctx(
                    &script_source,
                    &module_sources,
                    &effect_id,
                    &params,
                    &granted,
                    &config,
                    data.clone(),
                )
            },
            |call, ctx: &EffectCtx| {
                ctx.budget.set(0);
                super::sandbox::set_call_deadline(&ctx.lua, std::time::Duration::from_secs(2));
                match call {
                    EffectCall::RenderPixmap { frame, reply } => {
                        let _ = reply.send(run_render_pixmap(
                            &ctx.lua,
                            ctx.render_fn.as_ref(),
                            &frame,
                            &ctx.params_v,
                            ctx.seed,
                            &ctx.data,
                        ));
                    }
                    EffectCall::LedColors {
                        leds,
                        frame,
                        zone,
                        reply,
                    } => {
                        let _ = reply.send(run_led_colors(
                            &ctx.lua,
                            ctx.led_colors_fn.as_ref(),
                            leds,
                            &frame,
                            &zone,
                            &ctx.params_v,
                            ctx.seed,
                            &ctx.data,
                        ));
                    }
                }
                ControlFlow::Continue(())
            },
        )
        .unwrap_or_else(|e| {
            log::error!("effect worker not started: {e:#}");
            LuaWorker::dead("effect")
        });
        Self(worker)
    }

    /// Fill a `CANVAS_W * CANVAS_H * 4` linear-RGBA buffer.
    pub async fn render_pixmap(&self, frame: EffectFrameInput) -> Result<Vec<u8>> {
        self.0
            .request(|reply| EffectCall::RenderPixmap { frame, reply })
            .await?
    }

    /// Compute one color per LED coordinate, order preserved. `sensor` is the
    /// live reading for the effect's declared `sensor` param, if any.
    pub async fn led_colors(
        &self,
        leds: Vec<LedCoord>,
        frame: EffectFrameInput,
        zone: EffectZoneInput,
    ) -> Result<Vec<PluginLedColor>> {
        self.0
            .request(|reply| EffectCall::LedColors {
                leds,
                frame,
                zone,
                reply,
            })
            .await?
    }
}

fn lua_err(context: &str, e: mlua::Error) -> anyhow::Error {
    anyhow!("effect {context}: {e}")
}

/// HSV (h,s,v each 0..1) to sRGB bytes. Used only to back `halod.hsv` for
/// effect scripts — not shared with the engine's own color math.
fn hsv_to_srgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let h = h.rem_euclid(1.0) * 6.0;
    let sector = h as i32;
    let f = h - sector as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    let (r, g, b) = match sector.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    (
        (r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (b.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn seeded_random(seed: u32, stream: i64) -> f64 {
    let bits = splitmix64(u64::from(seed) ^ stream as u64) >> 11;
    bits as f64 / (1u64 << 53) as f64
}

fn noise_axis(value: f64) -> (i64, u32) {
    let base = value.floor();
    let fraction = ((value - base) * 65_536.0).floor().clamp(0.0, 65_535.0) as u32;
    (base as i64, fraction)
}

fn noise_lattice(seed: u32, x: i64, y: i64) -> u32 {
    let key = u64::from(seed)
        ^ (x as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (y as u64).wrapping_mul(0xd1b5_4a32_d192_ed03);
    (splitmix64(key) >> 32) as u32
}

fn fixed_lerp(a: u32, b: u32, fraction: u32) -> u32 {
    let inverse = 65_536u64 - u64::from(fraction);
    ((u64::from(a) * inverse + u64::from(b) * u64::from(fraction)) >> 16) as u32
}

fn seeded_noise_1d(seed: u32, x: f64) -> f64 {
    let (base, fraction) = noise_axis(x);
    let value = fixed_lerp(
        noise_lattice(seed, base, 0),
        noise_lattice(seed, base.saturating_add(1), 0),
        fraction,
    );
    f64::from(value) / f64::from(u32::MAX)
}

fn seeded_noise_2d(seed: u32, x: f64, y: f64) -> f64 {
    let (base_x, fraction_x) = noise_axis(x);
    let (base_y, fraction_y) = noise_axis(y);
    let top = fixed_lerp(
        noise_lattice(seed, base_x, base_y),
        noise_lattice(seed, base_x.saturating_add(1), base_y),
        fraction_x,
    );
    let bottom = fixed_lerp(
        noise_lattice(seed, base_x, base_y.saturating_add(1)),
        noise_lattice(seed, base_x.saturating_add(1), base_y.saturating_add(1)),
        fraction_x,
    );
    f64::from(fixed_lerp(top, bottom, fraction_y)) / f64::from(u32::MAX)
}

#[derive(Clone, Copy, Deserialize)]
struct HelperColor {
    r: f64,
    g: f64,
    b: f64,
}

fn finite_number(value: f64, name: &str) -> mlua::Result<f64> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(mlua::Error::RuntimeError(format!(
            "effect helper {name} requires finite values"
        )))
    }
}

fn helper_color(lua: &Lua, color: HelperColor) -> mlua::Result<Table> {
    for value in [color.r, color.g, color.b] {
        finite_number(value, "color")?;
    }
    let table = lua.create_table()?;
    table.set("r", color.r)?;
    table.set("g", color.g)?;
    table.set("b", color.b)?;
    Ok(table)
}

fn lerp_helper_color(a: HelperColor, b: HelperColor, amount: f64) -> HelperColor {
    let amount = amount.clamp(0.0, 1.0);
    HelperColor {
        r: a.r + (b.r - a.r) * amount,
        g: a.g + (b.g - a.g) * amount,
        b: a.b + (b.b - a.b) * amount,
    }
}

fn srgb_to_linear_unit(value: f64) -> f64 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb_unit(value: f64) -> f64 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

/// The small global helper surface effect scripts get beyond the base sandbox.
/// Frame-varying data and deterministic helpers live on the effect context.
fn register_effect_helpers(lua: &Lua) -> mlua::Result<()> {
    let halod: Table = lua.globals().get("halod")?;
    halod.set("canvas_w", CANVAS_W)?;
    halod.set("canvas_h", CANVAS_H)?;
    halod.set(
        "hsv",
        lua.create_function(|_, (h, s, v): (f32, f32, f32)| Ok(hsv_to_srgb(h, s, v)))?,
    )?;
    Ok(())
}

fn effect_context_table(
    lua: &Lua,
    frame: &EffectFrameInput,
    params: &mlua::Value,
    seed: u32,
    zone: &EffectZoneInput,
    data: &super::data_api::DataRuntime,
) -> mlua::Result<Table> {
    finite_number(f64::from(frame.time), "time")?;
    finite_number(f64::from(frame.dt), "dt")?;
    let context = lua.create_table()?;
    context.set("time", frame.time)?;
    context.set("dt", frame.dt)?;
    context.set("params", params.clone())?;
    context.set("frame", frame.frame)?;
    context.set("seed", seed)?;
    context.set("zone", lua.to_value(zone)?)?;
    context.set("sensors", lua.to_value(frame.sensors.as_ref())?)?;
    let data = data.clone();
    context.set(
        "data",
        lua.create_function(move |lua, (_self, key): (Table, String)| {
            if !data.consumes.iter().any(|scope| {
                scope == &key || (scope == "host.sensors.*" && key.starts_with("host.sensors."))
            }) {
                return Err(mlua::Error::RuntimeError(format!(
                    "data key '{key}' is not declared in consumes"
                )));
            }
            crate::services::data_bus::snapshot_to_lua(lua, &data.bus.read(&key))
        })?,
    )?;

    let bands = lua.create_sequence_from(frame.audio.bands.iter().copied())?;
    let audio = lua.create_table()?;
    audio.set("level", frame.audio.level)?;
    audio.set("flux", frame.audio.flux)?;
    audio.set("beat", frame.audio.beat)?;
    audio.set("seq", frame.audio.seq)?;
    audio.set("bands", bands)?;
    context.set("audio", audio)?;

    context.set(
        "random",
        lua.create_function(move |_, (_self, stream): (Table, Option<i64>)| {
            Ok(seeded_random(seed, stream.unwrap_or(0)))
        })?,
    )?;
    context.set(
        "noise1d",
        lua.create_function(move |_, (_self, x): (Table, f64)| {
            finite_number(x, "noise1d")?;
            Ok(seeded_noise_1d(seed, x))
        })?,
    )?;
    context.set(
        "noise2d",
        lua.create_function(move |_, (_self, x, y): (Table, f64, f64)| {
            finite_number(x, "noise2d")?;
            finite_number(y, "noise2d")?;
            Ok(seeded_noise_2d(seed, x, y))
        })?,
    )?;
    context.set(
        "lerp_color",
        lua.create_function(
            |lua, (_self, a, b, amount): (Table, mlua::Value, mlua::Value, f64)| {
                finite_number(amount, "lerp_color")?;
                let a: HelperColor = lua.from_value(a)?;
                let b: HelperColor = lua.from_value(b)?;
                helper_color(lua, lerp_helper_color(a, b, amount))
            },
        )?,
    )?;
    context.set(
        "gradient",
        lua.create_function(|lua, (_self, stops, amount): (Table, Table, f64)| {
            finite_number(amount, "gradient")?;
            let count = stops.raw_len();
            if !(2..=16).contains(&count) {
                return Err(mlua::Error::RuntimeError(
                    "effect gradients require 2..=16 stops".into(),
                ));
            }
            let mut parsed = Vec::with_capacity(count);
            for index in 1..=count {
                let stop: Table = stops.raw_get(index)?;
                let at = finite_number(stop.get("at")?, "gradient")?;
                let color: HelperColor = lua.from_value(stop.get("color")?)?;
                parsed.push((at, color));
            }
            parsed.sort_by(|a, b| a.0.total_cmp(&b.0));
            let before = parsed.first().copied().unwrap();
            let after = parsed.last().copied().unwrap();
            let (left, right) = parsed
                .windows(2)
                .find(|pair| amount >= pair[0].0 && amount <= pair[1].0)
                .map(|pair| (pair[0], pair[1]))
                .unwrap_or(if amount < before.0 {
                    (before, before)
                } else {
                    (after, after)
                });
            let span = right.0 - left.0;
            let position = if span.abs() <= f64::EPSILON {
                0.0
            } else {
                (amount - left.0) / span
            };
            helper_color(lua, lerp_helper_color(left.1, right.1, position))
        })?,
    )?;
    context.set(
        "srgb_to_linear",
        lua.create_function(|_, (_self, value): (Table, f64)| {
            finite_number(value, "srgb_to_linear")?;
            Ok(srgb_to_linear_unit(value))
        })?,
    )?;
    context.set(
        "linear_to_srgb",
        lua.create_function(|_, (_self, value): (Table, f64)| {
            finite_number(value, "linear_to_srgb")?;
            Ok(linear_to_srgb_unit(value))
        })?,
    )?;
    Ok(context)
}

fn stable_effect_seed(effect_id: &str, params: &HashMap<String, EffectParamValue>) -> u32 {
    let sorted: BTreeMap<_, _> = params.iter().collect();
    let mut hash = 0x811c_9dc5u32;
    for byte in effect_id
        .as_bytes()
        .iter()
        .copied()
        .chain([0xff])
        .chain(serde_json::to_vec(&sorted).unwrap_or_default())
    {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Build the effect worker's VM context on the worker thread. Runs once at
/// spawn; the [`LuaWorker`] loop then drives calls against the returned
/// [`EffectCtx`].
fn build_ctx(
    source: &str,
    module_sources: &std::collections::BTreeMap<String, String>,
    effect_id: &str,
    params: &HashMap<String, EffectParamValue>,
    granted: &[Permission],
    config: &crate::plugin::ResolvedConfig,
    data: super::data_api::DataRuntime,
) -> Result<EffectCtx> {
    let (lua, budget) = sandbox::bootstrap_vm(
        granted,
        config,
        PLUGIN_VM_MEMORY_BYTES,
        PLUGIN_INSTRUCTION_BUDGET,
    )
    .map_err(|e| lua_err("sandbox setup", e))?;
    sandbox::install_package_modules(&lua, module_sources)
        .map_err(|e| lua_err("package modules", e))?;
    register_effect_helpers(&lua).map_err(|e| lua_err("effect helpers", e))?;
    super::data_api::register(&lua, data.clone()).map_err(|e| lua_err("data API", e))?;

    let manifest: Table = lua
        .load(source)
        .eval()
        .map_err(|e| lua_err("script evaluation", e))?;
    let render_fn: Option<Function> = manifest.get(format!("render_effect_{effect_id}")).ok();
    let led_colors_fn: Option<Function> = manifest.get(format!("led_effect_{effect_id}")).ok();

    let params_v = lua
        .to_value(params)
        .map_err(|e| lua_err("params table", e))?;

    Ok(EffectCtx {
        lua,
        render_fn,
        led_colors_fn,
        params_v,
        seed: stable_effect_seed(effect_id, params),
        budget,
        data,
    })
}

fn run_render_pixmap(
    lua: &Lua,
    f: Option<&Function>,
    frame: &EffectFrameInput,
    params: &mlua::Value,
    seed: u32,
    data: &super::data_api::DataRuntime,
) -> Result<Vec<u8>> {
    let f = f.ok_or_else(|| anyhow!("effect has no render_effect_<id>() callback"))?;
    let buf = ByteBuf::from_bytes(vec![0u8; (CANVAS_W * CANVAS_H * 4) as usize]);
    let ud = lua
        .create_userdata(buf)
        .map_err(|e| lua_err("pixmap buffer", e))?;
    let context = effect_context_table(lua, frame, params, seed, &EffectZoneInput::canvas(), data)
        .map_err(|e| lua_err("context", e))?;
    f.call::<()>((ud.clone(), context))
        .map_err(|e| lua_err("render", e))?;
    let bytes = ud
        .borrow::<ByteBuf>()
        .map_err(|e| lua_err("pixmap buffer readback", e))?
        .as_slice()
        .to_vec();
    Ok(bytes)
}

fn run_led_colors(
    lua: &Lua,
    f: Option<&Function>,
    leds: Vec<LedCoord>,
    frame: &EffectFrameInput,
    zone: &EffectZoneInput,
    params: &mlua::Value,
    seed: u32,
    data: &super::data_api::DataRuntime,
) -> Result<Vec<PluginLedColor>> {
    let f = f.ok_or_else(|| anyhow!("effect has no led_effect_<id>() callback"))?;
    let n = leds.len();
    let leds_v = lua.to_value(&leds).map_err(|e| lua_err("leds arg", e))?;
    let context = effect_context_table(lua, frame, params, seed, zone, data)
        .map_err(|e| lua_err("context", e))?;
    let value: mlua::Value = f
        .call((leds_v, context))
        .map_err(|e| lua_err("led effect", e))?;
    let raw: Vec<PluginLedColor> = lua
        .from_value(value)
        .map_err(|e| lua_err("led effect result", e))?;
    if raw.len() != n {
        anyhow::bail!("led effect returned {} colors for {} LEDs", raw.len(), n);
    }
    Ok(raw.into_iter().map(PluginLedColor::clamp).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> HashMap<String, EffectParamValue> {
        HashMap::new()
    }

    fn frame(time: f32, frame: u64) -> EffectFrameInput {
        EffectFrameInput {
            time,
            dt: 0.016,
            frame,
            audio: crate::services::audio::SpectrumFrame {
                level: 0.4,
                flux: 0.2,
                beat: true,
                seq: 9,
                ..Default::default()
            },
            sensors: Arc::new([("temp".to_owned(), 80.0)].into_iter().collect()),
        }
    }

    fn zone(id: &str, led_count: usize) -> EffectZoneInput {
        EffectZoneInput {
            id: id.to_owned(),
            topology: "linear".to_owned(),
            led_count,
            device_id: "device".to_owned(),
        }
    }

    fn leds(id: &str, count: usize) -> Vec<LedCoord> {
        let last = count.saturating_sub(1).max(1) as f32;
        (0..count)
            .map(|index| LedCoord {
                id: 100 + index as u32,
                zone_id: id.to_owned(),
                p: index as f32 / last,
                p_ring: index as f32 / last,
                nx: 0.0,
                ny: 0.0,
            })
            .collect()
    }

    #[tokio::test]
    async fn pixmap_render_fills_buffer_with_solid_color() {
        let src = r#"return {
            render_effect_plasma = function(buf, ctx)
                for i = 0, #buf - 1, 4 do
                    buf:set_u8(i, 10)
                    buf:set_u8(i + 1, 20)
                    buf:set_u8(i + 2, 30)
                    buf:set_u8(i + 3, 255)
                end
            end,
        }"#;
        let handle = PluginEffectHandle::spawn(
            src.to_string(),
            Default::default(),
            "plasma".to_string(),
            params(),
            vec![],
            HashMap::new(),
        );
        let bytes = handle.render_pixmap(frame(0.0, 1)).await.unwrap();
        assert_eq!(bytes.len(), (CANVAS_W * CANVAS_H * 4) as usize);
        assert_eq!(&bytes[0..4], &[10, 20, 30, 255]);
        assert_eq!(&bytes[bytes.len() - 4..], &[10, 20, 30, 255]);
    }

    #[tokio::test]
    async fn led_colors_round_trips_order_and_count() {
        let src = r#"return {
            led_effect_comet = function(leds, ctx)
                assert(ctx.zone.id == "ring" and ctx.zone.led_count == 3)
                local out = {}
                for i, led in ipairs(leds) do
                    assert(led.id == 99 + i and led.zone_id == "ring")
                    out[i] = { r = led.p, g = 0, b = 0 }
                end
                return out
            end,
        }"#;
        let handle = PluginEffectHandle::spawn(
            src.to_string(),
            Default::default(),
            "comet".to_string(),
            params(),
            vec![],
            HashMap::new(),
        );
        let colors = handle
            .led_colors(leds("ring", 3), frame(0.0, 1), zone("ring", 3))
            .await
            .unwrap();
        assert_eq!(colors.len(), 3);
        assert_eq!(colors[0].r, 0.0);
        assert_eq!(colors[1].r, 0.5);
        assert_eq!(colors[2].r, 1.0);
    }

    #[tokio::test]
    async fn led_colors_are_clamped_to_unit_range() {
        let src = r#"return {
            led_effect_over = function(leds, ctx)
                return { { r = 5.0, g = -3.0, b = 0.5 } }
            end,
        }"#;
        let handle = PluginEffectHandle::spawn(
            src.to_string(),
            Default::default(),
            "over".to_string(),
            params(),
            vec![],
            HashMap::new(),
        );
        let colors = handle
            .led_colors(leds("ring", 1), frame(0.0, 1), zone("ring", 1))
            .await
            .unwrap();
        assert_eq!(colors[0].r, 1.0);
        assert_eq!(colors[0].g, 0.0);
        assert_eq!(colors[0].b, 0.5);
    }

    #[tokio::test]
    async fn missing_callback_errors_instead_of_panicking() {
        let src = r#"return {}"#;
        let handle = PluginEffectHandle::spawn(
            src.to_string(),
            Default::default(),
            "nope".to_string(),
            params(),
            vec![],
            HashMap::new(),
        );
        assert!(handle.render_pixmap(frame(0.0, 1)).await.is_err());
        assert!(handle
            .led_colors(vec![], frame(0.0, 1), zone("ring", 0))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn script_error_propagates_as_err() {
        let src = r#"return {
            render_effect_boom = function(buf, ctx) error("kaboom") end,
        }"#;
        let handle = PluginEffectHandle::spawn(
            src.to_string(),
            Default::default(),
            "boom".to_string(),
            params(),
            vec![],
            HashMap::new(),
        );
        assert!(handle.render_pixmap(frame(0.0, 1)).await.is_err());
    }

    #[tokio::test]
    async fn instruction_budget_stops_an_infinite_loop() {
        let src = r#"return {
            render_effect_spin = function(buf, ctx)
                while true do end
            end,
        }"#;
        let handle = PluginEffectHandle::spawn(
            src.to_string(),
            Default::default(),
            "spin".to_string(),
            params(),
            vec![],
            HashMap::new(),
        );
        let result = handle.render_pixmap(frame(0.0, 1)).await;
        assert!(result.is_err(), "runaway script must error, not hang");
    }

    /// A tiny pixmap + direction-aware direct effect, standing in for a real
    /// shipped effect plugin (now hosted in the official plugin repo) so the
    /// worker's pixmap/led_colors/param-passing mechanics stay covered here.
    const MINI_FX: &str = r#"return {
        render_effect_plasma = function(buf, ctx)
            local speed = ctx.params.speed or 1.0
            local v = (math.sin(ctx.time * speed) + 1.0) * 0.5
            local byte = math.floor(v * 255)
            for i = 0, #buf - 1, 4 do
                buf:set_u8(i, byte)
                buf:set_u8(i + 1, byte)
                buf:set_u8(i + 2, byte)
                buf:set_u8(i + 3, 255)
            end
        end,
        led_effect_comet = function(leds, ctx)
            local dir = ctx.params.direction or "forward"
            local speed = ctx.params.speed or 1.0
            local head = (ctx.time * speed) % 1.0
            if dir == "backward" then head = 1.0 - head end
            local out = {}
            for i, led in ipairs(leds) do
                local d = math.abs(led.p - head)
                local v = math.max(0.0, 1.0 - d * 4.0)
                out[i] = { r = 0, g = v, b = v }
            end
            return out
        end,
    }"#;

    #[tokio::test]
    async fn mini_effect_plugin_renders_pixmap_and_led_colors_without_error() {
        let pixmap = PluginEffectHandle::spawn(
            MINI_FX.to_string(),
            Default::default(),
            "plasma".to_string(),
            [("speed".to_string(), EffectParamValue::Float(0.8))]
                .into_iter()
                .collect(),
            vec![],
            HashMap::new(),
        );
        let bytes = pixmap.render_pixmap(frame(1.5, 1)).await.unwrap();
        assert_eq!(bytes.len(), (CANVAS_W * CANVAS_H * 4) as usize);
        assert!(
            bytes.iter().any(|&b| b != 0),
            "plasma must not render solid black"
        );

        let direct = PluginEffectHandle::spawn(
            MINI_FX.to_string(),
            Default::default(),
            "comet".to_string(),
            params(),
            vec![],
            HashMap::new(),
        );
        let colors = direct
            .led_colors(leds("ring", 8), frame(0.0, 1), zone("ring", 8))
            .await
            .unwrap();
        assert_eq!(colors.len(), 8);
        assert!(
            colors.iter().any(|c| c.r > 0.0 || c.g > 0.0 || c.b > 0.0),
            "comet must light at least one LED at its head"
        );
    }

    #[tokio::test]
    async fn mini_effect_plugin_comet_direction_reverses_the_sweep() {
        let leds = leds("ring", 8);

        let mut fwd_params = params();
        fwd_params.insert(
            "direction".to_string(),
            EffectParamValue::Str("forward".to_string()),
        );
        fwd_params.insert("speed".to_string(), EffectParamValue::Float(1.0));
        let forward = PluginEffectHandle::spawn(
            MINI_FX.to_string(),
            Default::default(),
            "comet".to_string(),
            fwd_params,
            vec![],
            HashMap::new(),
        );
        let forward_colors = forward
            .led_colors(leds.clone(), frame(0.3, 1), zone("ring", 8))
            .await
            .unwrap();

        let mut back_params = params();
        back_params.insert(
            "direction".to_string(),
            EffectParamValue::Str("backward".to_string()),
        );
        back_params.insert("speed".to_string(), EffectParamValue::Float(1.0));
        let backward = PluginEffectHandle::spawn(
            MINI_FX.to_string(),
            Default::default(),
            "comet".to_string(),
            back_params,
            vec![],
            HashMap::new(),
        );
        let backward_colors = backward
            .led_colors(leds, frame(0.3, 1), zone("ring", 8))
            .await
            .unwrap();

        assert_ne!(
            forward_colors.iter().map(|c| c.g).collect::<Vec<_>>(),
            backward_colors.iter().map(|c| c.g).collect::<Vec<_>>(),
            "forward vs backward direction must sweep the comet head differently"
        );
    }

    #[tokio::test]
    async fn effect_context_and_helpers_are_complete_and_deterministic() {
        let source = r#"return {
            led_effect_probe = function(leds, ctx)
                assert(ctx.time == 1.25 and math.abs(ctx.dt - 0.016) < 0.000001)
                assert(ctx.params.mode == "test" and ctx.frame >= 10)
                assert(type(ctx.seed) == "number")
                assert(math.abs(ctx.audio.level - 0.4) < 0.000001 and ctx.audio.seq == 9)
                assert(ctx.sensors.temp == 80, tostring(ctx.sensors.temp))
                assert(ctx.zone.id == "ring" and ctx.zone.topology == "linear")
                assert(ctx.zone.led_count == 1 and ctx.zone.device_id == "device")
                assert(leds[1].id == 100 and leds[1].zone_id == "ring")

                local mixed = ctx:lerp_color({r=0,g=0,b=0}, {r=1,g=0.5,b=0.25}, 0.5)
                local gradient = ctx:gradient({
                    {at=0,color={r=0,g=0,b=0}},
                    {at=1,color={r=1,g=0.5,b=0.25}},
                }, 0.5)
                assert(math.abs(mixed.g - 0.25) < 1e-12)
                assert(math.abs(gradient.b - 0.125) < 1e-12)
                local linear = ctx:srgb_to_linear(0.5)
                assert(math.abs(ctx:linear_to_srgb(linear) - 0.5) < 1e-12)
                return {{
                    r = ctx:random(42),
                    g = ctx:noise1d(3.25),
                    b = ctx:noise2d(-2.5, 8.75),
                }}
            end,
        }"#;
        let handle = PluginEffectHandle::spawn(
            source.to_owned(),
            Default::default(),
            "probe".to_owned(),
            [("mode".to_owned(), EffectParamValue::Str("test".to_owned()))]
                .into_iter()
                .collect(),
            vec![],
            HashMap::new(),
        );
        let mut first_frame = frame(1.25, 10);
        first_frame.dt = 0.016;
        let first = handle
            .led_colors(leds("ring", 1), first_frame.clone(), zone("ring", 1))
            .await
            .unwrap();
        let mut second_frame = first_frame;
        second_frame.frame = 11;
        let second = handle
            .led_colors(leds("ring", 1), second_frame, zone("ring", 1))
            .await
            .unwrap();
        assert_eq!(first[0].r.to_bits(), second[0].r.to_bits());
        assert_eq!(first[0].g.to_bits(), second[0].g.to_bits());
        assert_eq!(first[0].b.to_bits(), second[0].b.to_bits());
    }

    #[tokio::test]
    async fn one_engine_frame_advances_direct_effect_state_once_across_zones() {
        let source = r#"local state = { frame = nil, advances = 0 }
        return {
            led_effect_stateful = function(leds, ctx)
                if state.frame ~= ctx.frame then
                    state.frame = ctx.frame
                    state.advances = state.advances + 1
                end
                return {{ r = state.advances / 10, g = 0, b = 0 }}
            end,
        }"#;
        let handle = PluginEffectHandle::spawn(
            source.to_owned(),
            Default::default(),
            "stateful".to_owned(),
            params(),
            vec![],
            HashMap::new(),
        );
        let first = handle
            .led_colors(leds("a", 1), frame(1.0, 20), zone("a", 1))
            .await
            .unwrap();
        let same_frame = handle
            .led_colors(leds("b", 1), frame(1.0, 20), zone("b", 1))
            .await
            .unwrap();
        let next_frame = handle
            .led_colors(leds("a", 1), frame(1.1, 21), zone("a", 1))
            .await
            .unwrap();
        assert_eq!(first[0].r, same_frame[0].r);
        assert!(next_frame[0].r > same_frame[0].r);
    }

    #[tokio::test]
    async fn positional_effect_callback_names_are_not_accepted() {
        let source = r#"return {
            render_legacy = function(buf, t, dt, params) end,
            led_colors_legacy = function(leds, t, dt, params) return {} end,
        }"#;
        let handle = PluginEffectHandle::spawn(
            source.to_owned(),
            Default::default(),
            "legacy".to_owned(),
            params(),
            vec![],
            HashMap::new(),
        );
        assert!(handle.render_pixmap(frame(0.0, 1)).await.is_err());
        assert!(handle
            .led_colors(vec![], frame(0.0, 1), zone("ring", 0))
            .await
            .is_err());
    }

    #[test]
    fn deterministic_helpers_have_pinned_outputs() {
        let seed = 0x1234_5678;
        assert_eq!(seeded_random(seed, 42).to_bits(), 0x3fcf_f23e_5763_00c8);
        assert_eq!(seeded_noise_1d(seed, 3.25).to_bits(), 0x3fed_33cc_da7d_33cd);
        assert_eq!(
            seeded_noise_2d(seed, -2.5, 8.75).to_bits(),
            0x3fd6_73c3_b7d6_73c4
        );
    }
}
