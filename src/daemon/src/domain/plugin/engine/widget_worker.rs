// SPDX-License-Identifier: GPL-3.0-or-later
//! Sandboxed renderer for plugin-declared LCD widgets.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::ops::ControlFlow;
use std::rc::Rc;

use ab_glyph::{point, Font, FontArc, PxScale, ScaleFont};
use anyhow::{anyhow, Result};
use image::{ImageBuffer, Pixel as _, Rgba};
use imageproc::drawing::{
    draw_filled_circle_mut, draw_filled_rect_mut, draw_hollow_circle_mut, draw_line_segment_mut,
    draw_polygon_mut,
};
use imageproc::point::Point;
use imageproc::rect::Rect;
use mlua::{AnyUserData, Function, Lua, LuaSerdeExt, Table, UserData, UserDataMethods, Value};
use tokio::sync::oneshot;
use unicode_segmentation::UnicodeSegmentation;

use halod_shared::lcd_custom::{
    TEXT_ITALIC_PARAM, TEXT_STRIKETHROUGH_PARAM, TEXT_UNDERLINE_PARAM, TEXT_WEIGHT_PARAM,
};
use halod_shared::types::{EffectParamValue, Permission, RgbColor};

use super::bytebuf::{alloc_zeroed, ByteBuf};
use super::lua_worker::LuaWorker;
use super::sandbox;
use super::{PLUGIN_INSTRUCTION_BUDGET, PLUGIN_VM_MEMORY_BYTES};

const MAX_WIDGET_SIDE: u32 = 1024;
const MAX_DRAW_TEXT_BYTES: usize = halod_shared::lcd_custom::MAX_WIDGET_TEXT_BYTES;
const MAX_COMPOSITION_DEPTH: usize = 8;
const MAX_DRAW_POINTS: usize = 64;
const MAX_RENDER_WORK_PIXELS: usize = 32 * 1024 * 1024;
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Clone)]
pub struct WidgetRenderInput {
    pub widget_id: String,
    pub width: u32,
    pub height: u32,
    pub time: f32,
    pub dt: f32,
    pub params: HashMap<String, EffectParamValue>,
    pub color: RgbColor,
    pub font: FontArc,
    pub audio: Option<WidgetAudioInput>,
    pub media_art: Option<WidgetImageInput>,
    pub images: HashMap<String, WidgetImageInput>,
    pub assets: HashMap<String, WidgetImageInput>,
    pub preview: bool,
}

#[derive(Clone)]
pub struct WidgetAudioInput {
    pub level: f32,
    pub flux: f32,
    pub beat: bool,
    pub seq: u64,
    pub bands: Vec<f32>,
}

#[derive(Clone)]
pub struct WidgetImageInput {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

enum WidgetCall {
    Render {
        input: WidgetRenderInput,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
}

struct WorkerCtx {
    lua: Lua,
    live: HashMap<String, Function>,
    preview: HashMap<String, Function>,
    budget: Rc<Cell<u64>>,
    data: super::data_api::DataRuntime,
}

#[derive(Clone)]
pub struct PluginWidgetHandle(LuaWorker<WidgetCall>);

impl PluginWidgetHandle {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn spawn(
        source: String,
        modules: BTreeMap<String, String>,
        widget_ids: Vec<String>,
        granted: Vec<Permission>,
        config: crate::domain::plugin::ResolvedConfig,
    ) -> Self {
        Self::spawn_with_data(
            source,
            modules,
            widget_ids,
            granted,
            config,
            Default::default(),
        )
    }

    pub fn spawn_with_data(
        source: String,
        modules: BTreeMap<String, String>,
        widget_ids: Vec<String>,
        granted: Vec<Permission>,
        config: crate::domain::plugin::ResolvedConfig,
        data: super::data_api::DataRuntime,
    ) -> Self {
        let worker = LuaWorker::spawn(
            "halod-lcd-widget",
            "LCD widget",
            CALL_TIMEOUT,
            move || {
                build_ctx(
                    &source,
                    &modules,
                    &widget_ids,
                    &granted,
                    &config,
                    data.clone(),
                )
            },
            |call, ctx: &WorkerCtx| {
                ctx.budget.set(0);
                sandbox::set_call_deadline(&ctx.lua, CALL_TIMEOUT);
                match call {
                    WidgetCall::Render { input, reply } => {
                        let _ = reply.send(render_one(ctx, input));
                    }
                }
                ControlFlow::Continue(())
            },
        )
        .unwrap_or_else(|error| {
            log::error!("LCD widget worker not started: {error:#}");
            LuaWorker::dead("LCD widget")
        });
        Self(worker)
    }

    pub async fn render(&self, input: WidgetRenderInput) -> Result<Vec<u8>> {
        self.0
            .request(|reply| WidgetCall::Render { input, reply })
            .await?
    }

