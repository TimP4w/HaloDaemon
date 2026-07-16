// SPDX-License-Identifier: GPL-3.0-or-later
//! The data-driven "custom" LCD template: composites a widget list over a
//! procedural or image backdrop, both configured by [`CustomTemplateDef`]
//! (edited by the GUI's LCD editor and passed in as the `widgets_json` param).

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use image::{Pixel as _, Rgba, RgbaImage};

use crate::services::audio::{self, AudioHandle};
use crate::services::media::{self, MediaHandle};
use halod_shared::lcd_custom::{
    param_f64, param_str, BgKind, CustomTemplateDef, WidgetDef, WidgetSprite, TEXT_WEIGHT_PARAM,
};
use halod_shared::types::{
    EffectParamValue, LcdEngineTemplateDescriptor, RgbColor, Sensor, SensorUnit,
};

use super::templates::{dim_color, rgba, Background, TemplateCtx};

struct PluginSprite {
    signature: u64,
    image: RgbaImage,
    composite: CachedComposite,
}

struct CachedComposite {
    signature: u64,
    rotation_bits: u32,
    opacity: u8,
    width: u32,
    height: u32,
    pixels: Vec<CompositePixel>,
}

#[derive(Clone, Copy)]
struct CompositePixel {
    x: u32,
    y: u32,
    color: Rgba<u8>,
}

impl CachedComposite {
    fn from_image(signature: u64, rotation_bits: u32, opacity: u8, image: &RgbaImage) -> Self {
        let pixels = image
            .enumerate_pixels()
            .filter(|(_, _, pixel)| pixel.0[3] != 0)
            .map(|(x, y, &color)| CompositePixel { x, y, color })
            .collect();
        Self {
            signature,
            rotation_bits,
            opacity,
            width: image.width(),
            height: image.height(),
            pixels,
        }
    }
}

pub struct CustomTemplate {
    def: CustomTemplateDef,
    /// Kept so `update_def` can rebuild `bg_image` against the same directory
    /// the session was created with.
    images_dir: std::path::PathBuf,
    /// Only populated for `BgKind::Image`; procedural kinds paint directly.
    bg_image: Option<Background>,
    /// Source-resolution decodes keyed by filename; shared across sized variants.
    decoded_cache: RefCell<HashMap<String, Option<Arc<DecodedImage>>>>,
    static_bg: Option<((u32, u32), RgbaImage)>,
    /// Acquired only when a widget declares audio updates; `None` renders a
    /// silent preview frame.
    audio: Option<Arc<AudioHandle>>,
    /// Acquired only when the layout has a `NowPlaying` widget — `None`
    /// renders the "no player" dimmed placeholder.
    media: Option<Arc<MediaHandle>>,
    plugin_handles: HashMap<String, crate::drivers::plugins::PluginWidgetHandle>,
    plugin_sprites: HashMap<String, PluginSprite>,
    composite_cache: RefCell<HashMap<String, CachedComposite>>,
    system_fonts: HashMap<String, ab_glyph::FontArc>,
    plugin_assets: HashMap<(String, u32), crate::drivers::plugins::WidgetImageInput>,
    plugin_revision: u64,
    last_plugin_render_t: Option<f64>,
}

impl CustomTemplate {
    pub(super) fn descriptor() -> LcdEngineTemplateDescriptor {
        LcdEngineTemplateDescriptor {
            id: "custom".to_string(),
            name: "Custom".to_string(),
            // The GUI builds rows from plugin descriptors, not this generic param UI.
            params: vec![],
        }
    }

    pub(super) fn from_params(
        params: &HashMap<String, EffectParamValue>,
        images_dir: &Path,
    ) -> Self {
        let def = match params.get(halod_shared::lcd_custom::WIDGETS_JSON_PARAM) {
            Some(EffectParamValue::Str(json)) => serde_json::from_str::<CustomTemplateDef>(json)
                .unwrap_or_else(|e| {
                    log::warn!("[LCD custom] invalid widgets_json, using default: {e}");
                    CustomTemplateDef::default()
                }),
            _ => CustomTemplateDef::default(),
        };
        Self::new(def, images_dir)
    }

    pub(super) fn render(&mut self, ctx: &TemplateCtx, buf: &mut RgbaImage) -> Result<(), String> {
        let (w, h) = (ctx.width, ctx.height);
        let accent = self.def.style.accent;

        // Paint the background into buf — reuse the allocation when possible by
        // swapping the rendered image with the caller's buffer.
        let mut img = match &self.def.style.background {
            BgKind::Image { .. } => self
                .bg_image
                .as_mut()
                .map(|bg| bg.canvas(w, h, ctx.t, RgbColor { r: 0, g: 0, b: 0 }))
                .unwrap_or_else(|| RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 255]))),
            BgKind::Solid => self.cached_static_bg(w, h, || {
                RgbaImage::from_pixel(w, h, rgba(dim_color(accent, 0.25)))
            }),
            BgKind::Flow => paint_flow(w, h, ctx.t, accent),
            BgKind::Grid => self.cached_static_bg(w, h, || paint_grid(w, h, accent)),
            BgKind::Glow => self.cached_static_bg(w, h, || paint_glow(w, h, accent)),
        };

        for widget in &self.def.widgets {
            self.render_widget(&mut img, widget, ctx);
        }

        // Swap the freshly-rendered image into the caller's buffer so the
        // caller reuses the same allocation on the next tick and we drop
        // the old (now-empty) allocation.
        std::mem::swap(buf, &mut img);
        Ok(())
    }

    pub(super) fn content_signature(&self, ctx: &TemplateCtx) -> Option<u64> {
        let mut hasher = DefaultHasher::new();
        self.background_signature(ctx.t).hash(&mut hasher);
        for widget in &self.def.widgets {
            self.widget_signature(widget, ctx).hash(&mut hasher);
        }
        Some(hasher.finish())
    }
}

