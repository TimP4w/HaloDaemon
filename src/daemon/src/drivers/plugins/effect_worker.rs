// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-instance worker thread for a plugin-declared RGB effect. Mirrors
//! `worker.rs`'s per-device pattern, minus the transport: an effect is pure
//! compute, so its VM never gets a `TransportApi`. The Lua contract is
//! frame-batched — one render call per instance per frame (pixmap) or per
//! zone per frame (direct) — never per LED.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::{anyhow, Result};
use mlua::{Function, HookTriggers, Lua, LuaSerdeExt, Table, VmState};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use halod_shared::types::{EffectParamValue, Permission};

use super::bytebuf::ByteBuf;
use super::sandbox;

pub const CANVAS_W: u32 = 400;
pub const CANVAS_H: u32 = 300;

/// A runaway script (accidental infinite loop) errors out instead of
/// stalling the RGB engine tick indefinitely.
const INSTRUCTION_BUDGET: u32 = 50_000_000;

/// One LED's chain/spatial coordinates, passed to a direct effect's
/// `led_colors_<id>` callback in one batch.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct LedCoord {
    pub p: f32,
    pub p_ring: f32,
    pub nx: f32,
    pub ny: f32,
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
        t: f32,
        dt: f32,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    LedColors {
        leds: Vec<LedCoord>,
        t: f32,
        dt: f32,
        reply: oneshot::Sender<Result<Vec<PluginLedColor>>>,
    },
}

/// Handle a live effect instance's engine passes hold. `UnboundedSender` is
/// `Send + Sync`, so it can sit in `LivePixmap`/`LiveDirect`. Dropping it
/// ends the worker (the channel closes).
#[derive(Clone)]
pub struct PluginEffectHandle {
    tx: mpsc::UnboundedSender<EffectCall>,
}

impl PluginEffectHandle {
    /// Spawn the worker thread for one live effect instance. `effect_id` is
    /// the plugin-local id (not the namespaced catalog id) — it drives the
    /// `render_<id>` / `led_colors_<id>` callback lookup.
    pub fn spawn(
        script_source: String,
        effect_id: String,
        params: HashMap<String, EffectParamValue>,
        granted: Vec<Permission>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("halod-effect".into())
            .spawn(move || {
                if let Err(e) = worker_main(&script_source, &effect_id, &params, &granted, rx) {
                    log::error!("effect worker stopped: {e:#}");
                }
            })
            .expect("spawn effect worker thread");
        Self { tx }
    }

    async fn request<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> EffectCall) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .map_err(|_| anyhow!("effect worker is gone"))?;
        rx.await
            .map_err(|_| anyhow!("effect worker dropped the reply"))
    }

    /// Fill a `CANVAS_W * CANVAS_H * 4` linear-RGBA buffer.
    pub async fn render_pixmap(&self, t: f32, dt: f32) -> Result<Vec<u8>> {
        self.request(|reply| EffectCall::RenderPixmap { t, dt, reply })
            .await?
    }

    /// Compute one color per LED coordinate, order preserved.
    pub async fn led_colors(
        &self,
        leds: Vec<LedCoord>,
        t: f32,
        dt: f32,
    ) -> Result<Vec<PluginLedColor>> {
        self.request(|reply| EffectCall::LedColors { leds, t, dt, reply })
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

/// `halod.canvas_w`/`halod.canvas_h`/`halod.hsv` — the small helper surface
/// effect scripts get beyond the base sandbox (`log`, `halod.buffer`).
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

/// Install the runaway-script guard: an instruction-count hook that errors
/// once a single callback call exceeds its budget. The counter is shared
/// with the hook via `Rc<Cell<_>>` (the VM is single-threaded on its own
/// worker thread, so this never crosses threads) and reset before each call.
fn install_budget_hook(lua: &Lua) -> Rc<Cell<u32>> {
    let counter = Rc::new(Cell::new(0u32));
    let hook_counter = counter.clone();
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(10_000),
        move |_, _| {
            let n = hook_counter.get().saturating_add(10_000);
            hook_counter.set(n);
            if n > INSTRUCTION_BUDGET {
                return Err(mlua::Error::RuntimeError(
                    "effect script exceeded its per-frame instruction budget".into(),
                ));
            }
            Ok(VmState::Continue)
        },
    );
    counter
}