    pub fn is_usable(&self) -> bool {
        self.0.is_usable()
    }
}

fn build_ctx(
    source: &str,
    modules: &BTreeMap<String, String>,
    widget_ids: &[String],
    granted: &[Permission],
    config: &crate::domain::plugin::ResolvedConfig,
    data: super::data_api::DataRuntime,
) -> Result<WorkerCtx> {
    let (lua, budget) = sandbox::bootstrap_vm(
        granted,
        config,
        PLUGIN_VM_MEMORY_BYTES,
        PLUGIN_INSTRUCTION_BUDGET,
    )
    .map_err(|error| anyhow!("widget sandbox setup: {error}"))?;
    sandbox::install_package_modules(&lua, modules)
        .map_err(|error| anyhow!("widget package modules: {error}"))?;
    super::data_api::register(&lua, data.clone())
        .map_err(|error| anyhow!("widget data API: {error}"))?;
    let manifest: Table = lua
        .load(source)
        .eval()
        .map_err(|error| anyhow!("widget script evaluation: {error}"))?;
    let mut live = HashMap::new();
    let mut preview = HashMap::new();
    for id in widget_ids {
        let live_fn: Function = manifest
            .get(format!("render_widget_{id}"))
            .map_err(|_| anyhow!("widget '{id}' has no render_widget_{id} callback"))?;
        let preview_fn: Function = manifest
            .get(format!("preview_widget_{id}"))
            .map_err(|_| anyhow!("widget '{id}' has no preview_widget_{id} callback"))?;
        live.insert(id.clone(), live_fn);
        preview.insert(id.clone(), preview_fn);
    }
    Ok(WorkerCtx {
        lua,
        live,
        preview,
        budget,
        data,
    })
}

fn checked_len(width: u32, height: u32) -> Result<usize> {
    if width == 0 || height == 0 || width > MAX_WIDGET_SIDE || height > MAX_WIDGET_SIDE {
        return Err(anyhow!(
            "widget dimensions {width}x{height} are out of range"
        ));
    }
    let len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow!("widget dimensions overflow"))?;
    alloc_zeroed(len)
        .map(|data| data.len())
        .map_err(|error| anyhow!("widget allocation: {error}"))
}

fn render_one(ctx: &WorkerCtx, input: WidgetRenderInput) -> Result<Vec<u8>> {
    let len = checked_len(input.width, input.height)?;
    let callback = if input.preview {
        ctx.preview.get(&input.widget_id)
    } else {
        ctx.live.get(&input.widget_id)
    }
    .ok_or_else(|| anyhow!("unknown widget callback '{}'", input.widget_id))?;
    let text_style = WidgetTextStyle::from_params(&input.params);
    let render_ctx = RenderCtx {
        width: input.width,
        height: input.height,
        color: input.color,
        font: input.font,
        text_style,
        audio: input.audio.or_else(|| input.preview.then(preview_audio)),
        media_art: input.media_art,
        images: input.images,
        assets: input.assets,
        preview: input.preview,
        composition: RefCell::new(CompositionState::new(input.width, input.height)),
        render_work_pixels: Cell::new(0),
        data: ctx.data.clone(),
    };
    let canvas = ctx
        .lua
        .create_userdata(ByteBuf::from_bytes(vec![0; len]))
        .map_err(|error| anyhow!("widget canvas: {error}"))?;
    let render_ctx = ctx
        .lua
        .create_userdata(render_ctx)
        .map_err(|error| anyhow!("widget context: {error}"))?;
    let params = ctx
        .lua
        .to_value(&input.params)
        .map_err(|error| anyhow!("widget params: {error}"))?;
    if input.preview {
        callback
            .call::<()>((
                canvas.clone(),
                input.width,
                input.height,
                params,
                render_ctx,
            ))
            .map_err(|error| anyhow!("widget preview: {error}"))?;
    } else {
        callback
            .call::<()>((
                canvas.clone(),
                input.width,
                input.height,
                input.time,
                input.dt,
                params,
                render_ctx,
            ))
            .map_err(|error| anyhow!("widget render: {error}"))?;
    }
    Ok(canvas
        .borrow::<ByteBuf>()
        .map_err(|error| anyhow!("widget canvas readback: {error}"))?
        .as_slice()
        .to_vec())
}

fn preview_audio() -> WidgetAudioInput {
    WidgetAudioInput {
        level: 0.62,
        flux: 0.18,
        beat: false,
        seq: 1,
        bands: (0..32)
            .map(|index| 0.22 + 0.58 * (index as f32 * 0.71).sin().abs())
            .collect(),
    }
}

struct RenderCtx {
    width: u32,
    height: u32,
    color: RgbColor,
    font: FontArc,
    text_style: WidgetTextStyle,
    audio: Option<WidgetAudioInput>,
    media_art: Option<WidgetImageInput>,
    images: HashMap<String, WidgetImageInput>,
    assets: HashMap<String, WidgetImageInput>,
    preview: bool,
    composition: RefCell<CompositionState>,
    render_work_pixels: Cell<usize>,
    data: super::data_api::DataRuntime,
}

impl super::data_api::HasDataRuntime for RenderCtx {
    fn data_runtime(&self) -> &super::data_api::DataRuntime {
        &self.data
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ClipRect {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl ClipRect {
    fn canvas(width: u32, height: u32) -> Self {
        Self {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        }
    }

    fn intersect(self, other: Self) -> Self {
        Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right).max(self.left.max(other.left)),
            bottom: self.bottom.min(other.bottom).max(self.top.max(other.top)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct AffineTransform {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl AffineTransform {
    const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    fn rotation(degrees: f32, cx: f32, cy: f32) -> Self {
        let radians = degrees.to_radians();
        let (sin, cos) = radians.sin_cos();
        Self {
            a: cos,
            b: sin,
            c: -sin,
            d: cos,
            e: cx - cos * cx + sin * cy,
            f: cy - sin * cx - cos * cy,
        }
    }

    fn then(self, next: Self) -> Self {
        Self {
            a: next.a * self.a + next.c * self.b,
            b: next.b * self.a + next.d * self.b,
            c: next.a * self.c + next.c * self.d,
            d: next.b * self.c + next.d * self.d,
            e: next.a * self.e + next.c * self.f + next.e,
            f: next.b * self.e + next.d * self.f + next.f,
        }
    }

    fn inverse(self) -> Option<Self> {
        let determinant = self.a * self.d - self.b * self.c;
        if determinant.abs() <= f32::EPSILON {
            return None;
        }
        let inverse = determinant.recip();
        Some(Self {
            a: self.d * inverse,
            b: -self.b * inverse,
            c: -self.c * inverse,
            d: self.a * inverse,
            e: (self.c * self.f - self.d * self.e) * inverse,
            f: (self.b * self.e - self.a * self.f) * inverse,
        })
    }

    fn map(self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
}

#[derive(Clone)]
struct CompositionState {
    canvas: ClipRect,
    clips: Vec<ClipRect>,
    opacities: Vec<f32>,
    transforms: Vec<AffineTransform>,
}

impl CompositionState {
    fn new(width: u32, height: u32) -> Self {
        Self {
            canvas: ClipRect::canvas(width, height),
            clips: Vec::new(),
            opacities: Vec::new(),
            transforms: Vec::new(),
        }
    }

    fn clip(&self) -> ClipRect {
        self.clips.last().copied().unwrap_or(self.canvas)
    }

    fn opacity(&self) -> f32 {
        self.opacities.last().copied().unwrap_or(1.0)
    }

    fn transform(&self) -> AffineTransform {
        self.transforms
            .last()
            .copied()
            .unwrap_or(AffineTransform::IDENTITY)
    }

    fn requires_layer(&self) -> bool {
        !self.clips.is_empty() || !self.opacities.is_empty() || !self.transforms.is_empty()
    }
}

impl RenderCtx {
    fn charge_render_work(&self, multiplier: usize) -> mlua::Result<()> {
        let pixels = self.width as usize * self.height as usize;
        let next = self
            .render_work_pixels
            .get()
            .saturating_add(pixels.saturating_mul(multiplier.max(1)));
        if next > MAX_RENDER_WORK_PIXELS {
            return Err(mlua::Error::RuntimeError(
                "widget drawing exceeds the per-frame work limit".into(),
            ));
        }
        self.render_work_pixels.set(next);
        Ok(())
    }
}

impl UserData for RenderCtx {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        super::data_api::add_ctx_method(methods);
        methods.add_method(
            "push_clip",
            |_, this, (x, y, width, height): (f32, f32, f32, f32)| {
                require_finite("push_clip", &[x, y, width, height])?;
                if width < 0.0 || height < 0.0 {
                    return Err(mlua::Error::RuntimeError(
                        "push_clip width and height must be non-negative".into(),
                    ));
                }
                let mut state = this.composition.borrow_mut();
                if state.clips.len() >= MAX_COMPOSITION_DEPTH {
                    return Err(stack_limit_error("clip"));
                }
                let clip = ClipRect {
                    left: x.floor().clamp(0.0, this.width as f32) as u32,
                    top: y.floor().clamp(0.0, this.height as f32) as u32,
                    right: (x + width).ceil().clamp(0.0, this.width as f32) as u32,
                    bottom: (y + height).ceil().clamp(0.0, this.height as f32) as u32,
                };
                let clip = state.clip().intersect(clip);
                state.clips.push(clip);
                Ok(())
            },
        );
        methods.add_method("pop_clip", |_, this, ()| {
            this.composition
                .borrow_mut()
                .clips
                .pop()
                .ok_or_else(|| stack_underflow_error("clip"))?;
            Ok(())
        });
        methods.add_method("push_opacity", |_, this, opacity: f32| {
            require_finite("push_opacity", &[opacity])?;
            if !(0.0..=1.0).contains(&opacity) {
                return Err(mlua::Error::RuntimeError(
                    "opacity must be between 0 and 1".into(),
                ));
            }
            let mut state = this.composition.borrow_mut();
            if state.opacities.len() >= MAX_COMPOSITION_DEPTH {
                return Err(stack_limit_error("opacity"));
            }
            let opacity = state.opacity() * opacity;
            state.opacities.push(opacity);
            Ok(())
        });
        methods.add_method("pop_opacity", |_, this, ()| {
            this.composition
                .borrow_mut()
                .opacities
                .pop()
                .ok_or_else(|| stack_underflow_error("opacity"))?;
            Ok(())
        });
        methods.add_method(
            "push_rotation",
            |_, this, (degrees, center_x, center_y): (f32, f32, f32)| {
                require_finite("push_rotation", &[degrees, center_x, center_y])?;
                let mut state = this.composition.borrow_mut();
                if state.transforms.len() >= MAX_COMPOSITION_DEPTH {
                    return Err(stack_limit_error("transform"));
                }
                let transform = state.transform().then(AffineTransform::rotation(
                    degrees.rem_euclid(360.0),
                    bounded_coord(center_x, this.width) as f32,
                    bounded_coord(center_y, this.height) as f32,
                ));
                state.transforms.push(transform);
                Ok(())
            },
        );
        methods.add_method("pop_rotation", |_, this, ()| {
            this.composition
                .borrow_mut()
                .transforms
                .pop()
                .ok_or_else(|| stack_underflow_error("transform"))?;
            Ok(())
        });
        methods.add_method("is_preview", |_, this, ()| Ok(this.preview));
        methods.add_method("color", |lua, this, ()| lua.to_value(&this.color));
        methods.add_method("audio", |lua, this, ()| {
            let Some(audio) = &this.audio else {
                return Ok(None);
            };
            let table = lua.create_table()?;
            table.set("level", audio.level)?;
            table.set("flux", audio.flux)?;
            table.set("beat", audio.beat)?;
            table.set("seq", audio.seq)?;
            table.set(
                "bands",
                lua.create_sequence_from(audio.bands.iter().copied())?,
            )?;
            Ok(Some(table))
        });
        methods.add_method(
            "draw_media_art",
            |_, this, (canvas, x, y, width, height): (AnyUserData, f32, f32, f32, f32)| {
                let Some(source) = this.media_art.as_ref() else {
                    return Ok(false);
                };
                let Some(source) =
                    ImageBuffer::from_raw(source.width, source.height, source.rgba.as_slice())
                else {
                    return Ok(false);
                };
                require_finite("draw_media_art", &[x, y, width, height])?;
                let width = checked_draw_dimension(width, this.width, "width")?;
                let height = checked_draw_dimension(height, this.height, "height")?;
                let fitted = fit_image(&source, width, height, "cover");
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                with_composed_canvas(this, &mut canvas, 2, |target| {
                    image::imageops::overlay(
                        target,
                        &fitted,
                        i64::from(bounded_coord(x, this.width)),
                        i64::from(bounded_coord(y, this.height)),
                    );
                })?;
                Ok(true)
            },
        );
        methods.add_method(
            "draw_image",
            |_,
             this,
             (canvas, filename, x, y, width, height, fit, shape): (
                AnyUserData,
                String,
                f32,
                f32,
                f32,
                f32,
                String,
                String,
            )| {
                let Some(source) = this.images.get(&filename) else {
                    return Ok(false);
                };
                let Some(source) =
                    ImageBuffer::from_raw(source.width, source.height, source.rgba.as_slice())
                else {
                    return Ok(false);
                };
                require_finite("draw_image", &[x, y, width, height])?;
                let width = checked_draw_dimension(width, this.width, "width")?;
                let height = checked_draw_dimension(height, this.height, "height")?;
                let mut fitted = fit_image(&source, width, height, &fit);
                mask_image(&mut fitted, &shape);
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                with_composed_canvas(this, &mut canvas, 2, |target| {
                    image::imageops::overlay(
                        target,
                        &fitted,
                        i64::from(bounded_coord(x, this.width)),
                        i64::from(bounded_coord(y, this.height)),
                    );
                })?;
                Ok(true)
            },
        );
        methods.add_method(
            "draw_asset",
            |_,
             this,
             (canvas, name, x, y, width, height, fit): (
                AnyUserData,
                String,
                f32,
                f32,
                f32,
                f32,
                String,
            )| {
                let Some(source) = this.assets.get(&name) else {
                    return Ok(false);
                };
                let Some(source) =
                    ImageBuffer::from_raw(source.width, source.height, source.rgba.as_slice())
                else {
                    return Ok(false);
                };
                require_finite("draw_asset", &[x, y, width, height])?;
                let width = checked_draw_dimension(width, this.width, "width")?;
                let height = checked_draw_dimension(height, this.height, "height")?;
                let fitted = fit_image(&source, width, height, &fit);
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                with_composed_canvas(this, &mut canvas, 2, |target| {
                    image::imageops::overlay(
                        target,
                        &fitted,
                        i64::from(bounded_coord(x, this.width)),
                        i64::from(bounded_coord(y, this.height)),
                    );
                })?;
                Ok(true)
            },
        );
        methods.add_method("local_time", |lua, this, ()| {
            use chrono::{Datelike as _, Timelike as _};
            let (year, month, day, hour, minute, second, weekday) = if this.preview {
                (2026, 1, 15, 10, 9, 36, "Thu".to_owned())
            } else {
                let now = chrono::Local::now();
                (
                    now.year(),
                    now.month(),
                    now.day(),
                    now.hour(),
                    now.minute(),
                    now.second(),
                    now.format("%a").to_string(),
                )
            };
            let table = lua.create_table()?;
            table.set("year", year)?;
            table.set("month", month)?;
            table.set("day", day)?;
            table.set("hour", hour)?;
            table.set("minute", minute)?;
            table.set("second", second)?;
            table.set("weekday", weekday)?;
            Ok(table)
        });
        methods.add_method(
            "fill_rect",
            |lua,
             this,
             (canvas, x, y, width, height, color): (
                AnyUserData,
                f32,
                f32,
                f32,
                f32,
                Option<mlua::Value>,
            )| {
                require_finite("fill_rect", &[x, y, width, height])?;
                let color = lua_color(lua, color, this.color)?;
                let width = width.round().clamp(0.0, this.width as f32) as u32;
                let height = height.round().clamp(0.0, this.height as f32) as u32;
                if width == 0 || height == 0 {
                    return Ok(());
                }
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                with_composed_canvas(this, &mut canvas, 1, |image| {
                    draw_filled_rect_mut(
                        image,
                        Rect::at(bounded_coord(x, this.width), bounded_coord(y, this.height))
                            .of_size(width, height),
                        color,
                    );
                })?;
                Ok(())
            },
        );
        methods.add_method(
            "fill_rounded_rect",
            |lua,
             this,
             (canvas, x, y, width, height, radius, color): (
                AnyUserData,
                f32,
                f32,
                f32,
                f32,
                f32,
                Option<mlua::Value>,
            )| {
                require_finite("fill_rounded_rect", &[x, y, width, height, radius])?;
                let color = lua_color(lua, color, this.color)?;
                let width = width.round().clamp(0.0, this.width as f32) as u32;
                let height = height.round().clamp(0.0, this.height as f32) as u32;
                if width == 0 || height == 0 {
                    return Ok(());
                }
                let radius = radius.round().clamp(0.0, width.min(height) as f32 / 2.0);
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                with_composed_canvas(this, &mut canvas, 1, |image| {
                    fill_rounded_rect_mut(
                        image,
                        bounded_coord(x, this.width),
                        bounded_coord(y, this.height),
                        width,
                        height,
                        radius,
                        color,
                    );
                })?;
                Ok(())
            },
        );
        methods.add_method(
            "draw_line",
            |lua,
             this,
             (canvas, x1, y1, x2, y2, color, stroke_width): (
                AnyUserData,
                f32,
                f32,
                f32,
                f32,
                Option<mlua::Value>,
                Option<f32>,
            )| {
                require_finite("draw_line", &[x1, y1, x2, y2])?;
                let stroke_width = checked_stroke_width(stroke_width)?;
                let color = lua_color(lua, color, this.color)?;
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                let points = [
                    (
                        bounded_coord(x1, this.width) as f32,
                        bounded_coord(y1, this.height) as f32,
                    ),
                    (
                        bounded_coord(x2, this.width) as f32,
                        bounded_coord(y2, this.height) as f32,
                    ),
                ];
                with_composed_canvas(this, &mut canvas, 1, |image| {
                    stroke_polyline_mut(image, &points, stroke_width, false, color);
                })?;
                Ok(())
            },
        );
        methods.add_method(
            "draw_circle",
            |lua,
             this,
             (canvas, x, y, radius, filled, color, stroke_width): (
                AnyUserData,
                f32,
                f32,
                f32,
                bool,
                Option<mlua::Value>,
                Option<f32>,
            )| {
                require_finite("draw_circle", &[x, y, radius])?;
                let color = lua_color(lua, color, this.color)?;
                let center = (bounded_coord(x, this.width), bounded_coord(y, this.height));
                let radius = bounded_dimension(radius, this.width.max(this.height)) as i32;
                let stroke_width = checked_stroke_width(stroke_width)?;
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                with_composed_canvas(this, &mut canvas, 1, |image| {
                    if filled {
                        draw_filled_circle_mut(image, center, radius, color);
                    } else {
                        draw_stroked_circle_mut(image, center, radius, stroke_width, color);
                    }
                })?;
                Ok(())
            },
        );
        methods.add_method(
            "draw_arc",
            |lua,
             this,
             (canvas, x, y, radius, thickness, start, sweep, cap_radius, color): (
                AnyUserData,
                f32,
                f32,
                f32,
                f32,
                f32,
                f32,
                f32,
                Option<mlua::Value>,
            )| {
                require_finite(
                    "draw_arc",
                    &[x, y, radius, thickness, start, sweep, cap_radius],
                )?;
                let color = lua_color(lua, color, this.color)?;
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                with_composed_canvas(this, &mut canvas, 1, |image| {
                    draw_arc_mut(
                        image,
                        (
                            bounded_coord(x, this.width) as f32,
                            bounded_coord(y, this.height) as f32,
                        ),
                        bounded_dimension(radius, this.width.max(this.height)) as f32,
                        bounded_dimension(thickness, this.width.max(this.height)) as f32,
                        bounded_angle(start, 0.0),
                        bounded_angle(sweep, 0.0).clamp(-360.0, 360.0),
                        bounded_dimension_or_zero(cap_radius, this.width.max(this.height)),
                        color,
                    );
                })?;
                Ok(())
            },
        );
        methods.add_method(
            "draw_triangle",
            |lua,
             this,
             (canvas, x1, y1, x2, y2, x3, y3, filled, color, stroke_width): (
                AnyUserData,
                f32,
                f32,
                f32,
                f32,
                f32,
                f32,
                bool,
                Option<mlua::Value>,
                Option<f32>,
            )| {
                require_finite("draw_triangle", &[x1, y1, x2, y2, x3, y3])?;
                let stroke_width = checked_stroke_width(stroke_width)?;
                let color = lua_color(lua, color, this.color)?;
                let points = [
                    (
                        bounded_coord(x1, this.width) as f32,
                        bounded_coord(y1, this.height) as f32,
                    ),
                    (
                        bounded_coord(x2, this.width) as f32,
                        bounded_coord(y2, this.height) as f32,
                    ),
                    (
                        bounded_coord(x3, this.width) as f32,
                        bounded_coord(y3, this.height) as f32,
                    ),
                ];
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                with_composed_canvas(this, &mut canvas, 1, |image| {
                    draw_polygon_or_stroke_mut(image, &points, filled, stroke_width, color);
                })?;
                Ok(())
            },
        );
        methods.add_method(
            "draw_polyline",
            |lua,
             this,
             (canvas, points, color, stroke_width): (
                AnyUserData,
                Table,
                Option<Value>,
                Option<f32>,
            )| {
                let points = lua_points(&points, this.width, this.height, 2, "draw_polyline")?;
                let stroke_width = checked_stroke_width(stroke_width)?;
                let color = lua_color(lua, color, this.color)?;
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                with_composed_canvas(this, &mut canvas, points.len().div_ceil(16), |image| {
                    stroke_polyline_mut(image, &points, stroke_width, false, color);
                })?;
                Ok(())
            },
        );
        methods.add_method(
            "draw_polygon",
            |lua,
             this,
             (canvas, points, filled, color, stroke_width): (
                AnyUserData,
                Table,
                bool,
                Option<Value>,
                Option<f32>,
            )| {
                let points = lua_points(&points, this.width, this.height, 3, "draw_polygon")?;
                let stroke_width = checked_stroke_width(stroke_width)?;
                let color = lua_color(lua, color, this.color)?;
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                with_composed_canvas(this, &mut canvas, points.len().div_ceil(16), |image| {
                    draw_polygon_or_stroke_mut(image, &points, filled, stroke_width, color);
                })?;
                Ok(())
            },
        );
        methods.add_method("measure_text", |_, this, (text, size): (String, f32)| {
            require_finite("measure_text", &[size])?;
            let text = bounded_text(text);
            let size = bounded_dimension(size, this.height) as f32;
            Ok(styled_text_size(&this.font, &text, size, this.text_style))
        });
        methods.add_method(
            "measure_text_box",
            |_, this, (text, width, style): (String, f32, Table)| {
                let style = TextBoxStyle::from_lua(&style, this.height)?;
                let width = checked_text_box_dimension(width, MAX_WIDGET_SIDE, "width")?;
                let layout = layout_text_box(
                    &this.font,
                    &bounded_text(text),
                    width,
                    style,
                    this.text_style,
                );
                Ok((layout.width, layout.height))
            },
        );
        methods.add_method(
            "ellipsize_text",
            |_, this, (text, size, max_width): (String, f32, f32)| {
                Ok(ellipsize_text(
                    &this.font,
                    text,
                    size,
                    max_width,
                    this.height,
                    this.text_style,
                ))
            },
        );
        methods.add_method(
            "draw_text",
            |lua,
             this,
             (canvas, text, x, y, size, color): (
                AnyUserData,
                String,
                f32,
                f32,
                f32,
                Option<mlua::Value>,
            )| {
                require_finite("draw_text", &[x, y, size])?;
                let text = bounded_text(text);
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                let color = lua_color(lua, color, this.color)?;
                with_composed_canvas(this, &mut canvas, 1, |image| {
                    draw_styled_text_mut(
                        image,
                        color,
                        bounded_coord(x, this.width),
                        bounded_coord(y, this.height),
                        bounded_dimension(size, this.height) as f32,
                        &this.font,
                        &text,
                        this.text_style,
                    );
                })?;
                Ok(())
            },
        );
        methods.add_method(
            "draw_text_box",
            |lua,
             this,
             (canvas, text, x, y, width, height, style, color): (
                AnyUserData,
                String,
                f32,
                f32,
                f32,
                f32,
                Table,
                Option<Value>,
            )| {
                require_finite("draw_text_box", &[x, y, width, height])?;
                let style = TextBoxStyle::from_lua(&style, this.height)?;
                let width = checked_text_box_dimension(width, MAX_WIDGET_SIDE, "width")?;
                let height = checked_text_box_dimension(height, MAX_WIDGET_SIDE, "height")?;
                let layout = layout_text_box(
                    &this.font,
                    &bounded_text(text),
                    width,
                    style,
                    this.text_style,
                );
                let color = lua_color(lua, color, this.color)?;
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                with_composed_canvas(this, &mut canvas, 2, |image| {
                    draw_text_box_mut(
                        image,
                        color,
                        bounded_coord(x, this.width),
                        bounded_coord(y, this.height),
                        width.ceil() as u32,
                        height.ceil() as u32,
                        &this.font,
                        &layout,
                        style,
                        this.text_style,
                    );
                })?;
                Ok(())
            },
        );
    }
}

fn stack_limit_error(stack: &str) -> mlua::Error {
    mlua::Error::RuntimeError(format!(
        "{stack} stack exceeds the depth limit of {MAX_COMPOSITION_DEPTH}"
    ))
}

fn stack_underflow_error(stack: &str) -> mlua::Error {
    mlua::Error::RuntimeError(format!("{stack} stack is empty"))
}

fn require_finite(operation: &str, values: &[f32]) -> mlua::Result<()> {
    if values.iter().any(|value| !value.is_finite()) {
        return Err(mlua::Error::RuntimeError(format!(
            "{operation} requires finite numeric arguments"
        )));
    }
    Ok(())
}

fn checked_draw_dimension(value: f32, limit: u32, name: &str) -> mlua::Result<u32> {
    if !value.is_finite() || value <= 0.0 {
        return Err(mlua::Error::RuntimeError(format!(
            "drawing {name} must be finite and positive"
        )));
    }
    Ok(value.round().clamp(1.0, limit.max(1) as f32) as u32)
}

fn checked_stroke_width(value: Option<f32>) -> mlua::Result<f32> {
    let value = value.unwrap_or(1.0);
    if !value.is_finite() || !(0.25..=32.0).contains(&value) {
        return Err(mlua::Error::RuntimeError(
            "stroke width must be between 0.25 and 32".into(),
        ));
    }
    Ok(value)
}

fn lua_points(
    table: &Table,
    canvas_width: u32,
    canvas_height: u32,
    minimum: usize,
    operation: &str,
) -> mlua::Result<Vec<(f32, f32)>> {
    let count = table.raw_len();
    if count < minimum || count > MAX_DRAW_POINTS {
        return Err(mlua::Error::RuntimeError(format!(
            "{operation} requires {minimum}..={MAX_DRAW_POINTS} points"
        )));
    }
    let mut points = Vec::with_capacity(count);
    for index in 1..=count {
        let point: Table = table.raw_get(index)?;
        let x = point
            .get::<Option<f32>>("x")?
            .or(point.get::<Option<f32>>(1)?)
            .ok_or_else(|| {
                mlua::Error::RuntimeError(format!("{operation} point {index} has no x"))
            })?;
        let y = point
            .get::<Option<f32>>("y")?
            .or(point.get::<Option<f32>>(2)?)
            .ok_or_else(|| {
                mlua::Error::RuntimeError(format!("{operation} point {index} has no y"))
            })?;
        require_finite(operation, &[x, y])?;
        points.push((
            bounded_coord(x, canvas_width) as f32,
            bounded_coord(y, canvas_height) as f32,
        ));
    }
    Ok(points)
}

fn with_composed_canvas<F>(
    ctx: &RenderCtx,
    canvas: &mut ByteBuf,
    work_multiplier: usize,
    draw: F,
) -> mlua::Result<()>
where
    F: FnOnce(&mut ImageBuffer<Rgba<u8>, &mut [u8]>),
{
    ctx.charge_render_work(work_multiplier)?;
    let state = ctx.composition.borrow().clone();
    if !state.requires_layer() {
        let mut image = canvas_image(ctx, canvas)?;
        draw(&mut image);
        return Ok(());
    }

    let len = ctx.width as usize * ctx.height as usize * 4;
    let mut layer_bytes = alloc_zeroed(len)?;
    let mut layer = ImageBuffer::from_raw(ctx.width, ctx.height, layer_bytes.as_mut_slice())
        .ok_or_else(|| mlua::Error::RuntimeError("invalid widget layer".into()))?;
    draw(&mut layer);
    let mut target = canvas_image(ctx, canvas)?;
    composite_layer(&mut target, &layer, &state);
    Ok(())
}

fn composite_layer(
    target: &mut ImageBuffer<Rgba<u8>, &mut [u8]>,
    layer: &ImageBuffer<Rgba<u8>, &mut [u8]>,
    state: &CompositionState,
) {
    let opacity = state.opacity();
    if opacity <= 0.0 {
        return;
    }
    let clip = state.clip();
    let Some(inverse) = state.transform().inverse() else {
        return;
    };
    for y in clip.top..clip.bottom {
        for x in clip.left..clip.right {
            let (source_x, source_y) = inverse.map(x as f32 + 0.5, y as f32 + 0.5);
            let source_x = source_x.floor() as i32;
            let source_y = source_y.floor() as i32;
            if source_x < 0
                || source_y < 0
                || source_x >= layer.width() as i32
                || source_y >= layer.height() as i32
            {
                continue;
            }
            let mut source = *layer.get_pixel(source_x as u32, source_y as u32);
            source.0[3] = (f32::from(source.0[3]) * opacity).round() as u8;
            if source.0[3] != 0 {
                target.get_pixel_mut(x, y).blend(&source);
            }
        }
    }
}

fn stroke_polyline_mut(
    image: &mut ImageBuffer<Rgba<u8>, &mut [u8]>,
    points: &[(f32, f32)],
    width: f32,
    closed: bool,
    color: Rgba<u8>,
) {
    let segment_count = if closed {
        points.len()
    } else {
        points.len().saturating_sub(1)
    };
    for index in 0..segment_count {
        let start = points[index];
        let end = points[(index + 1) % points.len()];
        if width <= 1.0 {
            draw_line_segment_mut(image, start, end, color);
            continue;
        }
        let dx = end.0 - start.0;
        let dy = end.1 - start.1;
        let length = dx.hypot(dy);
        let radius = width / 2.0;
        if length > f32::EPSILON {
            let nx = -dy / length * radius;
            let ny = dx / length * radius;
            let polygon = [
                Point::new((start.0 + nx).round() as i32, (start.1 + ny).round() as i32),
                Point::new((end.0 + nx).round() as i32, (end.1 + ny).round() as i32),
                Point::new((end.0 - nx).round() as i32, (end.1 - ny).round() as i32),
                Point::new((start.0 - nx).round() as i32, (start.1 - ny).round() as i32),
            ];
            draw_polygon_mut(image, &polygon, color);
        }
        draw_filled_circle_mut(
            image,
            (start.0.round() as i32, start.1.round() as i32),
            radius.ceil() as i32,
            color,
        );
        draw_filled_circle_mut(
            image,
            (end.0.round() as i32, end.1.round() as i32),
            radius.ceil() as i32,
            color,
        );
    }
}

fn draw_stroked_circle_mut(
    image: &mut ImageBuffer<Rgba<u8>, &mut [u8]>,
    center: (i32, i32),
    radius: i32,
    width: f32,
    color: Rgba<u8>,
) {
    let pixels = width.ceil() as i32;
    let start = radius.saturating_sub(pixels / 2);
    for offset in 0..pixels {
        draw_hollow_circle_mut(image, center, start + offset, color);
    }
}

fn draw_polygon_or_stroke_mut(
    image: &mut ImageBuffer<Rgba<u8>, &mut [u8]>,
    points: &[(f32, f32)],
    filled: bool,
    stroke_width: f32,
    color: Rgba<u8>,
) {
    if filled {
        let points: Vec<_> = points
            .iter()
            .map(|&(x, y)| Point::new(x.round() as i32, y.round() as i32))
            .collect();
        draw_polygon_mut(image, &points, color);
    } else {
        stroke_polyline_mut(image, points, stroke_width, true, color);
    }
}

#[derive(Clone, Copy, Default)]
enum TextWeight {
    #[default]
    Normal,
    Semibold,
    Bold,
}

#[derive(Clone, Copy, Default)]
struct WidgetTextStyle {
    weight: TextWeight,
    italic: bool,
    underline: bool,
    strikethrough: bool,
}

impl WidgetTextStyle {
    fn from_params(params: &HashMap<String, EffectParamValue>) -> Self {
        let weight = match params.get(TEXT_WEIGHT_PARAM) {
            Some(EffectParamValue::Str(value)) if value == "semibold" => TextWeight::Semibold,
            Some(EffectParamValue::Str(value)) if value == "bold" => TextWeight::Bold,
            _ => TextWeight::Normal,
        };
        let enabled = |key| matches!(params.get(key), Some(EffectParamValue::Bool(true)));
        Self {
            weight,
            italic: enabled(TEXT_ITALIC_PARAM),
            underline: enabled(TEXT_UNDERLINE_PARAM),
            strikethrough: enabled(TEXT_STRIKETHROUGH_PARAM),
        }
    }

    fn embolden(self, size: f32) -> u32 {
        match self.weight {
            TextWeight::Normal => 0,
            TextWeight::Semibold => 1,
            TextWeight::Bold => (size / 18.0).ceil().clamp(1.0, 3.0) as u32,
        }
    }

    fn italic_reach(self, size: f32) -> f32 {
        if self.italic {
            size * 0.22
        } else {
            0.0
        }
    }
}

#[derive(Clone, Copy, Default)]
enum TextHorizontalAlign {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Default)]
enum TextVerticalAlign {
    #[default]
    Top,
    Middle,
    Bottom,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum TextBoxWrap {
    #[default]
    None,
    Word,
    Character,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum TextBoxOverflow {
    #[default]
    Clip,
    Ellipsis,
}

#[derive(Clone, Copy)]
struct TextBoxStyle {
    size: f32,
    horizontal: TextHorizontalAlign,
    vertical: TextVerticalAlign,
    wrap: TextBoxWrap,
    max_lines: usize,
    overflow: TextBoxOverflow,
}

impl TextBoxStyle {
    fn from_lua(table: &Table, canvas_height: u32) -> mlua::Result<Self> {
        for pair in table.clone().pairs::<String, Value>() {
            let (key, _) = pair?;
            if !matches!(
                key.as_str(),
                "size" | "horizontal" | "vertical" | "wrap" | "max_lines" | "overflow"
            ) {
                return Err(mlua::Error::RuntimeError(format!(
                    "unsupported text box style field `{key}`"
                )));
            }
        }

        let size = table.get::<Option<f32>>("size")?.ok_or_else(|| {
            mlua::Error::RuntimeError("text box style requires a numeric `size`".into())
        })?;
        if !size.is_finite() || size <= 0.0 || size > canvas_height.max(1) as f32 {
            return Err(mlua::Error::RuntimeError(format!(
                "text box style size must be between 0 and {}",
                canvas_height.max(1)
            )));
        }

        let horizontal = match table
            .get::<Option<String>>("horizontal")?
            .as_deref()
            .unwrap_or("left")
        {
            "left" => TextHorizontalAlign::Left,
            "center" => TextHorizontalAlign::Center,
            "right" => TextHorizontalAlign::Right,
            value => return Err(invalid_text_box_style("horizontal", value)),
        };
        let vertical = match table
            .get::<Option<String>>("vertical")?
            .as_deref()
            .unwrap_or("top")
        {
            "top" => TextVerticalAlign::Top,
            "middle" => TextVerticalAlign::Middle,
            "bottom" => TextVerticalAlign::Bottom,
            value => return Err(invalid_text_box_style("vertical", value)),
        };
        let wrap = match table
            .get::<Option<String>>("wrap")?
            .as_deref()
            .unwrap_or("none")
        {
            "none" => TextBoxWrap::None,
            "word" => TextBoxWrap::Word,
            "character" => TextBoxWrap::Character,
            value => return Err(invalid_text_box_style("wrap", value)),
        };
        let overflow = match table
            .get::<Option<String>>("overflow")?
            .as_deref()
            .unwrap_or("clip")
        {
            "clip" => TextBoxOverflow::Clip,
            "ellipsis" => TextBoxOverflow::Ellipsis,
            value => return Err(invalid_text_box_style("overflow", value)),
        };
        let max_lines = table.get::<Option<usize>>("max_lines")?.unwrap_or(64);
        if !(1..=64).contains(&max_lines) {
            return Err(mlua::Error::RuntimeError(
                "text box style max_lines must be between 1 and 64".into(),
            ));
        }

        Ok(Self {
            size,
            horizontal,
            vertical,
            wrap,
            max_lines,
            overflow,
        })
    }
}

fn invalid_text_box_style(field: &str, value: &str) -> mlua::Error {
    mlua::Error::RuntimeError(format!("invalid text box {field} `{value}`"))
}

#[derive(Clone)]
struct TextBoxLine {
    text: String,
    width: f32,
}

struct TextBoxLayout {
    lines: Vec<TextBoxLine>,
    width: f32,
    height: f32,
    line_height: f32,
}

fn checked_text_box_dimension(value: f32, limit: u32, name: &str) -> mlua::Result<f32> {
    if !value.is_finite() || value <= 0.0 || value > limit as f32 {
        return Err(mlua::Error::RuntimeError(format!(
            "text box {name} must be between 0 and {limit}"
        )));
    }
    Ok(value)
}

fn text_width(font: &FontArc, text: &str, size: f32, style: WidgetTextStyle) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let scaled = font.as_scaled(PxScale::from(size));
    let mut width = 0.0;
    let mut previous = None;
    for character in text.chars() {
        let glyph = scaled.glyph_id(character);
        if let Some(previous) = previous {
            width += scaled.kern(previous, glyph);
        }
        width += scaled.h_advance(glyph);
        previous = Some(glyph);
    }
    width + style.italic_reach(size) + style.embolden(size) as f32
}

fn styled_text_size(font: &FontArc, text: &str, size: f32, style: WidgetTextStyle) -> (f32, f32) {
    let scaled = font.as_scaled(PxScale::from(size));
    (
        text_width(font, text, size, style),
        scaled.ascent() - scaled.descent() + style.embolden(size).div_ceil(2) as f32,
    )
}

fn character_wrapped_lines(
    font: &FontArc,
    text: &str,
    width: f32,
    size: f32,
    style: WidgetTextStyle,
) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for grapheme in text.graphemes(true) {
        let mut candidate = current.clone();
        candidate.push_str(grapheme);
        if !current.is_empty() && text_width(font, &candidate, size, style) > width {
            lines.push(std::mem::take(&mut current));
        }
        current.push_str(grapheme);
    }
    lines.push(current);
    lines
}

fn word_wrapped_lines(
    font: &FontArc,
    text: &str,
    width: f32,
    size: f32,
    style: WidgetTextStyle,
) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for token in text.split_word_bounds() {
        let token = if current.is_empty() {
            token.trim_start_matches(char::is_whitespace)
        } else {
            token
        };
        if token.is_empty() {
            continue;
        }
        let mut candidate = current.clone();
        candidate.push_str(token);
        if !current.is_empty() && text_width(font, &candidate, size, style) > width {
            lines.push(current.trim_end_matches(char::is_whitespace).to_owned());
            current = String::new();
        }
        let token = token.trim_start_matches(char::is_whitespace);
        if text_width(font, token, size, style) > width {
            let mut chunks = character_wrapped_lines(font, token, width, size, style);
            if let Some(last) = chunks.pop() {
                lines.extend(chunks);
                current = last;
            }
        } else {
            current.push_str(token);
        }
    }
    lines.push(current.trim_end_matches(char::is_whitespace).to_owned());
    lines
}

fn ellipsize_graphemes(
    font: &FontArc,
    text: &str,
    width: f32,
    size: f32,
    style: WidgetTextStyle,
    force: bool,
) -> String {
    if !force && text_width(font, text, size, style) <= width {
        return text.to_owned();
    }
    let ellipsis = "…";
    if text_width(font, ellipsis, size, style) > width {
        return String::new();
    }
    let mut result = String::new();
    for grapheme in text.graphemes(true) {
        let mut candidate = result.clone();
        candidate.push_str(grapheme);
        candidate.push_str(ellipsis);
        if text_width(font, &candidate, size, style) > width {
            break;
        }
        result.push_str(grapheme);
    }
    result.push_str(ellipsis);
    result
}

fn layout_text_box(
    font: &FontArc,
    text: &str,
    width: f32,
    style: TextBoxStyle,
    font_style: WidgetTextStyle,
) -> TextBoxLayout {
    if text.is_empty() {
        return TextBoxLayout {
            lines: Vec::new(),
            width: 0.0,
            height: 0.0,
            line_height: styled_text_size(font, "M", style.size, font_style).1,
        };
    }

    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let paragraph_lines = match style.wrap {
            TextBoxWrap::None => vec![paragraph.to_owned()],
            TextBoxWrap::Word => word_wrapped_lines(font, paragraph, width, style.size, font_style),
            TextBoxWrap::Character => {
                character_wrapped_lines(font, paragraph, width, style.size, font_style)
            }
        };
        lines.extend(paragraph_lines);
    }

    let truncated = lines.len() > style.max_lines;
    lines.truncate(style.max_lines);
    if style.overflow == TextBoxOverflow::Ellipsis {
        let last_index = lines.len().saturating_sub(1);
        for (index, line) in lines.iter_mut().enumerate() {
            *line = ellipsize_graphemes(
                font,
                line,
                width,
                style.size,
                font_style,
                truncated && index == last_index,
            );
        }
    }

    let line_height = styled_text_size(font, "M", style.size, font_style).1;
    let lines: Vec<_> = lines
        .into_iter()
        .map(|text| TextBoxLine {
            width: text_width(font, &text, style.size, font_style),
            text,
        })
        .collect();
    TextBoxLayout {
        width: lines
            .iter()
            .map(|line| line.width.min(width))
            .fold(0.0, f32::max),
        height: line_height * lines.len() as f32,
        line_height,
        lines,
    }
}

fn draw_text_box_mut(
    image: &mut ImageBuffer<Rgba<u8>, &mut [u8]>,
    color: Rgba<u8>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    font: &FontArc,
    layout: &TextBoxLayout,
    style: TextBoxStyle,
    font_style: WidgetTextStyle,
) {
    let mut bytes = vec![0; width as usize * height as usize * 4];
    let mut box_image = ImageBuffer::from_raw(width, height, bytes.as_mut_slice()).unwrap();
    let content_y = match style.vertical {
        TextVerticalAlign::Top => 0.0,
        TextVerticalAlign::Middle => (height as f32 - layout.height) / 2.0,
        TextVerticalAlign::Bottom => height as f32 - layout.height,
    };
    for (line_index, line) in layout.lines.iter().enumerate() {
        let line_x = match style.horizontal {
            TextHorizontalAlign::Left => 0.0,
            TextHorizontalAlign::Center => (width as f32 - line.width) / 2.0,
            TextHorizontalAlign::Right => width as f32 - line.width,
        };
        draw_styled_text_mut(
            &mut box_image,
            color,
            line_x.round() as i32,
            (content_y + line_index as f32 * layout.line_height).round() as i32,
            style.size,
            font,
            &line.text,
            font_style,
        );
    }
    image::imageops::overlay(image, &box_image, i64::from(x), i64::from(y));
}

fn draw_styled_text_mut(
    image: &mut ImageBuffer<Rgba<u8>, &mut [u8]>,
    color: Rgba<u8>,
    x: i32,
    y: i32,
    size: f32,
    font: &FontArc,
    text: &str,
    style: WidgetTextStyle,
) {
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let line_height = scaled.ascent() - scaled.descent();
    let embolden = style.embolden(size);
    let mut pen_x = 0.0;
    let mut last = None;

    for character in text.chars() {
        let glyph_id = scaled.glyph_id(character);
        if let Some(previous) = last {
            pen_x += scaled.kern(previous, glyph_id);
        }
        let glyph = glyph_id.with_scale_and_position(scale, point(pen_x, scaled.ascent()));
        pen_x += scaled.h_advance(glyph_id);
        last = Some(glyph_id);
        let Some(outlined) = font.outline_glyph(glyph) else {
            continue;
        };
        let bounds = outlined.px_bounds();
        outlined.draw(|glyph_x, glyph_y, coverage| {
            let local_y = glyph_y as f32 + bounds.min.y;
            let italic_x = if style.italic {
                ((line_height - local_y) * 0.22).max(0.0)
            } else {
                0.0
            };
            let base_x = x + (glyph_x as f32 + bounds.min.x + italic_x).round() as i32;
            let base_y = y + local_y.round() as i32;
            let alpha = (coverage.clamp(0.0, 1.0) * f32::from(color.0[3])).round() as u8;
            if alpha == 0 {
                return;
            }
            let source = Rgba([color.0[0], color.0[1], color.0[2], alpha]);
            for offset_y in 0..=embolden.div_ceil(2) {
                for offset_x in 0..=embolden {
                    let pixel_x = base_x + offset_x as i32;
                    let pixel_y = base_y + offset_y as i32;
                    if pixel_x >= 0
                        && pixel_y >= 0
                        && pixel_x < image.width() as i32
                        && pixel_y < image.height() as i32
                    {
                        image
                            .get_pixel_mut(pixel_x as u32, pixel_y as u32)
                            .blend(&source);
                    }
                }
            }
        });
    }

    let (width, _) = styled_text_size(font, text, size, style);
    let decoration_thickness = (size * 0.06).round().max(1.0) as u32;
    let mut decoration = |line_y: f32| {
        draw_filled_rect_mut(
            image,
            Rect::at(x, y + line_y.round() as i32)
                .of_size(width.ceil().max(1.0) as u32, decoration_thickness),
            color,
        );
    };
    if style.underline {
        decoration(line_height * 0.92);
    }
    if style.strikethrough {
        decoration(line_height * 0.52);
    }
}

fn bounded_coord(value: f32, side: u32) -> i32 {
    if value.is_finite() {
        value
            .round()
            .clamp(-(side as f32), side.saturating_mul(2) as f32) as i32
    } else {
        0
    }
}

fn bounded_dimension(value: f32, side: u32) -> u32 {
    if value.is_finite() {
        value.round().clamp(1.0, side.max(1) as f32) as u32
    } else {
        1
    }
}

fn bounded_dimension_or_zero(value: f32, side: u32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, side as f32)
    } else {
        0.0
    }
}

fn bounded_angle(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

fn fill_rounded_rect_mut(
    image: &mut ImageBuffer<Rgba<u8>, &mut [u8]>,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    radius: f32,
    color: Rgba<u8>,
) {
    let right = i64::from(left) + i64::from(width);
    let bottom = i64::from(top) + i64::from(height);
    let first_x = i64::from(left).max(0) as u32;
    let first_y = i64::from(top).max(0) as u32;
    let last_x = right.clamp(0, i64::from(image.width())) as u32;
    let last_y = bottom.clamp(0, i64::from(image.height())) as u32;
    let radius_squared = radius * radius;

    for y in first_y..last_y {
        let local_y = y as f32 - top as f32 + 0.5;
        let dy = if local_y < radius {
            radius - local_y
        } else if local_y > height as f32 - radius {
            local_y - (height as f32 - radius)
        } else {
            0.0
        };
        for x in first_x..last_x {
            let local_x = x as f32 - left as f32 + 0.5;
            let dx = if local_x < radius {
                radius - local_x
            } else if local_x > width as f32 - radius {
                local_x - (width as f32 - radius)
            } else {
                0.0
            };
            if dx * dx + dy * dy <= radius_squared {
                image.put_pixel(x, y, color);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_arc_mut(
    image: &mut ImageBuffer<Rgba<u8>, &mut [u8]>,
    center: (f32, f32),
    radius: f32,
    thickness: f32,
    start_degrees: f32,
    sweep_degrees: f32,
    cap_radius: f32,
    color: Rgba<u8>,
) {
    if sweep_degrees.abs() < f32::EPSILON {
        return;
    }
    let half_thickness = (thickness / 2.0).clamp(0.5, radius * 0.9);
    let outer_radius = radius + half_thickness;
    let inner_radius = (radius - half_thickness).max(0.1);
    let start = start_degrees.to_radians();
    let sweep = sweep_degrees.to_radians();
    let steps = (radius * sweep.abs() / 2.0).ceil().clamp(1.0, 360.0) as usize;
    let point = |angle: f32, distance: f32| {
        Point::new(
            (center.0 + angle.sin() * distance).round() as i32,
            (center.1 - angle.cos() * distance).round() as i32,
        )
    };

    for step in 0..steps {
        let a0 = start + sweep * step as f32 / steps as f32;
        let a1 = start + sweep * (step + 1) as f32 / steps as f32;
        let polygon = [
            point(a0, outer_radius),
            point(a1, outer_radius),
            point(a1, inner_radius),
            point(a0, inner_radius),
        ];
        if polygon[0] != polygon[1] && polygon[2] != polygon[3] {
            draw_polygon_mut(image, &polygon, color);
        }
    }

    let cap_radius = cap_radius.clamp(0.0, half_thickness);
    if cap_radius >= 0.5 && sweep_degrees.abs() < 359.99 {
        let sweep_direction = sweep.signum();
        for (angle, direction) in [(start, -sweep_direction), (start + sweep, sweep_direction)] {
            let endpoint = (
                center.0 + angle.sin() * radius,
                center.1 - angle.cos() * radius,
            );
            let radial = (angle.sin(), -angle.cos());
            let tangent = (angle.cos() * direction, angle.sin() * direction);
            let inset = half_thickness - cap_radius;
            let cap_center = |normal: f32| {
                (
                    endpoint.0 + radial.0 * inset * normal,
                    endpoint.1 + radial.1 * inset * normal,
                )
            };
            let upper = cap_center(1.0);
            let lower = cap_center(-1.0);
            let cap = cap_radius.round().max(1.0) as i32;
            for cap_center in [upper, lower] {
                draw_filled_circle_mut(
                    image,
                    (cap_center.0.round() as i32, cap_center.1.round() as i32),
                    cap,
                    color,
                );
            }
            if inset >= 0.5 {
                let polygon = [
                    Point::new(upper.0.round() as i32, upper.1.round() as i32),
                    Point::new(lower.0.round() as i32, lower.1.round() as i32),
                    Point::new(
                        (lower.0 + tangent.0 * cap_radius).round() as i32,
                        (lower.1 + tangent.1 * cap_radius).round() as i32,
                    ),
                    Point::new(
                        (upper.0 + tangent.0 * cap_radius).round() as i32,
                        (upper.1 + tangent.1 * cap_radius).round() as i32,
                    ),
                ];
                if polygon[0] != polygon[1] && polygon[2] != polygon[3] {
                    draw_polygon_mut(image, &polygon, color);
                }
            }
        }
    }
}

fn bounded_text(mut text: String) -> String {
    if text.len() > MAX_DRAW_TEXT_BYTES {
        let boundary = text
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= MAX_DRAW_TEXT_BYTES)
            .last()
            .unwrap_or(0);
        text.truncate(boundary);
    }
    text
}

fn ellipsize_text(
    font: &FontArc,
    text: String,
    size: f32,
    max_width: f32,
    canvas_height: u32,
    style: WidgetTextStyle,
) -> String {
    let text = bounded_text(text);
    let scaled = font.as_scaled(PxScale::from(bounded_dimension(size, canvas_height) as f32));
    let advance = |character| scaled.h_advance(scaled.glyph_id(character));
    let max_width = (max_width - style.italic_reach(size) - style.embolden(size) as f32).max(0.0);
    let full_width: f32 = text.chars().map(&advance).sum();
    if full_width <= max_width {
        return text;
    }
    let ellipsis = '…';
    let ellipsis_width = advance(ellipsis);
    if ellipsis_width > max_width {
        return String::new();
    }
    let mut result = String::new();
    let mut width = 0.0;
    for character in text.chars() {
        let next = advance(character);
        if width + next + ellipsis_width > max_width {
            break;
        }
        result.push(character);
        width += next;
    }
    result.push(ellipsis);
    result
}

fn lua_color(lua: &Lua, value: Option<mlua::Value>, fallback: RgbColor) -> mlua::Result<Rgba<u8>> {
    let color = match value {
        Some(value) => lua.from_value(value)?,
        None => fallback,
    };
    Ok(Rgba([color.r, color.g, color.b, 255]))
}

fn canvas_image<'a>(
    ctx: &RenderCtx,
    canvas: &'a mut ByteBuf,
) -> mlua::Result<ImageBuffer<Rgba<u8>, &'a mut [u8]>> {
    ImageBuffer::from_raw(ctx.width, ctx.height, canvas.as_mut_slice())
        .ok_or_else(|| mlua::Error::RuntimeError("invalid widget canvas".into()))
}

fn fit_image<I>(source: &I, width: u32, height: u32, fit: &str) -> image::RgbaImage
where
    I: image::GenericImageView<Pixel = image::Rgba<u8>>,
{
    use image::imageops::FilterType::Triangle;
    let (sw, sh) = source.dimensions();
    if sw == 0 || sh == 0 || width == 0 || height == 0 {
        return image::RgbaImage::new(width, height);
    }
    match fit {
        "cover" => {
            let scale = (width as f32 / sw as f32).max(height as f32 / sh as f32);
            let rw = (sw as f32 * scale).round().max(1.0) as u32;
            let rh = (sh as f32 * scale).round().max(1.0) as u32;
            let resized = image::imageops::resize(source, rw, rh, Triangle);
            image::imageops::crop_imm(
                &resized,
                rw.saturating_sub(width) / 2,
                rh.saturating_sub(height) / 2,
                width,
                height,
            )
            .to_image()
        }
        "contain" => {
            let scale = (width as f32 / sw as f32).min(height as f32 / sh as f32);
            let rw = (sw as f32 * scale).round().max(1.0) as u32;
            let rh = (sh as f32 * scale).round().max(1.0) as u32;
            let resized = image::imageops::resize(source, rw, rh, Triangle);
            let mut target = image::RgbaImage::new(width, height);
            image::imageops::overlay(
                &mut target,
                &resized,
                i64::from(width.saturating_sub(rw) / 2),
                i64::from(height.saturating_sub(rh) / 2),
            );
            target
        }
        _ => image::imageops::resize(source, width, height, Triangle),
    }
}

fn mask_image(image: &mut image::RgbaImage, shape: &str) {
    if !matches!(shape, "circle" | "rounded") {
        return;
    }
    let (width, height) = image.dimensions();
    let radius = if shape == "circle" {
        width.min(height) as f32 / 2.0
    } else {
        width.min(height) as f32 * 0.12
    };
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let outside = if shape == "circle" {
            let dx = x as f32 + 0.5 - width as f32 / 2.0;
            let dy = y as f32 + 0.5 - height as f32 / 2.0;
            dx * dx + dy * dy > radius * radius
        } else {
            let dx = (radius - (x as f32 + 0.5).min(width as f32 - x as f32 - 0.5)).max(0.0);
            let dy = (radius - (y as f32 + 0.5).min(height as f32 - y as f32 - 0.5)).max(0.0);
            dx > 0.0 && dy > 0.0 && dx * dx + dy * dy > radius * radius
        };
        if outside {
            pixel.0[3] = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> FontArc {
        FontArc::try_from_slice(include_bytes!(
            "../../../../../assets/fonts/NotoSans-Regular.ttf"
        ))
        .unwrap()
    }

    fn input(preview: bool) -> WidgetRenderInput {
        WidgetRenderInput {
            widget_id: "meter".to_owned(),
            width: 32,
            height: 24,
            time: 1.0,
            dt: 0.016,
            params: HashMap::new(),
            color: RgbColor { r: 7, g: 8, b: 9 },
            font: font(),
            audio: None,
            media_art: None,
            images: HashMap::new(),
            assets: HashMap::new(),
            preview,
        }
    }

    #[tokio::test]
    async fn live_and_preview_callbacks_are_distinct_and_mandatory() {
        let source = r#"
            return {
                render_widget_meter = function(canvas)
                    canvas:set_u8(0, 11)
                    canvas:set_u8(3, 255)
                end,
                preview_widget_meter = function(canvas)
                    canvas:set_u8(0, 22)
                    canvas:set_u8(3, 255)
                end,
            }
        "#;
        let worker = PluginWidgetHandle::spawn(
            source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        assert_eq!(worker.render(input(false)).await.unwrap()[0], 11);
        assert_eq!(worker.render(input(true)).await.unwrap()[0], 22);

        let missing = PluginWidgetHandle::spawn(
            "return { render_widget_meter = function() end }".to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        assert!(missing.render(input(true)).await.is_err());
    }

    #[tokio::test]
    async fn preview_audio_is_complete_and_deterministic() {
        let source = r#"
            local function check(canvas, ctx)
                local audio = assert(ctx:audio())
                assert(math.abs(audio.level - 0.62) < 0.0001)
                assert(math.abs(audio.flux - 0.18) < 0.0001)
                assert(audio.beat == false and audio.seq == 1 and #audio.bands == 32)
                canvas:set_u8(0, math.floor(audio.bands[1] * 255))
                canvas:set_u8(3, 255)
            end
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx) check(canvas, ctx) end,
                preview_widget_meter = function(canvas, w, h, params, ctx) check(canvas, ctx) end,
            }
        "#;
        let worker = PluginWidgetHandle::spawn(
            source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        let first = worker.render(input(true)).await.unwrap();
        let second = worker.render(input(true)).await.unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn live_context_returns_complete_audio_or_nil() {
        let source = r#"
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx)
                    local audio = assert(ctx:audio())
                    assert(math.abs(audio.level - 0.4) < 0.0001)
                    assert(math.abs(audio.flux - 0.7) < 0.0001)
                    assert(audio.beat == true and audio.seq == 91)
                    assert(#audio.bands == 3 and math.abs(audio.bands[2] - 0.2) < 0.0001)
                    canvas:set_u8(3, 255)
                end,
                preview_widget_meter = function() end,
            }
        "#;
        let worker = PluginWidgetHandle::spawn(
            source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        let mut live = input(false);
        live.audio = Some(WidgetAudioInput {
            level: 0.4,
            flux: 0.7,
            beat: true,
            seq: 91,
            bands: vec![0.1, 0.2, 0.3],
        });
        worker.render(live).await.unwrap();

        let missing_source = r#"
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx)
                    assert(ctx:audio() == nil)
                end,
                preview_widget_meter = function() end,
            }
        "#;
        let missing = PluginWidgetHandle::spawn(
            missing_source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        missing.render(input(false)).await.unwrap();
    }

    #[tokio::test]
    async fn draw_text_is_host_rendered_into_the_canvas() {
        let source = r#"
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx)
                    ctx:draw_text(canvas, "Halo", 1, 1, 15, nil)
                end,
                preview_widget_meter = function(canvas, w, h, params, ctx)
                    ctx:draw_text(canvas, "Halo", 1, 1, 15, nil)
                end,
            }
        "#;
        let worker = PluginWidgetHandle::spawn(
            source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        let pixels = worker.render(input(true)).await.unwrap();
        assert!(pixels.chunks_exact(4).any(|pixel| pixel[3] != 0));
    }

    #[tokio::test]
    async fn composition_clips_rotates_and_applies_opacity() {
        let source = r#"
            local function render(canvas, ctx)
                ctx:push_clip(8, 2, 5, 9)
                ctx:push_opacity(0.5)
                ctx:push_rotation(90, 8, 8)
                ctx:fill_rect(canvas, 2, 4, 8, 4, { r = 255, g = 0, b = 0 })
                ctx:pop_rotation()
                ctx:pop_opacity()
                ctx:pop_clip()

                ctx:draw_polyline(canvas, {
                    { x = 16, y = 3 }, { x = 22, y = 9 }, { x = 28, y = 3 },
                }, { r = 0, g = 255, b = 0 }, 3)
                ctx:draw_polygon(canvas, {
                    { 18, 14 }, { 28, 14 }, { 23, 22 },
                }, false, { r = 0, g = 0, b = 255 }, 4)
            end
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx) render(canvas, ctx) end,
                preview_widget_meter = function(canvas, w, h, params, ctx) render(canvas, ctx) end,
            }
        "#;
        let worker = PluginWidgetHandle::spawn(
            source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        let pixels = worker.render(input(true)).await.unwrap();
        let pixel = |x: usize, y: usize| &pixels[(y * 32 + x) * 4..(y * 32 + x + 1) * 4];

        assert_eq!(pixel(10, 5), [255, 0, 0, 128]);
        assert_eq!(pixel(4, 6), [0, 0, 0, 0]);
        assert!(pixels
            .chunks_exact(4)
            .any(|pixel| pixel[1] != 0 && pixel[3] == 255));
        assert!(pixels
            .chunks_exact(4)
            .any(|pixel| pixel[2] != 0 && pixel[3] == 255));
    }

    #[tokio::test]
    async fn composition_rejects_non_finite_values_and_stack_overflow() {
        let source = r#"
            local function render(canvas, ctx)
                assert(not pcall(function() ctx:push_clip(0 / 0, 0, 1, 1) end))
                assert(not pcall(function() ctx:push_opacity(1 / 0) end))
                assert(not pcall(function() ctx:push_rotation(0, 0 / 0, 0) end))
                assert(not pcall(function() ctx:pop_clip() end))
                assert(not pcall(function() ctx:pop_opacity() end))
                assert(not pcall(function() ctx:pop_rotation() end))
                assert(not pcall(function()
                    ctx:draw_line(canvas, 0 / 0, 0, 1, 1, nil, 1)
                end))
                assert(not pcall(function()
                    ctx:draw_polyline(canvas, { { 0, 0 }, { 1 / 0, 1 } }, nil, 1)
                end))
                assert(not pcall(function()
                    ctx:draw_polygon(canvas, { { 0, 0 }, { 1, 1 }, { 2, 0 } }, false, nil, 0)
                end))
                for _ = 1, 8 do ctx:push_clip(0, 0, 1, 1) end
                assert(not pcall(function() ctx:push_clip(0, 0, 1, 1) end))
                for _ = 1, 8 do ctx:pop_clip() end
                canvas:set_u8(3, 255)
            end
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx) render(canvas, ctx) end,
                preview_widget_meter = function(canvas, w, h, params, ctx) render(canvas, ctx) end,
            }
        "#;
        let worker = PluginWidgetHandle::spawn(
            source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        assert_eq!(worker.render(input(true)).await.unwrap()[3], 255);
    }

    #[tokio::test]
    async fn text_boxes_measure_and_draw_unicode_with_host_layout() {
        let source = r#"
            local style = {
                size = 10,
                horizontal = "center",
                vertical = "middle",
                wrap = "character",
                max_lines = 2,
                overflow = "ellipsis",
            }
            local function render(canvas, ctx)
                local width, height = ctx:measure_text_box("Cafe\u{0301} 世界-long-unbroken", 12, style)
                assert(width > 0 and width <= 12 and height > 10)
                ctx:draw_text_box(canvas, "Cafe\u{0301} 世界-long-unbroken", 4, 2, 12, 20, style, nil)
            end
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx) render(canvas, ctx) end,
                preview_widget_meter = function(canvas, w, h, params, ctx) render(canvas, ctx) end,
            }
        "#;
        let worker = PluginWidgetHandle::spawn(
            source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        let pixels = worker.render(input(true)).await.unwrap();
        let visible: Vec<_> = pixels
            .chunks_exact(4)
            .enumerate()
            .filter(|(_, pixel)| pixel[3] != 0)
            .map(|(index, _)| (index % 32, index / 32))
            .collect();
        assert!(!visible.is_empty());
        assert!(visible
            .iter()
            .all(|&(x, y)| (4..16).contains(&x) && (2..22).contains(&y)));
    }

    #[tokio::test]
    async fn text_boxes_reject_invalid_sizes_and_styles() {
        let source = r#"
            local function render(canvas, ctx)
                local style = { size = 10 }
                assert(not pcall(function() ctx:measure_text_box("x", 0, style) end))
                assert(not pcall(function() ctx:measure_text_box("x", -1, style) end))
                assert(not pcall(function() ctx:measure_text_box("x", 0 / 0, style) end))
                assert(not pcall(function() ctx:measure_text_box("x", 10, { size = 0 }) end))
                assert(not pcall(function() ctx:measure_text_box("x", 10, { size = 10, wrap = "line" }) end))
                assert(not pcall(function() ctx:measure_text_box("x", 10, { size = 10, font = "other" }) end))
                assert(not pcall(function()
                    ctx:draw_text_box(canvas, "x", 0, 0, 10, 0, style, nil)
                end))
                canvas:set_u8(3, 255)
            end
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx) render(canvas, ctx) end,
                preview_widget_meter = function(canvas, w, h, params, ctx) render(canvas, ctx) end,
            }
        "#;
        let worker = PluginWidgetHandle::spawn(
            source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        assert_eq!(worker.render(input(true)).await.unwrap()[3], 255);
    }

    #[test]
    fn text_box_layout_handles_newlines_empty_text_and_long_words() {
        let font = font();
        let style = TextBoxStyle {
            size: 10.0,
            horizontal: TextHorizontalAlign::Left,
            vertical: TextVerticalAlign::Top,
            wrap: TextBoxWrap::Word,
            max_lines: 3,
            overflow: TextBoxOverflow::Ellipsis,
        };
        let font_style = WidgetTextStyle::default();

        let empty = layout_text_box(&font, "", 20.0, style, font_style);
        assert!(empty.lines.is_empty());
        assert_eq!((empty.width, empty.height), (0.0, 0.0));

        let newline = layout_text_box(&font, "first\n\nthird", 100.0, style, font_style);
        assert_eq!(newline.lines.len(), 3);
        assert_eq!(newline.lines[1].text, "");

        let long = layout_text_box(
            &font,
            "supercalifragilisticexpialidocious",
            18.0,
            style,
            font_style,
        );
        assert_eq!(long.lines.len(), 3);
        assert!(long.lines.last().unwrap().text.ends_with('…'));
        assert!(long.lines.iter().all(|line| line.width <= 18.0));
    }

    #[tokio::test]
    async fn text_style_params_are_applied_by_the_host_renderer() {
        let source = r#"
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx)
                    ctx:draw_text(canvas, "Halo", 2, 2, 15, nil)
                end,
                preview_widget_meter = function(canvas, w, h, params, ctx)
                    ctx:draw_text(canvas, "Halo", 2, 2, 15, nil)
                end,
            }
        "#;
        let worker = PluginWidgetHandle::spawn(
            source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        let normal = worker.render(input(true)).await.unwrap();
        let mut styled_input = input(true);
        styled_input.params.insert(
            "text_weight".to_owned(),
            EffectParamValue::Str("bold".to_owned()),
        );
        styled_input
            .params
            .insert("text_italic".to_owned(), EffectParamValue::Bool(true));
        styled_input
            .params
            .insert("text_underline".to_owned(), EffectParamValue::Bool(true));
        styled_input.params.insert(
            "text_strikethrough".to_owned(),
            EffectParamValue::Bool(true),
        );
        let styled = worker.render(styled_input).await.unwrap();
        let visible = |bytes: &[u8]| bytes.chunks_exact(4).filter(|pixel| pixel[3] != 0).count();

        assert_ne!(styled, normal);
        assert!(visible(&styled) > visible(&normal));
    }