impl CustomTemplate {
    /// Build a live instance from an already-parsed `def`. Shared by
    /// `from_params` (device path) and the editor-sprite path so both render
    /// identical widget content.
    fn new(def: CustomTemplateDef, images_dir: &Path) -> Self {
        let bg_image = match &def.style.background {
            BgKind::Image { filename, dim } => Some(Background::new(filename, *dim, images_dir)),
            _ => None,
        };
        Self {
            def,
            images_dir: images_dir.to_path_buf(),
            bg_image,
            decoded_cache: RefCell::new(HashMap::new()),
            static_bg: None,
            audio: None,
            media: None,
            plugin_handles: HashMap::new(),
            plugin_sprites: HashMap::new(),
            composite_cache: RefCell::new(HashMap::new()),
            system_fonts: HashMap::new(),
            plugin_assets: HashMap::new(),
            plugin_revision: 0,
            last_plugin_render_t: None,
        }
    }

    pub(super) async fn gather_plugin_sprites(
        &mut self,
        ctx: &TemplateCtx<'_>,
        app: &crate::state::AppState,
    ) {
        let dt = self
            .last_plugin_render_t
            .map(|last| (ctx.t - last).max(0.0) as f32)
            .unwrap_or(0.0);
        self.last_plugin_render_t = Some(ctx.t);
        let revision = app.registry.content_revision();
        if revision != self.plugin_revision {
            self.plugin_handles.clear();
            self.plugin_sprites.clear();
            self.composite_cache.borrow_mut().clear();
            self.system_fonts.clear();
            self.plugin_assets.clear();
            self.plugin_revision = revision;
        }
        let sensors: HashMap<String, crate::drivers::plugins::WidgetSensorInput> = ctx
            .sensors
            .iter()
            .map(|(id, sensor)| {
                (
                    id.clone(),
                    crate::drivers::plugins::WidgetSensorInput {
                        value: sensor.value,
                        label: sensor.name.clone(),
                        formatted: format_sensor_value(sensor),
                        unit: sensor_unit(&sensor.unit),
                    },
                )
            })
            .collect();
        let widgets: Vec<WidgetDef> = self.def.widgets.clone();
        let mut audio_active = false;
        let mut media_active = false;
        for widget in widgets {
            let catalog_id = widget.widget.clone();
            let Some(entry) = app.registry.widget_entry(&catalog_id) else {
                self.plugin_sprites.remove(&widget.id);
                self.composite_cache.borrow_mut().remove(&widget.id);
                continue;
            };
            let sensor_updates = update_source_enabled(
                entry.descriptor.updates.sensors,
                entry.descriptor.updates.sensors_when.as_ref(),
                &widget,
            );
            let audio_updates = update_source_enabled(
                entry.descriptor.updates.audio,
                entry.descriptor.updates.audio_when.as_ref(),
                &widget,
            );
            if audio_updates && self.audio.is_none() {
                self.audio = Some(audio::shared());
            }
            audio_active |= audio_updates;
            if entry.descriptor.updates.media && self.media.is_none() {
                self.media = Some(media::shared());
            }
            media_active |= entry.descriptor.updates.media;
            let (_, _, size) = widget_rect(widget.x, widget.y, widget.scale, ctx.width, ctx.height);
            let width = size.round().max(1.0) as u32;
            let height = (size * y_ratio(&widget)).round().max(1.0) as u32;
            let mut hasher = DefaultHasher::new();
            serde_json::to_value(&widget)
                .map(|value| value.to_string())
                .unwrap_or_default()
                .hash(&mut hasher);
            width.hash(&mut hasher);
            height.hash(&mut hasher);
            self.color_for(&widget).r.hash(&mut hasher);
            if let Some(interval) = entry.descriptor.updates.interval_ms {
                ((ctx.t * 1000.0) as u64 / u64::from(interval)).hash(&mut hasher);
            }
            if sensor_updates {
                let mut values: Vec<_> = sensors.iter().collect();
                values.sort_by_key(|(id, _)| *id);
                for (id, sensor) in values {
                    id.hash(&mut hasher);
                    sensor.value.to_bits().hash(&mut hasher);
                }
            }
            if audio_updates {
                self.audio
                    .as_ref()
                    .map(|audio| audio.latest().seq)
                    .unwrap_or(0)
                    .hash(&mut hasher);
            }
            if entry.descriptor.updates.media {
                self.media
                    .as_ref()
                    .and_then(|media| media.latest())
                    .map(|info| {
                        let art = info.art.as_ref().map(|art| Arc::as_ptr(art) as usize);
                        (info.title, info.artist, info.status, art)
                    })
                    .hash(&mut hasher);
            }
            let selected_font = widget
                .font
                .as_deref()
                .unwrap_or(&self.def.style.font)
                .to_owned();
            selected_font.hash(&mut hasher);
            let signature = hasher.finish();
            if self
                .plugin_sprites
                .get(&widget.id)
                .is_some_and(|sprite| sprite.signature == signature)
            {
                continue;
            }
            let handle = match self.plugin_handles.get(&entry.plugin_id) {
                Some(handle) => handle.clone(),
                None => {
                    let Some(handle) = app
                        .registry
                        .build_widget_handle(app.secret_store.as_ref(), &catalog_id)
                    else {
                        continue;
                    };
                    self.plugin_handles
                        .insert(entry.plugin_id.clone(), handle.clone());
                    handle
                }
            };
            let Some(widget_id) = catalog_id
                .strip_prefix(&format!("{}:", entry.plugin_id))
                .map(str::to_owned)
            else {
                continue;
            };
            let audio_frame = audio_updates
                .then(|| self.audio.as_ref().map(|audio| audio.latest()))
                .flatten();
            let media_info = self.media.as_ref().and_then(|media| media.latest());
            let preview = (sensor_updates && sensors.is_empty())
                || (audio_updates && audio_frame.as_ref().is_none_or(|frame| frame.seq == 0))
                || (entry.descriptor.updates.media && media_info.is_none());
            let font = match selected_font.as_str() {
                halod_shared::lcd_custom::FONT_MONO => super::templates::load_mono_font_arc(),
                halod_shared::lcd_custom::FONT_INTER => super::templates::load_inter_font_arc(),
                halod_shared::lcd_custom::FONT_SANS => super::templates::load_font_arc(),
                _ => match self.system_fonts.get(&selected_font) {
                    Some(font) => font.clone(),
                    None => {
                        let font = super::templates::load_system_font_arc(&selected_font)
                            .unwrap_or_else(super::templates::load_font_arc);
                        self.system_fonts
                            .insert(selected_font.clone(), font.clone());
                        font
                    }
                },
            };
            let mut params = widget.params.clone();
            if let Some(weight) = &entry.descriptor.fixed_text_weight {
                params.insert(
                    TEXT_WEIGHT_PARAM.to_owned(),
                    EffectParamValue::Str(weight.clone()),
                );
            }
            let input = crate::drivers::plugins::WidgetRenderInput {
                widget_id,
                width,
                height,
                time: ctx.t as f32,
                dt,
                params,
                color: self.color_for(&widget),
                font,
                sensors: sensors.clone(),
                audio_bands: audio_frame
                    .as_ref()
                    .filter(|frame| frame.seq != 0)
                    .map(|frame| frame.bands.to_vec())
                    .unwrap_or_default(),
                audio_level: audio_frame
                    .as_ref()
                    .filter(|frame| frame.seq != 0)
                    .map(|frame| frame.level),
                media: media_info.map(|info| {
                    let art = info
                        .art
                        .map(|art| crate::drivers::plugins::WidgetImageInput {
                            width: art.width(),
                            height: art.height(),
                            rgba: art.as_raw().clone(),
                        });
                    crate::drivers::plugins::WidgetMediaInput {
                        title: info.title,
                        artist: info.artist,
                        status: match info.status {
                            media::PlaybackStatus::Playing => "playing",
                            media::PlaybackStatus::Paused => "paused",
                            media::PlaybackStatus::Stopped => "stopped",
                        }
                        .to_owned(),
                        art,
                    }
                }),
                images: entry
                    .descriptor
                    .params
                    .iter()
                    .filter(|param| matches!(param.kind, halod_shared::types::ParamKind::Image))
                    .filter_map(|param| {
                        let filename = param_str(&widget, &param.id);
                        let decoded = self.decoded_image(&filename)?;
                        let index =
                            super::templates::frame_at_ms(&decoded.delays, decoded.total_ms, ctx.t);
                        let frame = decoded.frames.get(index)?;
                        Some((
                            filename,
                            crate::drivers::plugins::WidgetImageInput {
                                width: frame.width(),
                                height: frame.height(),
                                rgba: frame.as_raw().clone(),
                            },
                        ))
                    })
                    .collect(),
                assets: {
                    // Bucket resize-time requests so SVGs stay sharp without
                    // retaining one raster allocation for every dragged pixel.
                    let asset_edge = width.max(height).next_power_of_two().min(1024);
                    match self.plugin_assets.get(&(catalog_id.clone(), asset_edge)) {
                        Some(asset) => {
                            HashMap::from([(entry.descriptor.icon.clone(), asset.clone())])
                        }
                        None => match app
                            .registry
                            .read_widget_icon_rgba_at(&catalog_id, asset_edge)
                        {
                            Ok(asset) => {
                                self.plugin_assets
                                    .insert((catalog_id.clone(), asset_edge), asset.clone());
                                HashMap::from([(entry.descriptor.icon.clone(), asset)])
                            }
                            Err(error) => {
                                log::warn!("LCD widget '{catalog_id}' asset failed: {error:#}");
                                HashMap::new()
                            }
                        },
                    }
                },
                preview,
            };
            match handle.render(input).await {
                Ok(bytes) => {
                    if let Some(image) = RgbaImage::from_raw(width, height, bytes) {
                        let composite = CachedComposite::from_image(signature, 0, u8::MAX, &image);
                        self.plugin_sprites.insert(
                            widget.id.clone(),
                            PluginSprite {
                                signature,
                                image,
                                composite,
                            },
                        );
                    }
                }
                Err(error) => {
                    log::warn!("LCD widget '{catalog_id}' render failed: {error:#}");
                    if !handle.is_usable() {
                        // A timeout poisons one plugin VM, not the layout. Drop
                        // the cached handle so the next frame starts a clean VM.
                        self.plugin_handles.remove(&entry.plugin_id);
                    }
                }
            }
        }
        if !audio_active {
            self.audio = None;
        }
        if !media_active {
            self.media = None;
        }
    }