fn worker_main(
    source: &str,
    effect_id: &str,
    params: &HashMap<String, EffectParamValue>,
    granted: &[Permission],
    mut rx: mpsc::UnboundedReceiver<EffectCall>,
) -> Result<()> {
    let lua = Lua::new();
    sandbox::apply(&lua, granted).map_err(|e| lua_err("sandbox setup", e))?;
    register_effect_helpers(&lua).map_err(|e| lua_err("effect helpers", e))?;
    let budget = install_budget_hook(&lua);

    let manifest: Table = lua
        .load(source)
        .eval()
        .map_err(|e| lua_err("script evaluation", e))?;
    let render_fn: Option<Function> = manifest.get(format!("render_{effect_id}")).ok();
    let led_colors_fn: Option<Function> = manifest.get(format!("led_colors_{effect_id}")).ok();

    let params_v = lua
        .to_value(params)
        .map_err(|e| lua_err("params table", e))?;

    while let Some(call) = rx.blocking_recv() {
        budget.set(0);
        match call {
            EffectCall::RenderPixmap { t, dt, reply } => {
                let _ = reply.send(run_render_pixmap(
                    &lua,
                    render_fn.as_ref(),
                    t,
                    dt,
                    &params_v,
                ));
            }
            EffectCall::LedColors { leds, t, dt, reply } => {
                let _ = reply.send(run_led_colors(
                    &lua,
                    led_colors_fn.as_ref(),
                    leds,
                    t,
                    dt,
                    &params_v,
                ));
            }
        }
    }
    Ok(())
}