    #[tokio::test]
    async fn draw_arc_renders_progress_without_filling_the_center() {
        let source = r#"
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx)
                    ctx:draw_arc(canvas, w / 2, h / 2, 8, 4, 0, 90, 2, nil)
                end,
                preview_widget_meter = function(canvas, w, h, params, ctx)
                    ctx:draw_arc(canvas, w / 2, h / 2, 8, 4, 0, 90, 2, nil)
                end,
            }
        "#;
        let worker = PluginWidgetHandle::spawn(
            source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        let pixels = worker.render(input(true)).await.unwrap();
        let alpha = |x: usize, y: usize| pixels[(y * 32 + x) * 4 + 3];
        assert_ne!(alpha(16, 4), 0, "arc must start at the top");
        assert_ne!(alpha(24, 12), 0, "positive sweep must proceed clockwise");
        assert_eq!(alpha(16, 12), 0, "an arc must not fill its center");
    }

    #[test]
    fn arc_cap_radius_changes_only_the_ends() {
        let render = |cap_radius| {
            let mut bytes = vec![0; 32 * 32 * 4];
            let mut image = ImageBuffer::from_raw(32, 32, bytes.as_mut_slice()).unwrap();
            draw_arc_mut(
                &mut image,
                (16.0, 16.0),
                8.0,
                4.0,
                0.0,
                90.0,
                cap_radius,
                Rgba([1, 2, 3, 255]),
            );
            image.pixels().filter(|pixel| pixel.0[3] != 0).count()
        };

        assert!(render(2.0) > render(0.0));
    }