    /// Update `def` in place, reusing `image_cache` and fonts.
    pub(super) fn update_def(&mut self, def: CustomTemplateDef) {
        if def.style.background != self.def.style.background {
            self.bg_image = match &def.style.background {
                BgKind::Image { filename, dim } => {
                    Some(Background::new(filename, *dim, &self.images_dir))
                }
                _ => None,
            };
            self.static_bg = None;
        }
        let live: std::collections::HashSet<&str> = def
            .widgets
            .iter()
            .map(|widget| widget.id.as_str())
            .collect();
        self.composite_cache
            .borrow_mut()
            .retain(|id, _| live.contains(id.as_str()));
        self.def = def;
    }

    /// Delta of widgets whose signature changed since `known`; cheap hash-based skip.
    fn editor_sprites_delta(
        &self,
        ctx: &TemplateCtx,
        known: &HashMap<String, u64>,
    ) -> (Vec<WidgetSprite>, Vec<(String, u64)>) {
        let mut sprites = Vec::new();
        let mut signatures = Vec::with_capacity(self.def.widgets.len());
        for widget in &self.def.widgets {
            let signature = editor_sprite_signature(
                widget,
                ctx.width,
                ctx.height,
                self.widget_signature(widget, ctx),
            );
            signatures.push((widget.id.clone(), signature));
            if known.get(&widget.id) != Some(&signature) {
                sprites.push(self.editor_sprite(widget, ctx, signature));
            }
        }
        (sprites, signatures)
    }

