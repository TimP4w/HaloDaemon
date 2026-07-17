// SPDX-License-Identifier: GPL-3.0-or-later
//! Sandboxed renderer for plugin-declared LCD widgets.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::ops::ControlFlow;
use std::rc::Rc;

use ab_glyph::{point, Font, FontArc, PxScale, ScaleFont};
use anyhow::{anyhow, Result};
use image::{ImageBuffer, Pixel as _, Rgba};
use imageproc::drawing::{
    draw_filled_circle_mut, draw_filled_rect_mut, draw_hollow_circle_mut, draw_hollow_polygon_mut,
    draw_line_segment_mut, draw_polygon_mut,
};
use imageproc::point::Point;
use imageproc::rect::Rect;
use mlua::{AnyUserData, Function, Lua, LuaSerdeExt, Table, UserData, UserDataMethods};
use tokio::sync::oneshot;

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
    pub sensors: HashMap<String, WidgetSensorInput>,
    pub audio: Option<WidgetAudioInput>,
    pub media: Option<WidgetMediaInput>,
    pub environment: WidgetEnvironmentInput,
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
pub struct WidgetMediaInput {
    pub title: String,
    pub artist: String,
    pub status: String,
    pub art: Option<WidgetImageInput>,
}

#[derive(Clone)]
pub struct WidgetImageInput {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone)]
pub struct WidgetSensorInput {
    pub value: f64,
    pub label: String,
    pub formatted: String,
    pub unit: String,
    pub sensor_type: String,
    pub stale: bool,
}

#[derive(Clone)]
pub struct WidgetEnvironmentInput {
    pub locale: String,
    pub timezone: String,
    pub temperature_unit: String,
    pub screen_shape: String,
    pub screen_width: u32,
    pub screen_height: u32,
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
}

#[derive(Clone)]
pub struct PluginWidgetHandle(LuaWorker<WidgetCall>);