    #[test]
    fn rounded_rectangle_rows_are_contiguous_and_symmetric() {
        let mut bytes = vec![0; 32 * 24 * 4];
        let mut image = ImageBuffer::from_raw(32, 24, bytes.as_mut_slice()).unwrap();
        fill_rounded_rect_mut(&mut image, 4, 6, 24, 12, 6.0, Rgba([1, 2, 3, 255]));

        let counts: Vec<usize> = (6..18)
            .map(|y| {
                let occupied: Vec<_> = (4..28)
                    .filter(|&x| image.get_pixel(x, y).0[3] != 0)
                    .collect();
                let first = *occupied.first().unwrap();
                let last = *occupied.last().unwrap();
                assert_eq!(occupied.len(), (last - first + 1) as usize);
                occupied.len()
            })
            .collect();

        assert_eq!(counts, counts.iter().rev().copied().collect::<Vec<_>>());
        assert!(counts[..6].windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(counts[5], 24);
    }

    #[tokio::test]
    async fn declared_images_and_assets_render_without_copying_into_lua() {
        let source = r#"
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx)
                    ctx:draw_image(canvas, "photo.png", 0, 0, w / 2, h, "fit", "rect")
                    assert(ctx:draw_asset(canvas, "logo.svg", w / 2, 0, w / 2, h, "fit"))
                    assert(not ctx:draw_asset(canvas, "undeclared.svg", 0, 0, w, h, "fit"))
                end,
                preview_widget_meter = function(canvas, w, h, params, ctx)
                    ctx:draw_image(canvas, "photo.png", 0, 0, w / 2, h, "fit", "rect")
                    assert(ctx:draw_asset(canvas, "logo.svg", w / 2, 0, w / 2, h, "fit"))
                    assert(not ctx:draw_asset(canvas, "undeclared.svg", 0, 0, w, h, "fit"))
                end,
            }
        "#;
        let worker = PluginWidgetHandle::spawn(
            source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        let red = WidgetImageInput {
            width: 2,
            height: 2,
            rgba: [255, 0, 0, 255].repeat(4),
        };
        let green = WidgetImageInput {
            width: 2,
            height: 2,
            rgba: [0, 255, 0, 255].repeat(4),
        };
        let mut render = input(true);
        render.images.insert("photo.png".to_owned(), red);
        render.assets.insert("logo.svg".to_owned(), green);
        let pixels = worker.render(render).await.unwrap();
        assert!(pixels
            .chunks_exact(4)
            .any(|pixel| pixel == [255, 0, 0, 255]));
        assert!(pixels
            .chunks_exact(4)
            .any(|pixel| pixel == [0, 255, 0, 255]));
    }