    /// Render one widget's content unrotated; the GUI handles position and rotation.
    fn editor_sprite(&self, widget: &WidgetDef, ctx: &TemplateCtx, signature: u64) -> WidgetSprite {
        let (cw, ch) = (ctx.width, ctx.height);
        let (cx, cy, size) = widget_rect(widget.x, widget.y, widget.scale, cw, ch);
        let opacity = (param_f64(widget, "opacity", 100.0) / 100.0).clamp(0.0, 1.0) as f32;

        // Draw the widget onto a canvas-sized transparent buffer at its true
        // center — pixel-identical to the device's `render_widget_into` — then
        // crop a box centered on that point. Cropping (rather than re-centering)
        // keeps the exact pixels and the same edge clipping as the panel.
        // The buffer is padded `margin` past the canvas so content overhanging
        // the panel edge is captured, not clipped: the GUI clips the sprite to
        // the stage when compositing and rotates it there, so a rotated widget
        // near an edge stays whole instead of keeping the unrotated clip.
        let margin = overhang_margin(size);
        let bw = (cw as i64 + 2 * margin).max(1) as u32;
        let bh = (ch as i64 + 2 * margin).max(1) as u32;
        let (dcx, dcy) = (cx + margin as f32, cy + margin as f32);
        let mut buf = RgbaImage::from_pixel(bw, bh, Rgba([0, 0, 0, 0]));
        self.render_widget_into(&mut buf, widget, dcx, dcy, size, ctx);
        fade_alpha(&mut buf, opacity);

        let icx = dcx.round() as i64;
        let icy = dcy.round() as i64;
        // Box hugs the drawn pixels so the GUI selection outline tracks content
        // (a short text strip isn't wrapped in a full square). Widgets that draw
        // nothing (empty text, Unknown) fall back to a `size`-based box so
        // they're still selectable/resizable.
        let fallback_hw = (size / 2.0).ceil().max(1.0) as i64;
        let fallback_hh = (size * y_ratio(widget) / 2.0).ceil().max(1.0) as i64;
        let (half_w, half_h) = match alpha_bounds(&buf) {
            Some((min_x, min_y, max_x, max_y)) => (
                (icx - min_x).max(max_x + 1 - icx).max(1),
                (icy - min_y).max(max_y + 1 - icy).max(1),
            ),
            None => (fallback_hw, fallback_hh),
        };
        let sw = (2 * half_w).max(1) as u32;
        let sh = (2 * half_h).max(1) as u32;
        let mut sprite = RgbaImage::from_pixel(sw, sh, Rgba([0, 0, 0, 0]));
        // Place buf so that buf(icx, icy) lands at the sprite center (half_w, half_h).
        image::imageops::overlay(&mut sprite, &buf, half_w - icx, half_h - icy);

        WidgetSprite {
            id: widget.id.clone(),
            signature,
            rgba_b64: base64::engine::general_purpose::STANDARD.encode(sprite.as_raw()),
            w: sw,
            h: sh,
        }
    }

    fn cached_static_bg(&mut self, w: u32, h: u32, paint: impl FnOnce() -> RgbaImage) -> RgbaImage {
        if let Some((dims, img)) = &self.static_bg {
            if *dims == (w, h) {
                return img.clone();
            }
        }
        let img = paint();
        self.static_bg = Some(((w, h), img.clone()));
        img
    }

    fn color_for(&self, widget: &WidgetDef) -> RgbColor {
        widget.color.unwrap_or(self.def.style.accent)
    }

    fn decoded_image(&self, filename: &str) -> Option<Arc<DecodedImage>> {
        if let Some(cached) = self.decoded_cache.borrow().get(filename) {
            return cached.clone();
        }
        let decoded = decode_source_image(&self.images_dir, filename);
        self.decoded_cache
            .borrow_mut()
            .insert(filename.to_owned(), decoded.clone());
        decoded
    }