fn run_render_pixmap(
    lua: &Lua,
    f: Option<&Function>,
    t: f32,
    dt: f32,
    params: &mlua::Value,
) -> Result<Vec<u8>> {
    let f = f.ok_or_else(|| anyhow!("effect has no render_<id>() callback"))?;
    let buf = ByteBuf::from_bytes(vec![0u8; (CANVAS_W * CANVAS_H * 4) as usize]);
    let ud = lua
        .create_userdata(buf)
        .map_err(|e| lua_err("pixmap buffer", e))?;
    f.call::<()>((ud.clone(), t, dt, params.clone()))
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
    t: f32,
    dt: f32,
    params: &mlua::Value,
) -> Result<Vec<PluginLedColor>> {
    let f = f.ok_or_else(|| anyhow!("effect has no led_colors_<id>() callback"))?;
    let n = leds.len();
    let leds_v = lua.to_value(&leds).map_err(|e| lua_err("leds arg", e))?;
    let value: mlua::Value = f
        .call((leds_v, t, dt, params.clone()))
        .map_err(|e| lua_err("led_colors", e))?;
    let raw: Vec<PluginLedColor> = lua
        .from_value(value)
        .map_err(|e| lua_err("led_colors result", e))?;
    if raw.len() != n {
        anyhow::bail!("led_colors returned {} colors for {} LEDs", raw.len(), n);
    }
    Ok(raw.into_iter().map(PluginLedColor::clamp).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> HashMap<String, EffectParamValue> {
        HashMap::new()
    }

    #[tokio::test]
    async fn pixmap_render_fills_buffer_with_solid_color() {
        let src = r#"return {
            render_plasma = function(buf, t, dt, params)
                for i = 0, #buf - 1, 4 do
                    buf:set_u8(i, 10)
                    buf:set_u8(i + 1, 20)
                    buf:set_u8(i + 2, 30)
                    buf:set_u8(i + 3, 255)
                end
            end,
        }"#;
        let handle =
            PluginEffectHandle::spawn(src.to_string(), "plasma".to_string(), params(), vec![]);
        let bytes = handle.render_pixmap(0.0, 0.016).await.unwrap();
        assert_eq!(bytes.len(), (CANVAS_W * CANVAS_H * 4) as usize);
        assert_eq!(&bytes[0..4], &[10, 20, 30, 255]);
        assert_eq!(&bytes[bytes.len() - 4..], &[10, 20, 30, 255]);
    }

    #[tokio::test]
    async fn led_colors_round_trips_order_and_count() {
        let src = r#"return {
            led_colors_comet = function(leds, t, dt, params)
                local out = {}
                for i, led in ipairs(leds) do
                    out[i] = { r = led.p, g = 0, b = 0 }
                end
                return out
            end,
        }"#;
        let handle =
            PluginEffectHandle::spawn(src.to_string(), "comet".to_string(), params(), vec![]);
        let leds = vec![
            LedCoord {
                p: 0.0,
                p_ring: 0.0,
                nx: 0.0,
                ny: 0.0,
            },
            LedCoord {
                p: 0.5,
                p_ring: 0.5,
                nx: 0.0,
                ny: 0.0,
            },
            LedCoord {
                p: 1.0,
                p_ring: 1.0,
                nx: 0.0,
                ny: 0.0,
            },
        ];
        let colors = handle.led_colors(leds, 0.0, 0.016).await.unwrap();
        assert_eq!(colors.len(), 3);
        assert_eq!(colors[0].r, 0.0);
        assert_eq!(colors[1].r, 0.5);
        assert_eq!(colors[2].r, 1.0);
    }

    #[tokio::test]
    async fn led_colors_are_clamped_to_unit_range() {
        let src = r#"return {
            led_colors_over = function(leds, t, dt, params)
                return { { r = 5.0, g = -3.0, b = 0.5 } }
            end,
        }"#;
        let handle =
            PluginEffectHandle::spawn(src.to_string(), "over".to_string(), params(), vec![]);
        let colors = handle
            .led_colors(
                vec![LedCoord {
                    p: 0.0,
                    p_ring: 0.0,
                    nx: 0.0,
                    ny: 0.0,
                }],
                0.0,
                0.0,
            )
            .await
            .unwrap();
        assert_eq!(colors[0].r, 1.0);
        assert_eq!(colors[0].g, 0.0);
        assert_eq!(colors[0].b, 0.5);
    }

    #[tokio::test]
    async fn missing_callback_errors_instead_of_panicking() {
        let src = r#"return {}"#;
        let handle =
            PluginEffectHandle::spawn(src.to_string(), "nope".to_string(), params(), vec![]);
        assert!(handle.render_pixmap(0.0, 0.0).await.is_err());
        assert!(handle.led_colors(vec![], 0.0, 0.0).await.is_err());
    }

    #[tokio::test]
    async fn script_error_propagates_as_err() {
        let src = r#"return {
            render_boom = function(buf, t, dt, params) error("kaboom") end,
        }"#;
        let handle =
            PluginEffectHandle::spawn(src.to_string(), "boom".to_string(), params(), vec![]);
        assert!(handle.render_pixmap(0.0, 0.0).await.is_err());
    }

    #[tokio::test]
    async fn instruction_budget_stops_an_infinite_loop() {
        let src = r#"return {
            render_spin = function(buf, t, dt, params)
                while true do end
            end,
        }"#;
        let handle =
            PluginEffectHandle::spawn(src.to_string(), "spin".to_string(), params(), vec![]);
        let result = handle.render_pixmap(0.0, 0.0).await;
        assert!(result.is_err(), "runaway script must error, not hang");
    }

    #[tokio::test]
    async fn shipped_example_effects_plugin_renders_without_error() {
        // Guards the documented example against drift: both callbacks must
        // run clean and produce plausible output.
        let src = include_str!("../../../../../plugins/examples/example_effects.lua");

        let pixmap = PluginEffectHandle::spawn(
            src.to_string(),
            "plasma".to_string(),
            [("speed".to_string(), EffectParamValue::Float(0.8))]
                .into_iter()
                .collect(),
            vec![],
        );
        let bytes = pixmap.render_pixmap(1.5, 0.016).await.unwrap();
        assert_eq!(bytes.len(), (CANVAS_W * CANVAS_H * 4) as usize);
        assert!(
            bytes.iter().any(|&b| b != 0),
            "plasma must not render solid black"
        );

        let direct =
            PluginEffectHandle::spawn(src.to_string(), "comet".to_string(), params(), vec![]);
        let leds: Vec<LedCoord> = (0..8)
            .map(|i| LedCoord {
                p: i as f32 / 7.0,
                p_ring: i as f32 / 7.0,
                nx: 0.0,
                ny: 0.0,
            })
            .collect();
        let colors = direct.led_colors(leds, 0.0, 0.016).await.unwrap();
        assert_eq!(colors.len(), 8);
        assert!(
            colors.iter().any(|c| c.r > 0.0 || c.g > 0.0 || c.b > 0.0),
            "comet must light at least one LED at its head"
        );
    }

    #[tokio::test]
    async fn shipped_example_plasma_renders_a_frame_well_under_the_tick_budget() {
        // Guards against the per-pixel-trig/per-pixel-hsv-call regression:
        // 400x300 pixels in interpreted Lua must stay fast enough for the
        // canvas engine's tick loop (well under its ~16ms/frame at 60fps).
        let src = include_str!("../../../../../plugins/examples/example_effects.lua");
        let handle = PluginEffectHandle::spawn(
            src.to_string(),
            "plasma".to_string(),
            [("speed".to_string(), EffectParamValue::Float(0.8))]
                .into_iter()
                .collect(),
            vec![],
        );
        // First call pays for VM/palette warm-up; time a subsequent one.
        handle.render_pixmap(0.0, 0.016).await.unwrap();
        let start = std::time::Instant::now();
        handle.render_pixmap(0.1, 0.016).await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "plasma render took {elapsed:?}, expected well under one frame budget"
        );
    }

    #[tokio::test]
    async fn shipped_example_comet_direction_reverses_the_sweep() {
        let src = include_str!("../../../../../plugins/examples/example_effects.lua");
        let leds: Vec<LedCoord> = (0..8)
            .map(|i| LedCoord {
                p: i as f32 / 7.0,
                p_ring: i as f32 / 7.0,
                nx: 0.0,
                ny: 0.0,
            })
            .collect();

        let mut fwd_params = params();
        fwd_params.insert(
            "direction".to_string(),
            EffectParamValue::Str("forward".to_string()),
        );
        fwd_params.insert("speed".to_string(), EffectParamValue::Float(1.0));
        let forward =
            PluginEffectHandle::spawn(src.to_string(), "comet".to_string(), fwd_params, vec![]);
        let forward_colors = forward.led_colors(leds.clone(), 0.3, 0.0).await.unwrap();

        let mut back_params = params();
        back_params.insert(
            "direction".to_string(),
            EffectParamValue::Str("backward".to_string()),
        );
        back_params.insert("speed".to_string(), EffectParamValue::Float(1.0));
        let backward =
            PluginEffectHandle::spawn(src.to_string(), "comet".to_string(), back_params, vec![]);
        let backward_colors = backward.led_colors(leds, 0.3, 0.0).await.unwrap();

        // Default comet color is {r:0, g:160, b:255} — compare `.g` (or `.b`),
        // not `.r`, since red is always zero regardless of direction.
        assert_ne!(
            forward_colors.iter().map(|c| c.g).collect::<Vec<_>>(),
            backward_colors.iter().map(|c| c.g).collect::<Vec<_>>(),
            "forward vs backward direction must sweep the comet head differently"
        );
    }
}