    #[tokio::test]
    async fn image_opacity_is_composited_by_the_host() {
        let source = r#"
            local function render(canvas, ctx)
                ctx:push_opacity(0.25)
                assert(ctx:draw_asset(canvas, "logo.svg", 0, 0, 8, 8, "fit"))
                ctx:pop_opacity()
            end
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx) render(canvas, ctx) end,
                preview_widget_meter = function(canvas, w, h, params, ctx) render(canvas, ctx) end,
            }
        "#;
        let worker = PluginWidgetHandle::spawn(
            source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        let mut render = input(true);
        render.assets.insert(
            "logo.svg".to_owned(),
            WidgetImageInput {
                width: 2,
                height: 2,
                rgba: [0, 255, 0, 255].repeat(4),
            },
        );
        let pixels = worker.render(render).await.unwrap();
        assert!(pixels.chunks_exact(4).any(|pixel| pixel == [0, 255, 0, 64]));
    }

    #[tokio::test]
    async fn zero_area_rectangles_are_noops_and_do_not_kill_the_worker() {
        let source = r#"
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx)
                    ctx:fill_rect(canvas, 0, 0, 0, h, nil)
                    ctx:fill_rect(canvas, 0, 0, w, 0, nil)
                    ctx:fill_rect(canvas, 0, 0, 1, 1, nil)
                end,
                preview_widget_meter = function(canvas, w, h, params, ctx)
                    ctx:fill_rect(canvas, 0, 0, 0, h, nil)
                    ctx:fill_rect(canvas, 0, 0, w, 0, nil)
                    ctx:fill_rect(canvas, 0, 0, 1, 1, nil)
                end,
            }
        "#;
        let worker = PluginWidgetHandle::spawn(
            source.to_owned(),
            BTreeMap::new(),
            vec!["meter".to_owned()],
            vec![],
            HashMap::new(),
        );
        assert_eq!(worker.render(input(true)).await.unwrap()[3], 255);
        assert!(worker.is_usable());
        assert_eq!(worker.render(input(true)).await.unwrap()[3], 255);
    }

    #[test]
    fn ellipsis_preserves_font_size_and_reveals_more_text_when_wider() {
        let font = font();
        let text = "An exceptionally long 🎵 title".to_owned();
        let narrow = ellipsize_text(
            &font,
            text.clone(),
            18.0,
            70.0,
            128,
            WidgetTextStyle::default(),
        );
        let wide = ellipsize_text(
            &font,
            text.clone(),
            18.0,
            140.0,
            128,
            WidgetTextStyle::default(),
        );
        assert!(narrow.ends_with('…'));
        assert!(wide.chars().count() > narrow.chars().count());
        assert_eq!(
            ellipsize_text(
                &font,
                text.clone(),
                18.0,
                1000.0,
                128,
                WidgetTextStyle::default(),
            ),
            text
        );
    }
}