    fn render_widget(&self, img: &mut RgbaImage, widget: &WidgetDef, ctx: &TemplateCtx) {
        let (w, h) = (ctx.width, ctx.height);
        let (cx, cy, size) = widget_rect(widget.x, widget.y, widget.scale, w, h);
        let opacity = (param_f64(widget, "opacity", 100.0) / 100.0).clamp(0.0, 1.0) as f32;
        let theta = rotation_theta(widget.rotation);
        // Fast path: no rotation, fully opaque → draw straight onto the frame.
        if theta.is_none() && opacity >= 0.999 {
            self.render_widget_into(img, widget, cx, cy, size, ctx);
            return;
        }
        if opacity <= 0.001 {
            return;
        }
        let Some(sprite) = self.plugin_sprites.get(&widget.id) else {
            return;
        };
        let rotation_bits = widget.rotation.to_bits();
        let opacity_byte = (opacity * 255.0).round() as u8;
        let rebuild = self
            .composite_cache
            .borrow()
            .get(&widget.id)
            .is_none_or(|cached| {
                cached.signature != sprite.signature
                    || cached.rotation_bits != rotation_bits
                    || cached.opacity != opacity_byte
            });
        if rebuild {
            let mut composite = match theta {
                Some(theta) => rotate_widget_image(&sprite.image, theta),
                None => sprite.image.clone(),
            };
            fade_alpha(&mut composite, opacity);
            let composite = CachedComposite::from_image(
                sprite.signature,
                rotation_bits,
                opacity_byte,
                &composite,
            );
            self.composite_cache
                .borrow_mut()
                .insert(widget.id.clone(), composite);
        }
        let cache = self.composite_cache.borrow();
        let Some(composite) = cache.get(&widget.id) else {
            return;
        };
        overlay_sparse(img, composite, cx, cy);
    }

    fn render_widget_into(
        &self,
        img: &mut RgbaImage,
        widget: &WidgetDef,
        cx: f32,
        cy: f32,
        _size: f32,
        _ctx: &TemplateCtx,
    ) {
        if let Some(sprite) = self.plugin_sprites.get(&widget.id) {
            overlay_sparse(img, &sprite.composite, cx, cy);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn background_signature(&self, t: f64) -> Option<u64> {
        let animated = match &self.def.style.background {
            BgKind::Flow => true,
            BgKind::Image { .. } => self.bg_image.as_ref().is_some_and(Background::is_animated),
            BgKind::Solid | BgKind::Grid | BgKind::Glow => false,
        };
        animated.then(|| hash_value(time_bucket(t)))
    }

    fn widget_signature(&self, widget: &WidgetDef, ctx: &TemplateCtx) -> Option<u64> {
        let _ = ctx;
        self.plugin_sprites
            .get(&widget.id)
            .map(|sprite| sprite.signature)
    }
}

/// Vertical baselines for title/artist: two lines straddle `cy`, one line is centered on it.
const SIGNATURE_TIME_BUCKET_MS: f64 = 10.0;

fn time_bucket(t: f64) -> u64 {
    (t * 1000.0 / SIGNATURE_TIME_BUCKET_MS) as u64
}

fn format_sensor_value(sensor: &Sensor) -> String {
    format!("{:.0}{}", sensor.value, sensor_unit(&sensor.unit))
}

fn sensor_unit(unit: &SensorUnit) -> String {
    match unit {
        SensorUnit::Celsius => " °C",
        SensorUnit::Fahrenheit => " °F",
        SensorUnit::Percent => " %",
        SensorUnit::Megahertz => " MHz",
        SensorUnit::Hours => " h",
        SensorUnit::Rpm => " RPM",
    }
    .to_owned()
}

fn hash_value<T: Hash>(value: T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn update_source_enabled(
    declared: bool,
    condition: Option<&halod_shared::types::LcdParamVisibility>,
    widget: &WidgetDef,
) -> bool {
    declared
        && condition.is_none_or(|condition| {
            matches!(
                widget.params.get(&condition.param),
                Some(EffectParamValue::Str(value)) if value == &condition.equals
            )
        })
}

struct DecodedImage {
    frames: Vec<RgbaImage>,
    delays: Vec<f64>,
    total_ms: f64,
}

/// Budget cap; oversized source images are uniformly downscaled before caching.
const DECODED_IMAGE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Downscale every frame by one uniform factor when their total bytes exceed
/// `budget`, preserving aspect and animation timing. Pure so it's unit-tested.
fn bound_decoded_bytes(frames: Vec<RgbaImage>, budget: usize) -> Vec<RgbaImage> {
    let total: usize = frames.iter().map(|f| f.as_raw().len()).sum();
    if total <= budget {
        return frames;
    }
    let scale = (budget as f64 / total as f64).sqrt();
    frames
        .into_iter()
        .map(|f| {
            let (w, h) = f.dimensions();
            let nw = ((w as f64 * scale) as u32).max(1);
            let nh = ((h as f64 * scale) as u32).max(1);
            image::imageops::resize(&f, nw, nh, image::imageops::FilterType::Triangle)
        })
        .collect()
}

/// Image at on-screen size; frames scaled lazily from the shared source decode.
/// Vertical scale relative to the horizontal `scale`, so a widget's height =
/// `size * y_ratio`. `1.0` (the default) keeps a square/uniform box.
fn y_ratio(w: &WidgetDef) -> f32 {
    if w.scale > 0.0 {
        (halod_shared::lcd_custom::scale_y(w) / w.scale).clamp(0.05, 20.0)
    } else {
        1.0
    }
}

/// Cache key for an `Image` widget at on-screen size; shared by render and prune paths.
fn decode_source_image(images_dir: &Path, filename: &str) -> Option<Arc<DecodedImage>> {
    if halod_shared::types::validate_image_filename(filename).is_err() {
        log::warn!("[LCD custom] rejected image filename: {filename}");
        return None;
    }
    let path = images_dir.join(filename);
    let data = crate::util::image::read_image_bounded(&path)
        .map_err(|e| log::warn!("[LCD custom] cannot read {filename}: {e}"))
        .ok()?;
    let decoded = super::templates::decode_image_frames(&data)
        .map_err(|e| log::warn!("[LCD custom] decode {filename} failed: {e}"))
        .ok()?;
    let (frames, delays): (Vec<RgbaImage>, Vec<f64>) = decoded.into_iter().unzip();
    let frames = bound_decoded_bytes(frames, DECODED_IMAGE_MAX_BYTES);
    let total_ms = delays.iter().copied().filter(|d| d.is_finite()).sum();
    Some(Arc::new(DecodedImage {
        frames,
        delays,
        total_ms,
    }))
}

/// Render the animated flow background.
fn paint_flow(w: u32, h: u32, t: f64, accent: RgbColor) -> RgbaImage {
    let base = dim_color(accent, 0.20);
    let mut img = RgbaImage::from_pixel(w, h, rgba(base));
    let cx = w as f32 * (0.5 + 0.3 * (t * 0.3).sin() as f32);
    let cy = h as f32 * (0.5 + 0.3 * (t * 0.23).cos() as f32);
    let r = (w.min(h) as f32) * 0.6;
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let d = (dx * dx + dy * dy).sqrt() / r;
            let f = (1.0 - d.clamp(0.0, 1.0)) * 0.50;
            if f > 0.0 {
                blend_pixel(&mut img, x, y, accent, f);
            }
        }
    }
    img
}

fn paint_grid(w: u32, h: u32, accent: RgbColor) -> RgbaImage {
    let base = dim_color(accent, 0.16);
    let mut img = RgbaImage::from_pixel(w, h, rgba(base));
    let step = (w.min(h) / 8).max(4);
    let line = dim_color(accent, 0.45);
    for x in (0..w).step_by(step as usize) {
        for y in 0..h {
            img.put_pixel(x, y, rgba(line));
        }
    }
    for y in (0..h).step_by(step as usize) {
        for x in 0..w {
            img.put_pixel(x, y, rgba(line));
        }
    }
    img
}

fn paint_glow(w: u32, h: u32, accent: RgbColor) -> RgbaImage {
    let mut img = RgbaImage::from_pixel(w, h, rgba(dim_color(accent, 0.10)));
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let r = (w.min(h) as f32) * 0.75;
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let d = (dx * dx + dy * dy).sqrt() / r;
            let f = (1.0 - d.clamp(0.0, 1.0)).powf(1.5);
            if f > 0.0 {
                blend_pixel(&mut img, x, y, accent, f);
            }
        }
    }
    img
}