impl PluginWidgetHandle {
    pub fn spawn(
        source: String,
        modules: BTreeMap<String, String>,
        widget_ids: Vec<String>,
        granted: Vec<Permission>,
        config: crate::plugin::ResolvedConfig,
    ) -> Self {
        let worker = LuaWorker::spawn(
            "halod-lcd-widget",
            "LCD widget",
            CALL_TIMEOUT,
            move || build_ctx(&source, &modules, &widget_ids, &granted, &config),
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
    config: &crate::plugin::ResolvedConfig,
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
        sensors: input.sensors,
        audio: input.audio.or_else(|| input.preview.then(preview_audio)),
        media: input.media.or_else(|| input.preview.then(preview_media)),
        environment: if input.preview {
            WidgetEnvironmentInput {
                locale: "en".to_owned(),
                timezone: "UTC".to_owned(),
                temperature_unit: "celsius".to_owned(),
                screen_shape: input.environment.screen_shape,
                screen_width: input.environment.screen_width,
                screen_height: input.environment.screen_height,
            }
        } else {
            input.environment
        },
        images: input.images,
        assets: input.assets,
        preview: input.preview,
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

fn preview_media() -> WidgetMediaInput {
    WidgetMediaInput {
        title: "Now Playing".to_owned(),
        artist: "HaloDaemon".to_owned(),
        status: "playing".to_owned(),
        art: None,
    }
}

struct RenderCtx {
    width: u32,
    height: u32,
    color: RgbColor,
    font: FontArc,
    text_style: WidgetTextStyle,
    sensors: HashMap<String, WidgetSensorInput>,
    audio: Option<WidgetAudioInput>,
    media: Option<WidgetMediaInput>,
    environment: WidgetEnvironmentInput,
    images: HashMap<String, WidgetImageInput>,
    assets: HashMap<String, WidgetImageInput>,
    preview: bool,
}

impl UserData for RenderCtx {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("is_preview", |_, this, ()| Ok(this.preview));
        methods.add_method("color", |lua, this, ()| lua.to_value(&this.color));
        methods.add_method("sensor_info", |lua, this, id: String| {
            let preview;
            let sensor = match this.sensors.get(&id) {
                Some(sensor) => sensor,
                None if this.preview => {
                    preview = WidgetSensorInput {
                        value: 42.0,
                        label: "Sensor".to_owned(),
                        formatted: "42".to_owned(),
                        unit: "°C".to_owned(),
                        sensor_type: "temperature".to_owned(),
                        stale: false,
                    };
                    &preview
                }
                None => return Ok(None),
            };
            let table = lua.create_table()?;
            table.set("value", sensor.value)?;
            table.set("label", sensor.label.clone())?;
            table.set("formatted", sensor.formatted.clone())?;
            table.set("unit", sensor.unit.clone())?;
            table.set("sensor_type", sensor.sensor_type.clone())?;
            table.set("stale", sensor.stale)?;
            Ok(Some(table))
        });
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
        methods.add_method("media", |lua, this, ()| {
            let Some(media) = &this.media else {
                return Ok(None);
            };
            let table = lua.create_table()?;
            table.set("title", media.title.clone())?;
            table.set("artist", media.artist.clone())?;
            table.set("status", media.status.clone())?;
            table.set("art_available", media.art.is_some())?;
            Ok(Some(table))
        });
        methods.add_method("environment", |lua, this, ()| {
            let table = lua.create_table()?;
            table.set("locale", this.environment.locale.clone())?;
            table.set("timezone", this.environment.timezone.clone())?;
            table.set(
                "temperature_unit",
                this.environment.temperature_unit.clone(),
            )?;
            table.set("screen_shape", this.environment.screen_shape.clone())?;
            table.set("screen_width", this.environment.screen_width)?;
            table.set("screen_height", this.environment.screen_height)?;
            Ok(table)
        });
        methods.add_method(
            "draw_media_art",
            |_, this, (canvas, x, y, width, height): (AnyUserData, f32, f32, f32, f32)| {
                let Some(source) = this.media.as_ref().and_then(|media| media.art.as_ref()) else {
                    return Ok(false);
                };
                let Some(source) =
                    ImageBuffer::from_raw(source.width, source.height, source.rgba.as_slice())
                else {
                    return Ok(false);
                };
                let width = width.round().clamp(1.0, this.width as f32) as u32;
                let height = height.round().clamp(1.0, this.height as f32) as u32;
                let fitted = fit_image(&source, width, height, "cover");
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                let mut target = canvas_image(this, &mut canvas)?;
                image::imageops::overlay(
                    &mut target,
                    &fitted,
                    i64::from(bounded_coord(x, this.width)),
                    i64::from(bounded_coord(y, this.height)),
                );
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
                let width = width.round().clamp(1.0, this.width as f32) as u32;
                let height = height.round().clamp(1.0, this.height as f32) as u32;
                let mut fitted = fit_image(&source, width, height, &fit);
                mask_image(&mut fitted, &shape);
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                let mut target = canvas_image(this, &mut canvas)?;
                image::imageops::overlay(
                    &mut target,
                    &fitted,
                    i64::from(bounded_coord(x, this.width)),
                    i64::from(bounded_coord(y, this.height)),
                );
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
                let width = width.round().clamp(1.0, this.width as f32) as u32;
                let height = height.round().clamp(1.0, this.height as f32) as u32;
                let fitted = fit_image(&source, width, height, &fit);
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                let mut target = canvas_image(this, &mut canvas)?;
                image::imageops::overlay(
                    &mut target,
                    &fitted,
                    i64::from(bounded_coord(x, this.width)),
                    i64::from(bounded_coord(y, this.height)),
                );
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
                let color = lua_color(lua, color, this.color)?;
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                let mut image = canvas_image(this, &mut canvas)?;
                let width = width.round().clamp(0.0, this.width as f32) as u32;
                let height = height.round().clamp(0.0, this.height as f32) as u32;
                if width == 0 || height == 0 {
                    return Ok(());
                }
                draw_filled_rect_mut(
                    &mut image,
                    Rect::at(bounded_coord(x, this.width), bounded_coord(y, this.height))
                        .of_size(width, height),
                    color,
                );
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
                let color = lua_color(lua, color, this.color)?;
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                let mut image = canvas_image(this, &mut canvas)?;
                let width = width.round().clamp(0.0, this.width as f32) as u32;
                let height = height.round().clamp(0.0, this.height as f32) as u32;
                if width == 0 || height == 0 {
                    return Ok(());
                }
                let radius = if radius.is_finite() {
                    radius.round().clamp(0.0, width.min(height) as f32 / 2.0)
                } else {
                    0.0
                };
                fill_rounded_rect_mut(
                    &mut image,
                    bounded_coord(x, this.width),
                    bounded_coord(y, this.height),
                    width,
                    height,
                    radius,
                    color,
                );
                Ok(())
            },
        );
        methods.add_method(
            "draw_line",
            |lua,
             this,
             (canvas, x1, y1, x2, y2, color): (
                AnyUserData,
                f32,
                f32,
                f32,
                f32,
                Option<mlua::Value>,
            )| {
                let color = lua_color(lua, color, this.color)?;
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                let mut image = canvas_image(this, &mut canvas)?;
                draw_line_segment_mut(
                    &mut image,
                    (
                        bounded_coord(x1, this.width) as f32,
                        bounded_coord(y1, this.height) as f32,
                    ),
                    (
                        bounded_coord(x2, this.width) as f32,
                        bounded_coord(y2, this.height) as f32,
                    ),
                    color,
                );
                Ok(())
            },
        );
        methods.add_method(
            "draw_circle",
            |lua,
             this,
             (canvas, x, y, radius, filled, color): (
                AnyUserData,
                f32,
                f32,
                f32,
                bool,
                Option<mlua::Value>,
            )| {
                let color = lua_color(lua, color, this.color)?;
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                let mut image = canvas_image(this, &mut canvas)?;
                let center = (bounded_coord(x, this.width), bounded_coord(y, this.height));
                let radius = bounded_dimension(radius, this.width.max(this.height)) as i32;
                if filled {
                    draw_filled_circle_mut(&mut image, center, radius, color);
                } else {
                    draw_hollow_circle_mut(&mut image, center, radius, color);
                }
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
                let color = lua_color(lua, color, this.color)?;
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                let mut image = canvas_image(this, &mut canvas)?;
                draw_arc_mut(
                    &mut image,
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
                Ok(())
            },
        );
        methods.add_method(
            "draw_triangle",
            |lua,
             this,
             (canvas, x1, y1, x2, y2, x3, y3, filled, color): (
                AnyUserData,
                f32,
                f32,
                f32,
                f32,
                f32,
                f32,
                bool,
                Option<mlua::Value>,
            )| {
                let color = lua_color(lua, color, this.color)?;
                let points = [
                    Point::new(
                        bounded_coord(x1, this.width),
                        bounded_coord(y1, this.height),
                    ),
                    Point::new(
                        bounded_coord(x2, this.width),
                        bounded_coord(y2, this.height),
                    ),
                    Point::new(
                        bounded_coord(x3, this.width),
                        bounded_coord(y3, this.height),
                    ),
                ];
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                let mut image = canvas_image(this, &mut canvas)?;
                if filled {
                    draw_polygon_mut(&mut image, &points, color);
                } else {
                    let points = points.map(|point| Point::new(point.x as f32, point.y as f32));
                    draw_hollow_polygon_mut(&mut image, &points, color);
                }
                Ok(())
            },
        );
        methods.add_method("measure_text", |_, this, (text, size): (String, f32)| {
            let text = bounded_text(text);
            let size = bounded_dimension(size, this.height) as f32;
            Ok(styled_text_size(&this.font, &text, size, this.text_style))
        });
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
                let text = bounded_text(text);
                let mut canvas = canvas.borrow_mut::<ByteBuf>()?;
                let mut image = canvas_image(this, &mut canvas)?;
                let color = lua_color(lua, color, this.color)?;
                draw_styled_text_mut(
                    &mut image,
                    color,
                    bounded_coord(x, this.width),
                    bounded_coord(y, this.height),
                    bounded_dimension(size, this.height) as f32,
                    &this.font,
                    &text,
                    this.text_style,
                );
                Ok(())
            },
        );
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

fn styled_text_size(font: &FontArc, text: &str, size: f32, style: WidgetTextStyle) -> (f32, f32) {
    let scaled = font.as_scaled(PxScale::from(size));
    let width: f32 = text
        .chars()
        .map(|character| scaled.h_advance(scaled.glyph_id(character)))
        .sum();
    (
        width + style.italic_reach(size) + style.embolden(size) as f32,
        scaled.ascent() - scaled.descent() + style.embolden(size).div_ceil(2) as f32,
    )
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
            "../../../../assets/fonts/NotoSans-Regular.ttf"
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
            sensors: HashMap::new(),
            audio: None,
            media: None,
            environment: WidgetEnvironmentInput {
                locale: "de-CH".to_owned(),
                timezone: "+02:00".to_owned(),
                temperature_unit: "celsius".to_owned(),
                screen_shape: "circle".to_owned(),
                screen_width: 320,
                screen_height: 320,
            },
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
    async fn preview_context_records_are_complete_and_deterministic() {
        let source = r#"
            local function check(canvas, ctx)
                local audio = assert(ctx:audio())
                assert(math.abs(audio.level - 0.62) < 0.0001)
                assert(math.abs(audio.flux - 0.18) < 0.0001)
                assert(audio.beat == false and audio.seq == 1 and #audio.bands == 32)
                local media = assert(ctx:media())
                assert(media.title == "Now Playing" and media.artist == "HaloDaemon")
                assert(media.status == "playing" and media.art_available == false)
                local sensor = assert(ctx:sensor_info("missing"))
                assert(sensor.value == 42 and sensor.label == "Sensor")
                assert(sensor.formatted == "42" and sensor.unit == "°C")
                assert(sensor.sensor_type == "temperature" and sensor.stale == false)
                local env = ctx:environment()
                assert(env.locale == "en" and env.timezone == "UTC")
                assert(env.temperature_unit == "celsius" and env.screen_shape == "circle")
                assert(env.screen_width == 320 and env.screen_height == 320)
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
    async fn live_context_returns_complete_records_or_nil() {
        let source = r#"
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx)
                    local audio = assert(ctx:audio())
                    assert(math.abs(audio.level - 0.4) < 0.0001)
                    assert(math.abs(audio.flux - 0.7) < 0.0001)
                    assert(audio.beat == true and audio.seq == 91)
                    assert(#audio.bands == 3 and math.abs(audio.bands[2] - 0.2) < 0.0001)
                    local sensor = assert(ctx:sensor_info("cpu"))
                    assert(sensor.value == 53.5 and sensor.label == "CPU")
                    assert(sensor.formatted == "54 °C" and sensor.unit == " °C")
                    assert(sensor.sensor_type == "temperature" and sensor.stale == false)
                    assert(ctx:sensor_info("missing") == nil)
                    assert(ctx:media() == nil)
                    local env = ctx:environment()
                    assert(env.locale == "de-CH" and env.timezone == "+02:00")
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
        live.sensors.insert(
            "cpu".to_owned(),
            WidgetSensorInput {
                value: 53.5,
                label: "CPU".to_owned(),
                formatted: "54 °C".to_owned(),
                unit: " °C".to_owned(),
                sensor_type: "temperature".to_owned(),
                stale: false,
            },
        );
        worker.render(live).await.unwrap();

        let missing_source = r#"
            return {
                render_widget_meter = function(canvas, w, h, t, dt, params, ctx)
                    assert(ctx:audio() == nil and ctx:media() == nil)
                    assert(ctx:sensor_info("cpu") == nil)
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
                    ctx:draw_asset(canvas, "logo.svg", w / 2, 0, w / 2, h, "fit")
                end,
                preview_widget_meter = function(canvas, w, h, params, ctx)
                    ctx:draw_image(canvas, "photo.png", 0, 0, w / 2, h, "fit", "rect")
                    ctx:draw_asset(canvas, "logo.svg", w / 2, 0, w / 2, h, "fit")
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