fn blend_pixel(img: &mut RgbaImage, x: u32, y: u32, color: RgbColor, factor: f32) {
    let factor = factor.clamp(0.0, 1.0);
    let px = img.get_pixel_mut(x, y);
    px.0[0] = (px.0[0] as f32 * (1.0 - factor) + color.r as f32 * factor) as u8;
    px.0[1] = (px.0[1] as f32 * (1.0 - factor) + color.g as f32 * factor) as u8;
    px.0[2] = (px.0[2] as f32 * (1.0 - factor) + color.b as f32 * factor) as u8;
}

// ── Widgets ────────────────────────────────────────────────────────────────────

/// Ring-gauge arc (same geometry as `SingleInfographicTemplate`'s gauge, reused
/// as a plain function rather than a whole-template struct).
fn rotation_theta(deg: f32) -> Option<f32> {
    let norm = deg.rem_euclid(360.0);
    (norm > 0.05 && norm < 359.95).then(|| deg.to_radians())
}

/// Rotate only a widget-local image, padded to its exact rotated bounding box.
/// The former path rotated a padded display-sized canvas for every widget and
/// frame, which made a single non-zero rotation dominate LCD render time.
fn rotate_widget_image(source: &RgbaImage, theta: f32) -> RgbaImage {
    let (sin, cos) = theta.sin_cos();
    let (w, h) = (source.width() as f32, source.height() as f32);
    let rotated_w = (w * cos.abs() + h * sin.abs()).ceil().max(1.0) as u32 + 2;
    let rotated_h = (w * sin.abs() + h * cos.abs()).ceil().max(1.0) as u32 + 2;
    let mut padded = RgbaImage::from_pixel(rotated_w, rotated_h, Rgba([0, 0, 0, 0]));
    image::imageops::overlay(
        &mut padded,
        source,
        i64::from(rotated_w.saturating_sub(source.width()) / 2),
        i64::from(rotated_h.saturating_sub(source.height()) / 2),
    );
    imageproc::geometric_transformations::rotate(
        &padded,
        (rotated_w as f32 / 2.0, rotated_h as f32 / 2.0),
        theta,
        imageproc::geometric_transformations::Interpolation::Bilinear,
        Rgba([0, 0, 0, 0]),
    )
}

/// Blend only the non-transparent pixels retained from a transformed widget.
/// Rotated text commonly occupies a small fraction of its axis-aligned bounds,
/// so scanning the whole transparent rectangle on every frame is needlessly
/// expensive even when the rotation itself is cached.
fn overlay_sparse(
    destination: &mut RgbaImage,
    composite: &CachedComposite,
    center_x: f32,
    center_y: f32,
) {
    let origin_x = (center_x - composite.width as f32 / 2.0).round() as i64;
    let origin_y = (center_y - composite.height as f32 / 2.0).round() as i64;
    let destination_width = i64::from(destination.width());
    let destination_height = i64::from(destination.height());

    for pixel in &composite.pixels {
        let x = origin_x + i64::from(pixel.x);
        let y = origin_y + i64::from(pixel.y);
        if x < 0 || y < 0 || x >= destination_width || y >= destination_height {
            continue;
        }
        let destination = destination.get_pixel_mut(x as u32, y as u32);
        if pixel.color.0[3] == u8::MAX {
            *destination = pixel.color;
        } else {
            destination.blend(&pixel.color);
        }
    }
}

/// Normalized-center widget geometry: the panel-space `(cx, cy)` and the shared
/// `widget_size`. Only the center is confined to the panel; content may overflow.
pub(super) fn widget_rect(x: f32, y: f32, scale: f32, w: u32, h: u32) -> (f32, f32, f32) {
    let (w, h) = (w as f32, h as f32);
    let size = halod_shared::lcd_geometry::widget_size(scale, w.min(h));
    (x.clamp(0.0, 1.0) * w, y.clamp(0.0, 1.0) * h, size)
}

/// Border (px) to pad a widget buffer with so content that overhangs the panel
/// edge is captured before rotation/cropping rather than clipped by the canvas.
/// Bounded by the widget's on-screen `size` — the reach of its content from
/// center — so a rotated widget near an edge stays whole. Shared by the device
/// render and the editor-sprite path so the two agree.
fn overhang_margin(size: f32) -> i64 {
    size.ceil().max(1.0) as i64
}

/// Scale every pixel's alpha by `opacity` in place (no-op when fully opaque).
fn fade_alpha(img: &mut RgbaImage, opacity: f32) {
    if opacity >= 0.999 {
        return;
    }
    for px in img.pixels_mut() {
        px.0[3] = (px.0[3] as f32 * opacity).round() as u8;
    }
}

/// Inclusive bounding box `(min_x, min_y, max_x, max_y)` of the non-transparent
/// pixels in `img`, or `None` when every pixel is fully transparent.
fn alpha_bounds(img: &RgbaImage) -> Option<(i64, i64, i64, i64)> {
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
    for (x, y, px) in img.enumerate_pixels() {
        if px.0[3] != 0 {
            let (x, y) = (x as i64, y as i64);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    (max_x >= min_x).then_some((min_x, min_y, max_x, max_y))
}

/// Signature for a widget's editor sprite: changes whenever its rendered
/// CONTENT does. Position/rotation are excluded — sprite content is
/// position-independent (the GUI places and rotates it), so dragging a widget
/// must not invalidate its texture. Serializes via `serde_json::Value` (a
/// `BTreeMap` under the hood, sorting `params` deterministically) rather than
/// `to_string` directly, so key order can't cause spurious signature changes.
fn editor_sprite_signature(widget: &WidgetDef, cw: u32, ch: u32, dynamic: Option<u64>) -> u64 {
    let mut canonical = widget.clone();
    canonical.x = 0.0;
    canonical.y = 0.0;
    canonical.rotation = 0.0;
    let mut hasher = DefaultHasher::new();
    serde_json::to_value(&canonical)
        .map(|v| v.to_string())
        .unwrap_or_default()
        .hash(&mut hasher);
    cw.hash(&mut hasher);
    ch.hash(&mut hasher);
    dynamic.hash(&mut hasher);
    hasher.finish()
}

/// A daemon-side editor preview kept alive across `render_lcd_editor`
/// requests for one device, so its `image_cache` (decoded/resized images and
/// GIF frames) and fonts survive between the GUI's ~200ms polls instead of
/// being rebuilt from scratch every tick.
pub(crate) struct EditorSession {
    pub(crate) device_id: String,
    tmpl: CustomTemplate,
    last_used: std::time::Instant,
}

impl EditorSession {
    /// Whether this session has sat idle longer than `timeout` as of `now`.
    pub(crate) fn is_idle(&self, now: std::time::Instant, timeout: std::time::Duration) -> bool {
        editor_session_is_idle(self.last_used, now, timeout)
    }

    /// Test-only: a session for `device_id` that's already idle (`last_used`
    /// well past [`EDITOR_SESSION_IDLE_TIMEOUT`]), so eviction-path tests
    /// don't need a real sleep.
    #[cfg(test)]
    pub(crate) fn new_idle_for_test(device_id: &str) -> Self {
        Self {
            device_id: device_id.to_string(),
            tmpl: CustomTemplate::new(
                CustomTemplateDef {
                    widgets: vec![],
                    style: halod_shared::lcd_custom::ScreenStyle::default(),
                },
                std::path::Path::new("/tmp"),
            ),
            last_used: std::time::Instant::now() - EDITOR_SESSION_IDLE_TIMEOUT * 2,
        }
    }

    /// See [`CustomTemplate::invalidate_image_cache`].
    pub(crate) fn invalidate_image_cache(&self) {
        self.tmpl.invalidate_image_cache();
    }

    pub(crate) async fn gather_plugin_sprites(
        &mut self,
        cw: u32,
        ch: u32,
        t: f64,
        sensors: &HashMap<String, Sensor>,
        app: &crate::state::AppState,
    ) {
        let ctx = TemplateCtx {
            width: cw,
            height: ch,
            t,
            sensors,
        };
        self.tmpl.gather_plugin_sprites(&ctx, app).await;
    }
}

pub(crate) fn prepare_editor_session<'a>(
    device_id: &str,
    def: &CustomTemplateDef,
    images_dir: &Path,
    session: &'a mut Option<EditorSession>,
) -> &'a mut EditorSession {
    match session.take() {
        Some(mut existing) if existing.device_id == device_id => {
            existing.tmpl.update_def(def.clone());
            session.insert(existing)
        }
        _ => session.insert(EditorSession {
            device_id: device_id.to_owned(),
            tmpl: CustomTemplate::new(def.clone(), images_dir),
            last_used: std::time::Instant::now(),
        }),
    }
}

/// How long an [`EditorSession`] may sit unused before the LCD engine drops
/// it, releasing its decoded-image cache. The GUI polls at ~200ms while the
/// editor tab is open, so idleness this long means the tab was closed.
pub(crate) const EDITOR_SESSION_IDLE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

pub(crate) fn editor_session_is_idle(
    last_used: std::time::Instant,
    now: std::time::Instant,
    timeout: std::time::Duration,
) -> bool {
    now.saturating_duration_since(last_used) > timeout
}

impl CustomTemplate {
    /// Drop decoded user images when the library changes. Resized images live
    /// only for one worker call, so there is no second cache to invalidate.
    pub(crate) fn invalidate_image_cache(&self) {
        self.decoded_cache.borrow_mut().clear();
    }
}

/// Render the widgets of `def` that changed since `known` to [`WidgetSprite`]s
/// against a `cw`×`ch` canvas, using `sensors` for live sensor readings.
/// Reuses `session` when it already belongs to `device_id` (rebuilding only
/// what `update_def` decides needs it); otherwise starts a fresh one so the
/// caller keeps it alive for the next request. Returns the changed sprites
/// plus the current (id, signature) pair for every widget.
#[allow(clippy::too_many_arguments)] // editor render request plus reusable session state
pub(crate) fn render_editor_sprites(
    device_id: &str,
    def: &CustomTemplateDef,
    cw: u32,
    ch: u32,
    sensors: &HashMap<String, Sensor>,
    images_dir: &Path,
    known: &HashMap<String, u64>,
    session: &mut Option<EditorSession>,
) -> (Vec<WidgetSprite>, Vec<(String, u64)>) {
    let s = prepare_editor_session(device_id, def, images_dir, session);
    s.last_used = std::time::Instant::now();

    let ctx = TemplateCtx {
        width: cw,
        height: ch,
        t: 0.0,
        sensors,
    };
    s.tmpl.editor_sprites_delta(&ctx, known)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_session_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<EditorSession>();
    }

    #[test]
    fn editor_session_idle_boundary_is_strict() {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        assert!(!editor_session_is_idle(start, start + timeout, timeout));
        assert!(editor_session_is_idle(
            start,
            start + timeout + std::time::Duration::from_millis(1),
            timeout,
        ));
    }

    #[test]
    fn decoded_frames_are_bounded_by_the_memory_budget() {
        let frames = vec![RgbaImage::new(16, 16); 20];
        let budget = 16 * 16 * 4 * 3;
        let bounded = bound_decoded_bytes(frames, budget);
        assert_eq!(bounded.len(), 20, "animation timing must be preserved");
        assert!(
            bounded
                .iter()
                .map(|frame| frame.as_raw().len())
                .sum::<usize>()
                <= budget
        );
    }

    #[test]
    fn alpha_bounds_ignores_fully_transparent_pixels() {
        let mut image = RgbaImage::new(8, 8);
        image.put_pixel(3, 5, Rgba([255, 0, 0, 255]));
        assert_eq!(alpha_bounds(&image), Some((3, 5, 3, 5)));
    }

    #[test]
    fn rotation_is_bounded_to_the_widget_not_the_display() {
        let source = RgbaImage::from_pixel(100, 20, Rgba([255, 0, 0, 255]));
        let rotated = rotate_widget_image(&source, std::f32::consts::FRAC_PI_2);
        assert!(rotated.width() <= 24);
        assert!(rotated.height() <= 104);
        assert!(rotated.pixels().any(|pixel| pixel.0[3] != 0));
    }

    #[test]
    fn sparse_overlay_matches_image_overlay() {
        let mut source = RgbaImage::new(12, 7);
        source.put_pixel(1, 2, Rgba([255, 0, 0, 255]));
        source.put_pixel(8, 5, Rgba([0, 255, 0, 127]));
        source.put_pixel(11, 0, Rgba([0, 0, 255, 1]));
        let composite = CachedComposite::from_image(0, 0, 255, &source);
        assert_eq!(composite.pixels.len(), 3);

        let background = RgbaImage::from_pixel(20, 20, Rgba([20, 30, 40, 255]));
        let mut expected = background.clone();
        let mut actual = background;
        let (center_x, center_y) = (9.25, 10.75);
        image::imageops::overlay(
            &mut expected,
            &source,
            (center_x - source.width() as f32 / 2.0).round() as i64,
            (center_y - source.height() as f32 / 2.0).round() as i64,
        );
        overlay_sparse(&mut actual, &composite, center_x, center_y);

        assert_eq!(actual, expected);
    }

    #[test]
    fn conditional_update_source_follows_widget_enum() {
        let mut params = HashMap::new();
        params.insert(
            "input".to_owned(),
            EffectParamValue::Str("sensor".to_owned()),
        );
        let widget = WidgetDef {
            id: "gauge".to_owned(),
            widget: "halo_lcd:gauge".to_owned(),
            x: 0.5,
            y: 0.5,
            scale: 1.0,
            rotation: 0.0,
            color: None,
            font: None,
            params,
        };
        let sensor = halod_shared::types::LcdParamVisibility {
            param: "input".to_owned(),
            equals: "sensor".to_owned(),
        };
        let audio = halod_shared::types::LcdParamVisibility {
            param: "input".to_owned(),
            equals: "audio".to_owned(),
        };

        assert!(update_source_enabled(true, Some(&sensor), &widget));
        assert!(!update_source_enabled(true, Some(&audio), &widget));
        assert!(!update_source_enabled(false, Some(&sensor), &widget));
    }
}
