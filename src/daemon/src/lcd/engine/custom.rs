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

use ab_glyph::{FontRef, PxScale};
use base64::Engine as _;
use image::{Rgba, RgbaImage};
use imageproc::drawing::{draw_filled_circle_mut, draw_filled_rect_mut};
use imageproc::rect::Rect;

use crate::services::audio::{self, AudioHandle};
use crate::services::media::{self, MediaHandle, MediaInfo};
use halod_shared::lcd_custom::{
    format_value, param_bool, param_color, param_f64, param_str, param_variant, sensor_fill_color,
    BgKind, CustomTemplateDef, FontKind, WidgetDef, WidgetSprite, WidgetType,
    DEFAULT_GRADIENT_HIGH, DEFAULT_LABEL_COLOR, DEFAULT_TRACK, DEFAULT_VALUE_COLOR,
    SPECTRUM_GRADIENT_HIGH,
};
use halod_shared::lcd_geometry::{
    CLOCK_FONT, DATE_FONT, DEBUG_FONT, NOW_PLAYING_ARTIST, NOW_PLAYING_ART_GAP,
    NOW_PLAYING_TEXT_WIDTH, NOW_PLAYING_TITLE, RING_DOT, RING_START_DEG, RING_SWEEP_DEG,
    RING_THICKNESS, SENSOR_BAR_VALUE_OFFSET, SENSOR_LABEL_FONT, SENSOR_LABEL_OFFSET,
    SENSOR_VALUE_FONT, SHAPE_SIZE, SPECTRUM_HEIGHT, SPECTRUM_WIDTH, TEXT_FONT,
};
use halod_shared::types::{EffectParamValue, LcdEngineTemplateDescriptor, RgbColor, Sensor};

use super::templates::{
    angle_cw_from_top, dim_color, draw_text_bold, load_font, load_inter_font, load_mono_font, rgba,
    text_extent, Background, TemplateCtx,
};

pub struct CustomTemplate {
    def: CustomTemplateDef,
    /// Kept so `update_def` can rebuild `bg_image` against the same directory
    /// the session was created with.
    images_dir: std::path::PathBuf,
    sans: FontRef<'static>,
    mono: FontRef<'static>,
    inter: FontRef<'static>,
    /// Only populated for `BgKind::Image`; procedural kinds paint directly.
    bg_image: Option<Background>,
    /// Source-resolution decodes keyed by filename; shared across sized variants.
    decoded_cache: RefCell<HashMap<String, Option<Arc<DecodedImage>>>>,
    /// Sized image widgets keyed by `image_widget_cache_key`; scaled lazily from `decoded_cache`.
    image_cache: RefCell<HashMap<(String, u32), Option<WidgetImage>>>,
    static_bg: Option<((u32, u32), RgbaImage)>,
    /// Acquired only when the layout has an `AudioSpectrum`/`AudioLevel`
    /// widget — `None` renders a silent (empty) frame.
    audio: Option<Arc<AudioHandle>>,
    /// Acquired only when the layout has a `NowPlaying` widget — `None`
    /// renders the "no player" dimmed placeholder.
    media: Option<Arc<MediaHandle>>,
}

impl CustomTemplate {
    pub(super) fn descriptor() -> LcdEngineTemplateDescriptor {
        LcdEngineTemplateDescriptor {
            id: "custom".to_string(),
            name: "Custom".to_string(),
            // GUI editor builds rows from `widget_schema`, not this generic param UI.
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
        let audio = def
            .widgets
            .iter()
            .any(|w| {
                matches!(
                    w.widget_type,
                    WidgetType::AudioSpectrum | WidgetType::AudioLevel
                )
            })
            .then(audio::shared);
        let media = def
            .widgets
            .iter()
            .any(|w| w.widget_type == WidgetType::NowPlaying)
            .then(media::shared);
        Self {
            def,
            images_dir: images_dir.to_path_buf(),
            sans: load_font(),
            mono: load_mono_font(),
            inter: load_inter_font(),
            bg_image,
            decoded_cache: RefCell::new(HashMap::new()),
            image_cache: RefCell::new(HashMap::new()),
            static_bg: None,
            audio,
            media,
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
        if self.audio.is_none()
            && def.widgets.iter().any(|w| {
                matches!(
                    w.widget_type,
                    WidgetType::AudioSpectrum | WidgetType::AudioLevel
                )
            })
        {
            self.audio = Some(audio::shared());
        }
        if self.media.is_none()
            && def
                .widgets
                .iter()
                .any(|w| w.widget_type == WidgetType::NowPlaying)
        {
            self.media = Some(media::shared());
        }
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

    fn font_for(&self, widget: &WidgetDef) -> &FontRef<'static> {
        match widget.font.unwrap_or(self.def.style.font) {
            FontKind::Mono => &self.mono,
            FontKind::Sans => &self.sans,
            FontKind::Inter => &self.inter,
        }
    }

    fn color_for(&self, widget: &WidgetDef) -> RgbColor {
        widget.color.unwrap_or(self.def.style.accent)
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
        // Rotation must see content that overhangs the panel edge — otherwise
        // the pre-rotation canvas clip is baked in and rotating can't bring it
        // back into view. Render into a buffer padded by `margin` on every side,
        // rotate about the shifted center, then composite back (the overlay
        // reclips to the panel). Opacity-only needs no padding — nothing moves.
        if let Some(theta) = theta {
            let margin = overhang_margin(size);
            let bw = (w as i64 + 2 * margin).max(1) as u32;
            let bh = (h as i64 + 2 * margin).max(1) as u32;
            let (ox, oy) = (margin as f32, margin as f32);
            let mut scratch = RgbaImage::from_pixel(bw, bh, Rgba([0, 0, 0, 0]));
            self.render_widget_into(&mut scratch, widget, cx + ox, cy + oy, size, ctx);
            let mut scratch = imageproc::geometric_transformations::rotate(
                &scratch,
                (cx + ox, cy + oy),
                theta,
                imageproc::geometric_transformations::Interpolation::Bilinear,
                Rgba([0, 0, 0, 0]),
            );
            fade_alpha(&mut scratch, opacity);
            image::imageops::overlay(img, &scratch, -margin, -margin);
            return;
        }
        let mut scratch = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 0]));
        self.render_widget_into(&mut scratch, widget, cx, cy, size, ctx);
        fade_alpha(&mut scratch, opacity);
        image::imageops::overlay(img, &scratch, 0, 0);
    }

    fn render_widget_into(
        &self,
        img: &mut RgbaImage,
        widget: &WidgetDef,
        cx: f32,
        cy: f32,
        size: f32,
        ctx: &TemplateCtx,
    ) {
        let color = self.color_for(widget);
        let font = self.font_for(widget);
        match widget.widget_type {
            WidgetType::Clock => self.render_clock(img, widget, cx, cy, size, color, font, ctx.t),
            WidgetType::Date => self.render_date(img, widget, cx, cy, size, color, font, ctx.t),
            WidgetType::Sensor => {
                self.render_sensor(img, widget, cx, cy, size, color, font, ctx.sensors)
            }
            WidgetType::Text => self.render_text(img, widget, cx, cy, size, color, font),
            WidgetType::Image => self.render_image(img, widget, cx, cy, size, ctx.t),
            WidgetType::Debug => {
                let text = format!("Frame {}", ctx.frame);
                draw_centered_text(img, color, cx, cy, size * DEBUG_FONT, font, &text, 1);
            }
            WidgetType::AudioSpectrum => {
                self.render_audio_spectrum(img, widget, cx, cy, size, color)
            }
            WidgetType::AudioLevel => self.render_audio_level(img, widget, cx, cy, size, color),
            WidgetType::NowPlaying => {
                self.render_now_playing(img, widget, cx, cy, size, font, ctx.t)
            }
            WidgetType::Logo => self.render_logo(img, widget, cx, cy, size, ctx.t),
            WidgetType::Shape => self.render_shape(img, widget, cx, cy, size, color),
            WidgetType::Unknown => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_clock(
        &self,
        img: &mut RgbaImage,
        widget: &WidgetDef,
        cx: f32,
        cy: f32,
        size: f32,
        color: RgbColor,
        font: &FontRef,
        t: f64,
    ) {
        let _ = t;
        let text = clock_text(widget);
        draw_centered_text(img, color, cx, cy, size * CLOCK_FONT, font, &text, 1);
    }

    #[allow(clippy::too_many_arguments)]
    fn render_date(
        &self,
        img: &mut RgbaImage,
        widget: &WidgetDef,
        cx: f32,
        cy: f32,
        size: f32,
        color: RgbColor,
        font: &FontRef,
        t: f64,
    ) {
        let _ = t;
        let text = date_text(widget);
        draw_centered_text(img, color, cx, cy, size * DATE_FONT, font, &text, 1);
    }

    #[allow(clippy::too_many_arguments)]
    fn render_sensor(
        &self,
        img: &mut RgbaImage,
        widget: &WidgetDef,
        cx: f32,
        cy: f32,
        size: f32,
        color: RgbColor,
        font: &FontRef,
        sensors: &HashMap<String, Sensor>,
    ) {
        let sensor_id = param_str(widget, "sensor");
        let label = param_str(widget, "label");
        let sensor = sensors.get(&sensor_id);
        let value_text = sensor_value_text(sensor);
        let label_text = if !label.is_empty() {
            label
        } else {
            sensor.map(|s| s.name.clone()).unwrap_or_default()
        };

        let variant = param_variant(widget, "stat");
        let track = param_color(widget, "track", DEFAULT_TRACK);
        let curve = param_f64(widget, "curve", 0.0) as f32;
        if variant == "ring" || variant == "bar" {
            let min = param_f64(widget, "min", 0.0);
            let max = param_f64(widget, "max", 100.0);
            let span = max - min;
            let frac = match sensor {
                Some(s) if span.abs() > f64::EPSILON => {
                    (((s.value - min) / span).clamp(0.0, 1.0)) as f32
                }
                _ => 0.0,
            };
            let fill = sensor_fill(widget, color, frac);
            let rounded = param_bool(widget, "rounded", false);
            let inverted = param_bool(widget, "inverted", false);
            if variant == "ring" {
                draw_ring(img, cx, cy, size, fill, track, frac);
            } else if curve >= 5.0 {
                draw_arc_bar(
                    img, cx, cy, size, curve, fill, track, frac, inverted, rounded,
                );
            } else {
                draw_bar(img, cx, cy, size, fill, track, frac, inverted, rounded);
            }
        }
        if param_bool(widget, "show_value", true) {
            let value_color = param_color(widget, "value_color", DEFAULT_VALUE_COLOR);
            // A bar (straight or curved) sits at the widget centre, so its value
            // reads above it; a ring/stat centres the value.
            let value_y = if variant == "bar" {
                cy - size * SENSOR_BAR_VALUE_OFFSET
            } else {
                cy
            };
            draw_centered_text(
                img,
                value_color,
                cx,
                value_y,
                size * SENSOR_VALUE_FONT,
                font,
                &value_text,
                1,
            );
        }
        if !label_text.is_empty() {
            let label_color = param_color(widget, "label_color", DEFAULT_LABEL_COLOR);
            draw_centered_text(
                img,
                label_color,
                cx,
                cy + size * SENSOR_LABEL_OFFSET,
                size * SENSOR_LABEL_FONT,
                font,
                &label_text,
                1,
            );
        }
    }

    #[allow(clippy::too_many_arguments)] // text rasterization inputs remain independently meaningful
    fn render_text(
        &self,
        img: &mut RgbaImage,
        widget: &WidgetDef,
        cx: f32,
        cy: f32,
        size: f32,
        color: RgbColor,
        font: &FontRef,
    ) {
        let text = param_str(widget, "text");
        if text.is_empty() {
            return;
        }
        draw_centered_text(img, color, cx, cy, size * TEXT_FONT, font, &text, 1);
    }

    /// Overlays a widget's cached image, advancing an animated GIF by `t`.
    fn render_image(
        &self,
        img: &mut RgbaImage,
        widget: &WidgetDef,
        cx: f32,
        cy: f32,
        size: f32,
        t: f64,
    ) {
        let filename = param_str(widget, "filename");
        if filename.is_empty() {
            return;
        }
        let ImageWidgetKey {
            key,
            ih,
            fit,
            shape,
        } = image_widget_cache_key(widget, size);
        let iw = key.1;
        let mut cache = self.image_cache.borrow_mut();
        let entry = cache.entry(key).or_insert_with(|| {
            self.decoded_image(&filename)
                .map(|src| WidgetImage::new(src, iw, ih, fit, shape))
        });
        if let Some(anim) = entry {
            image::imageops::overlay(
                img,
                anim.frame_at(t),
                (cx - iw as f32 / 2.0).round() as i64,
                (cy - ih as f32 / 2.0).round() as i64,
            );
        }
    }

    /// The source-resolution decode for `filename`, from `decoded_cache` or
    /// disk on first use.
    fn decoded_image(&self, filename: &str) -> Option<Arc<DecodedImage>> {
        if let Some(hit) = self.decoded_cache.borrow().get(filename) {
            return hit.clone();
        }
        let decoded = decode_source_image(filename);
        self.decoded_cache
            .borrow_mut()
            .insert(filename.to_string(), decoded.clone());
        decoded
    }

    /// A geometric primitive: circle / square / rectangle / triangle / line,
    /// filled or outline-only, in the widget's `fill` (or accent) color.
    fn render_shape(
        &self,
        img: &mut RgbaImage,
        widget: &WidgetDef,
        cx: f32,
        cy: f32,
        size: f32,
        color: RgbColor,
    ) {
        use imageproc::drawing::{
            draw_hollow_circle_mut, draw_hollow_rect_mut, draw_line_segment_mut, draw_polygon_mut,
        };
        use imageproc::point::Point;
        let col = rgba(param_color(widget, "fill", color));
        let filled = param_bool(widget, "filled", true);
        let shape = param_str_or(widget, "shape", "circle");
        let half_x = (size * SHAPE_SIZE / 2.0).max(1.0);
        let half_y = (half_x * y_ratio(widget)).max(1.0);
        // Circle/square stay proportional (smaller axis); rect/triangle/line stretch.
        let half = half_x.min(half_y);
        let icx = cx.round() as i32;
        let icy = cy.round() as i32;
        let thick = ((size * 0.03).round() as i32).max(2);
        match shape.as_str() {
            "circle" => {
                let r = half as i32;
                if filled {
                    draw_filled_circle_mut(img, (icx, icy), r, col);
                } else {
                    for k in 0..thick {
                        draw_hollow_circle_mut(img, (icx, icy), (r - k).max(1), col);
                    }
                }
            }
            "square" | "rectangle" => {
                let (hw, hh) = if shape == "square" {
                    (half, half)
                } else {
                    (half_x, half_y)
                };
                let rx = (cx - hw).round() as i32;
                let ry = (cy - hh).round() as i32;
                let rw = (hw * 2.0).max(1.0) as u32;
                let rh = (hh * 2.0).max(1.0) as u32;
                if filled {
                    draw_filled_rect_mut(img, Rect::at(rx, ry).of_size(rw, rh), col);
                } else {
                    for k in 0..thick {
                        let kk = k as u32;
                        let r2 = Rect::at(rx + k, ry + k).of_size(
                            rw.saturating_sub(2 * kk).max(1),
                            rh.saturating_sub(2 * kk).max(1),
                        );
                        draw_hollow_rect_mut(img, r2, col);
                    }
                }
            }
            "triangle" => {
                let top = (cx, cy - half_y);
                let bl = (cx - half_x, cy + half_y);
                let br = (cx + half_x, cy + half_y);
                if filled {
                    let pts = [
                        Point::new(top.0.round() as i32, top.1.round() as i32),
                        Point::new(br.0.round() as i32, br.1.round() as i32),
                        Point::new(bl.0.round() as i32, bl.1.round() as i32),
                    ];
                    draw_polygon_mut(img, &pts, col);
                } else {
                    for e in [(top, br), (br, bl), (bl, top)] {
                        for k in 0..thick {
                            let off = k as f32 - thick as f32 / 2.0;
                            draw_line_segment_mut(
                                img,
                                (e.0 .0, e.0 .1 + off),
                                (e.1 .0, e.1 .1 + off),
                                col,
                            );
                        }
                    }
                }
            }
            "line" => {
                let t = ((size * 0.05).round() as u32).max(2);
                let rect = Rect::at(
                    (cx - half_x).round() as i32,
                    (cy - t as f32 / 2.0).round() as i32,
                )
                .of_size((half_x * 2.0).max(1.0) as u32, t);
                draw_filled_rect_mut(img, rect, col);
            }
            _ => {}
        }
    }

    /// Bundled logo image + "halodaemon" text, each toggleable via
    /// `show_img` / `show_text`.
    fn render_logo(
        &self,
        img: &mut RgbaImage,
        widget: &WidgetDef,
        cx: f32,
        cy: f32,
        size: f32,
        t: f64,
    ) {
        let show_img = param_bool(widget, "show_img", true);
        let show_text = param_bool(widget, "show_text", true);
        let font = self.font_for(widget);
        let gap = size * 0.08;
        let text_px = size * TEXT_FONT * 0.8;
        let (_tw, text_h) = text_extent(font, PxScale::from(text_px), "halodaemon");
        let img_side = if show_img && show_text {
            (size - gap - text_h).min(size * 0.7)
        } else if show_img {
            size
        } else {
            0.0
        };
        let total_h = if show_img && show_text {
            img_side + gap + text_h
        } else if show_img {
            img_side
        } else {
            text_h
        };
        let mut y = cy - total_h / 2.0;
        if show_img && img_side > 0.0 {
            let side = img_side.max(1.0) as u32;
            let mut cache = self.image_cache.borrow_mut();
            let key = (halod_shared::lcd_custom::LOGO_IMAGE.to_string(), side);
            // The logo is a bundled SVG rasterized straight at `side` — no
            // file decode to share, so it skips `decoded_cache`.
            let entry = cache.entry(key.clone()).or_insert_with(|| {
                rasterize_logo(side).map(|frame| {
                    let src = Arc::new(DecodedImage {
                        frames: vec![frame],
                        delays: vec![f64::INFINITY],
                        total_ms: 0.0,
                    });
                    WidgetImage::new(src, side, side, "fit".into(), "rect".into())
                })
            });
            if let Some(anim) = entry {
                image::imageops::overlay(
                    img,
                    anim.frame_at(t),
                    (cx - side as f32 / 2.0).round() as i64,
                    y.round() as i64,
                );
            }
            y += img_side + gap;
        }
        if show_text {
            let white = RgbColor {
                r: 255,
                g: 255,
                b: 255,
            };
            let purple = RgbColor {
                r: 0x9b,
                g: 0x7f,
                b: 0xe0,
            };
            let scale = PxScale::from(text_px);
            let (hw, _) = text_extent(font, scale, "halo");
            let (dw, dh) = text_extent(font, scale, "daemon");
            let tw = hw + dw;
            let x0 = cx - tw / 2.0;
            let by = y + text_h / 2.0 - dh / 2.0;
            draw_text_bold(
                img,
                rgba(white),
                x0.round() as i32,
                by.round() as i32,
                scale,
                font,
                "halo",
                1,
            );
            draw_text_bold(
                img,
                rgba(purple),
                (x0 + hw).round() as i32,
                by.round() as i32,
                scale,
                font,
                "daemon",
                1,
            );
        }
    }

    /// Bottom-aligned bar strip folding the 64 DSP bands down to the widget's
    /// `bands` param by group-mean, `size * SPECTRUM_WIDTH` wide,
    /// `size * SPECTRUM_HEIGHT` tall, with 1-px gaps between bars.
    fn render_audio_spectrum(
        &self,
        img: &mut RgbaImage,
        widget: &WidgetDef,
        cx: f32,
        cy: f32,
        size: f32,
        color: RgbColor,
    ) {
        let frame = self.audio.as_ref().map(|h| h.latest()).unwrap_or_default();
        let n = halod_shared::lcd_geometry::spectrum_bands(param_f64(widget, "bands", 32.0));
        let base = param_color(widget, "fill", color);
        let high = param_color(widget, "gradient_high", SPECTRUM_GRADIENT_HIGH);
        let gradient = param_bool(widget, "gradient", false);
        let flip_h = param_bool(widget, "flip_h", false);
        let flip_v = param_bool(widget, "flip_v", false);
        let mirror = param_bool(widget, "mirror", false);
        let width = size * SPECTRUM_WIDTH;
        let height = size * SPECTRUM_HEIGHT * y_ratio(widget);
        let heights = spectrum_bar_heights(&frame.bands, n, height);

        let x0 = cx - width / 2.0;
        let y0 = cy - height / 2.0;
        let bar_w = width / n as f32;
        for i in 0..n {
            let bx0 = (x0 + i as f32 * bar_w).round() as i32;
            let bx1 = (x0 + (i + 1) as f32 * bar_w).round() as i32 - 1; // 1-px gap
            let bw = (bx1 - bx0).max(1) as u32;
            let h = heights[spectrum_src(i, n, mirror, flip_h)];
            if h < 1.0 {
                continue;
            }
            let by0 = if flip_v { y0 } else { y0 + (height - h) };
            let bh = h.max(1.0) as u32;
            let rect = Rect::at(bx0, by0.round() as i32).of_size(bw, bh);
            let frac = if height > 0.0 { h / height } else { 0.0 };
            draw_filled_rect_mut(
                img,
                rect,
                rgba(sensor_fill_color(base, high, gradient, frac)),
            );
        }
    }

    fn render_audio_level(
        &self,
        img: &mut RgbaImage,
        widget: &WidgetDef,
        cx: f32,
        cy: f32,
        size: f32,
        color: RgbColor,
    ) {
        let frame = self.audio.as_ref().map(|h| h.latest()).unwrap_or_default();
        let level = frame.level.clamp(0.0, 1.0);
        let variant = param_variant(widget, "ring");
        let track = param_color(widget, "track", DEFAULT_TRACK);
        let curve = param_f64(widget, "curve", 0.0) as f32;
        let fill = sensor_fill(widget, color, level);
        let rounded = param_bool(widget, "rounded", false);
        let inverted = param_bool(widget, "inverted", false);
        if variant == "ring" {
            draw_ring(img, cx, cy, size, fill, track, level);
        } else if curve >= 5.0 {
            draw_arc_bar(
                img, cx, cy, size, curve, fill, track, level, inverted, rounded,
            );
        } else {
            draw_bar(img, cx, cy, size, fill, track, level, inverted, rounded);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_now_playing(
        &self,
        img: &mut RgbaImage,
        widget: &WidgetDef,
        cx: f32,
        cy: f32,
        size: f32,
        font: &FontRef,
        t: f64,
    ) {
        let info = self.media.as_ref().and_then(|h| h.latest());
        let show_art = param_bool(widget, "show_art", true);
        let show_title = param_bool(widget, "show_title", true);
        let show_artist = param_bool(widget, "show_artist", true);
        let layout = now_playing_layout(info.as_ref(), show_art, show_title, show_artist);
        // Explicit per-line colors, dimmed together when there's no player.
        let no_media = info.is_none();
        let dim = |c: RgbColor| if no_media { dim_color(c, 0.6) } else { c };
        let title_color = dim(param_color(widget, "title_color", DEFAULT_VALUE_COLOR));
        let artist_color = dim(param_color(widget, "artist_color", DEFAULT_LABEL_COLOR));

        // Fixed text-area width (one base size). The title scrolls within it
        // when it overflows, so the block never shifts as the marquee advances.
        let text_w = size * NOW_PLAYING_TEXT_WIDTH;
        let title_px = size * NOW_PLAYING_TITLE;
        let artist_px = size * NOW_PLAYING_ARTIST;
        let (art_x0, text_x0) = now_playing_geometry(cx, size, layout.want_art, text_w);

        if let Some(art) = &layout.art {
            // Fill the reserved art cell (`size`) regardless of the source
            // resolution. Clamping to the source dimensions would leave a
            // growing empty strip between a small thumbnail and the text as the
            // widget scales up (Windows art is capped at 240px; MPRIS is not).
            let side = size.max(1.0) as u32;
            let resized = image::imageops::resize(
                art.as_ref(),
                side,
                side,
                image::imageops::FilterType::Triangle,
            );
            image::imageops::overlay(
                img,
                &resized,
                art_x0.round() as i64,
                (cy - side as f32 / 2.0).round() as i64,
            );
        }

        // Two lines split above/below center; a single line is centered.
        let (title_y, artist_y) =
            now_playing_line_ys(cy, size, layout.title.is_some(), layout.artist.is_some());
        if let Some(title) = &layout.title {
            draw_scrolling_text(
                img,
                title_color,
                text_x0,
                title_y,
                text_w,
                title_px,
                font,
                title,
                t,
            );
        }
        if let Some(artist) = &layout.artist {
            draw_scrolling_text(
                img,
                artist_color,
                text_x0,
                artist_y,
                text_w,
                artist_px,
                font,
                artist,
                t,
            );
        }
    }

    fn image_is_animated(&self, widget: &WidgetDef, ctx: &TemplateCtx) -> bool {
        let (_, _, size) = widget_rect(widget.x, widget.y, widget.scale, ctx.width, ctx.height);
        let key = image_widget_cache_key(widget, size).key;
        self.image_cache
            .borrow()
            .get(&key)
            .and_then(|o| o.as_ref())
            .is_some_and(WidgetImage::is_animated)
    }

    fn background_signature(&self, t: f64) -> Option<u64> {
        let animated = match &self.def.style.background {
            BgKind::Flow => true,
            BgKind::Image { .. } => self.bg_image.as_ref().is_some_and(Background::is_animated),
            BgKind::Solid | BgKind::Grid | BgKind::Glow => false,
        };
        animated.then(|| hash_value(time_bucket(t)))
    }

    fn widget_signature(&self, widget: &WidgetDef, ctx: &TemplateCtx) -> Option<u64> {
        match widget.widget_type {
            WidgetType::Clock => Some(hash_value(clock_text(widget))),
            WidgetType::Date => Some(hash_value(date_text(widget))),
            WidgetType::Sensor => {
                let sensor_id = param_str(widget, "sensor");
                Some(hash_value(sensor_value_text(ctx.sensors.get(&sensor_id))))
            }
            WidgetType::Text | WidgetType::Shape | WidgetType::Unknown => None,
            WidgetType::Image => self.image_is_animated(widget, ctx).then(|| {
                let mut hasher = DefaultHasher::new();
                param_str(widget, "filename").hash(&mut hasher);
                time_bucket(ctx.t).hash(&mut hasher);
                hasher.finish()
            }),
            WidgetType::Debug => Some(hash_value(ctx.frame)),
            WidgetType::AudioSpectrum | WidgetType::AudioLevel => Some(hash_value(
                self.audio.as_ref().map(|a| a.latest().seq).unwrap_or(0),
            )),
            WidgetType::NowPlaying => Some(self.now_playing_signature(widget, ctx)),
            WidgetType::Logo => None,
        }
    }

    fn now_playing_signature(&self, widget: &WidgetDef, ctx: &TemplateCtx) -> u64 {
        let info = self.media.as_ref().and_then(|h| h.latest());
        let show_art = param_bool(widget, "show_art", true);
        let show_title = param_bool(widget, "show_title", true);
        let show_artist = param_bool(widget, "show_artist", true);
        let layout = now_playing_layout(info.as_ref(), show_art, show_title, show_artist);
        let (_, _, size) = widget_rect(widget.x, widget.y, widget.scale, ctx.width, ctx.height);
        let text_w = size * NOW_PLAYING_TEXT_WIDTH;
        let font = self.font_for(widget);
        let scrolling = layout
            .title
            .as_deref()
            .is_some_and(|t| needs_marquee(font, size * NOW_PLAYING_TITLE, text_w, t))
            || layout
                .artist
                .as_deref()
                .is_some_and(|t| needs_marquee(font, size * NOW_PLAYING_ARTIST, text_w, t));
        let art_id = layout
            .art
            .as_ref()
            .map(|a| Arc::as_ptr(a) as usize)
            .unwrap_or(0);

        let mut hasher = DefaultHasher::new();
        layout.title.hash(&mut hasher);
        layout.artist.hash(&mut hasher);
        art_id.hash(&mut hasher);
        info.as_ref().map(|i| i.status.clone()).hash(&mut hasher);
        if scrolling {
            time_bucket(ctx.t).hash(&mut hasher);
        }
        hasher.finish()
    }
}

/// Vertical baselines for title/artist: two lines straddle `cy`, one line is centered on it.
fn now_playing_line_ys(cy: f32, size: f32, has_title: bool, has_artist: bool) -> (f32, f32) {
    if has_title && has_artist {
        (cy - size * 0.12, cy + size * 0.16)
    } else {
        (cy, cy)
    }
}

/// Left edges of the `NowPlaying` art cell and text block: art (side `size`),
/// a gap, then the text block, all centered on `cx`.
fn now_playing_geometry(cx: f32, size: f32, want_art: bool, text_w: f32) -> (f32, f32) {
    let (art_w, gap) = if want_art {
        (size, size * NOW_PLAYING_ART_GAP)
    } else {
        (0.0, 0.0)
    };
    let left = cx - (art_w + gap + text_w) / 2.0;
    (left, left + art_w + gap)
}

/// Pure layout for the `NowPlaying` widget: what text/art to show, given the
/// current media state. Factored out of `render_now_playing` so it's testable
/// without a live `MediaHandle`.
struct NowPlayingLayout {
    /// The full title (when shown); scrolled/clipped to the text area at draw
    /// time. `None` when `show_title` is off.
    title: Option<String>,
    artist: Option<String>,
    art: Option<Arc<RgbaImage>>,
    /// True when `show_art` was requested, even if no art is available —
    /// lets the caller decide layout without re-reading the param.
    want_art: bool,
}

fn now_playing_layout(
    info: Option<&MediaInfo>,
    show_art: bool,
    show_title: bool,
    show_artist: bool,
) -> NowPlayingLayout {
    let Some(info) = info else {
        return NowPlayingLayout {
            title: show_title.then(|| "-".to_string()),
            artist: None,
            art: None,
            want_art: show_art,
        };
    };
    let artist = show_artist
        .then(|| info.artist.clone())
        .filter(|a| !a.is_empty());
    NowPlayingLayout {
        title: show_title.then(|| info.title.clone()),
        artist,
        art: if show_art { info.art.clone() } else { None },
        want_art: show_art,
    }
}

const MARQUEE_HEAD_PAUSE_SECS: f64 = 2.0;
/// Scroll speed as a multiple of the font's pixel size, per second.
const MARQUEE_SPEED_FACTOR: f32 = 2.5;
/// Gap between the tail of one pass and the head of the next, as a multiple of
/// the font pixel size.
const MARQUEE_SEP_FACTOR: f32 = 2.0;

/// Marquee scroll offset (px): 0 while `text_w` fits `area_w`; otherwise,
/// after a head-pause, advances at `px_per_sec` and wraps every `text_w + sep_w`.
fn marquee_offset_px(text_w: f32, area_w: f32, sep_w: f32, px_per_sec: f32, t: f64) -> f32 {
    if text_w <= area_w {
        return 0.0;
    }
    let cycle = text_w + sep_w;
    let scroll_t = (t - MARQUEE_HEAD_PAUSE_SECS).max(0.0) as f32;
    (scroll_t * px_per_sec).rem_euclid(cycle)
}

const SIGNATURE_TIME_BUCKET_MS: f64 = 10.0;

fn time_bucket(t: f64) -> u64 {
    (t * 1000.0 / SIGNATURE_TIME_BUCKET_MS) as u64
}

fn hash_value<T: Hash>(v: T) -> u64 {
    let mut hasher = DefaultHasher::new();
    v.hash(&mut hasher);
    hasher.finish()
}

fn clock_text(widget: &WidgetDef) -> String {
    let now = chrono::Local::now();
    match param_variant(widget, "24h").as_str() {
        "24h_seconds" => now.format("%H:%M:%S").to_string(),
        "12h" => now.format("%I:%M %p").to_string(),
        _ => now.format("%H:%M").to_string(),
    }
}

fn date_text(widget: &WidgetDef) -> String {
    let now = chrono::Local::now();
    match param_variant(widget, "short").as_str() {
        "numeric" => now.format("%d/%m/%Y").to_string(),
        _ => now.format("%a, %b %d").to_string(),
    }
}

fn sensor_value_text(sensor: Option<&Sensor>) -> String {
    match sensor {
        Some(s) => format_value(s.value, &s.unit),
        None => "--".to_string(),
    }
}

fn needs_marquee(font: &FontRef, px_size: f32, area_w: f32, text: &str) -> bool {
    if text.is_empty() || px_size < 1.0 || area_w < 1.0 {
        return false;
    }
    let (text_w, _) = text_extent(font, PxScale::from(px_size), text);
    text_w > area_w
}

/// Draws `text` clipped to an `area_w`-wide window at `x_left`/`cy`; text
/// that overflows scrolls (marquee), text that fits is static.
#[allow(clippy::too_many_arguments)]
fn draw_scrolling_text(
    img: &mut RgbaImage,
    color: RgbColor,
    x_left: f32,
    cy: f32,
    area_w: f32,
    px_size: f32,
    font: &FontRef,
    text: &str,
    t: f64,
) {
    if text.is_empty() || px_size < 1.0 || area_w < 1.0 {
        return;
    }
    let scale = PxScale::from(px_size);
    let (text_w, line_h) = text_extent(font, scale, text);
    let pad = 2i32; // room for the bold weight (±1 px) and rounding
    let strip_w = area_w.ceil() as u32;
    let strip_h = (line_h.ceil() as i32 + pad * 2).max(1) as u32;
    let mut strip = RgbaImage::new(strip_w, strip_h);

    if text_w <= area_w {
        draw_text_bold(&mut strip, rgba(color), 0, pad, scale, font, text, 1);
    } else {
        let sep_w = px_size * MARQUEE_SEP_FACTOR;
        let off = marquee_offset_px(text_w, area_w, sep_w, px_size * MARQUEE_SPEED_FACTOR, t);
        let x0 = -off.round() as i32;
        draw_text_bold(&mut strip, rgba(color), x0, pad, scale, font, text, 1);
        // A second copy a cycle ahead makes the wrap seamless.
        let x1 = x0 + (text_w + sep_w).round() as i32;
        draw_text_bold(&mut strip, rgba(color), x1, pad, scale, font, text, 1);
    }

    image::imageops::overlay(
        img,
        &strip,
        x_left.round() as i64,
        (cy - strip_h as f32 / 2.0).round() as i64,
    );
}

/// Fold `bands` down to `n` output bars by group-mean, each scaled into
/// `[0, max_h]`. Pure geometry — no rendering — so the fold math is testable
/// independent of pixel writes.
fn spectrum_bar_heights(bands: &[f32; audio::BANDS], n: usize, max_h: f32) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }
    let total = bands.len();
    (0..n)
        .map(|i| {
            let lo = i * total / n;
            let hi = ((i + 1) * total / n).max(lo + 1);
            let group = &bands[lo..hi.min(total)];
            let mean = group.iter().sum::<f32>() / group.len() as f32;
            (mean.clamp(0.0, 1.0) * max_h).clamp(0.0, max_h)
        })
        .collect()
}

/// Maps a spectrum bar position `i` (of `n`) to the band index it renders.
/// `mirror` folds low bands to the centre, rising symmetrically toward both
/// edges — `spectrum_src(i, n, true, _) == spectrum_src(n-1-i, n, true, _)` for
/// every `n`; `flip_h` reverses the order.
fn spectrum_src(i: usize, n: usize, mirror: bool, flip_h: bool) -> usize {
    if mirror {
        let d = ((2 * i as i32 - (n as i32 - 1)).abs() / 2) as usize;
        d.min(n - 1)
    } else if flip_h {
        n - 1 - i
    } else {
        i
    }
}

/// One frame for a static image, many for a GIF, pre-resized to `side`×`side`.
/// Source-resolution decode (one frame static, all frames GIF); `Arc`-shared across sizes.
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
struct WidgetImage {
    src: Arc<DecodedImage>,
    sized: Vec<Option<RgbaImage>>,
    iw: u32,
    ih: u32,
    fit: String,
    shape: String,
}

impl WidgetImage {
    fn new(src: Arc<DecodedImage>, iw: u32, ih: u32, fit: String, shape: String) -> Self {
        let sized = vec![None; src.frames.len()];
        Self {
            src,
            sized,
            iw,
            ih,
            fit,
            shape,
        }
    }

    fn is_animated(&self) -> bool {
        self.src.frames.len() > 1
    }

    /// The sized frame visible at time `t`, scaling it from the source on
    /// first use.
    fn frame_at(&mut self, t: f64) -> &RgbaImage {
        let idx = super::templates::frame_at_ms(&self.src.delays, self.src.total_ms, t);
        self.sized[idx].get_or_insert_with(|| {
            let mut frame = fit_resize(&self.src.frames[idx], self.iw, self.ih, &self.fit);
            apply_shape_mask(&mut frame, &self.shape);
            frame
        })
    }
}

/// A `str` param with a non-empty default (empty/missing → `default`).
fn param_str_or(w: &WidgetDef, key: &str, default: &str) -> String {
    let s = param_str(w, key);
    if s.is_empty() {
        default.to_string()
    } else {
        s
    }
}

/// Vertical scale relative to the horizontal `scale`, so a widget's height =
/// `size * y_ratio`. `1.0` (the default) keeps the legacy square/uniform box.
fn y_ratio(w: &WidgetDef) -> f32 {
    if w.scale > 0.0 {
        (halod_shared::lcd_custom::scale_y(w) / w.scale).clamp(0.05, 20.0)
    } else {
        1.0
    }
}

/// Cache key for an `Image` widget at on-screen size; shared by render and prune paths.
struct ImageWidgetKey {
    key: (String, u32),
    ih: u32,
    fit: String,
    shape: String,
}

fn image_widget_cache_key(widget: &WidgetDef, size: f32) -> ImageWidgetKey {
    let filename = param_str(widget, "filename");
    let iw = size.max(1.0) as u32;
    let ih = (size * y_ratio(widget)).max(1.0) as u32;
    let fit = param_str_or(widget, "fit", "fit");
    let shape = param_str_or(widget, "shape", "rect");
    ImageWidgetKey {
        key: (format!("{filename}\u{1}{fit}\u{1}{shape}\u{1}{ih}"), iw),
        ih,
        fit,
        shape,
    }
}

/// Scale a source frame into a `tw`×`th` frame per the fit mode:
/// `fit` stretches (default, legacy behaviour), `cover` fills and centre-crops,
/// `contain` fits inside and letterboxes on transparency — both aspect-preserving.
fn fit_resize(frame: &RgbaImage, tw: u32, th: u32, fit: &str) -> RgbaImage {
    use image::imageops::FilterType::Triangle;
    let (w, h) = frame.dimensions();
    if w == 0 || h == 0 || tw == 0 || th == 0 {
        return RgbaImage::from_pixel(tw.max(1), th.max(1), Rgba([0, 0, 0, 0]));
    }
    match fit {
        "cover" => {
            let scale = (tw as f32 / w as f32).max(th as f32 / h as f32);
            let nw = (w as f32 * scale).round().max(1.0) as u32;
            let nh = (h as f32 * scale).round().max(1.0) as u32;
            let scaled = image::imageops::resize(frame, nw, nh, Triangle);
            let ox = nw.saturating_sub(tw) / 2;
            let oy = nh.saturating_sub(th) / 2;
            image::imageops::crop_imm(&scaled, ox, oy, tw, th).to_image()
        }
        "contain" => {
            let scale = (tw as f32 / w as f32).min(th as f32 / h as f32);
            let nw = (w as f32 * scale).round().max(1.0) as u32;
            let nh = (h as f32 * scale).round().max(1.0) as u32;
            let scaled = image::imageops::resize(frame, nw, nh, Triangle);
            let mut canvas = RgbaImage::from_pixel(tw, th, Rgba([0, 0, 0, 0]));
            let ox = (tw.saturating_sub(nw) / 2) as i64;
            let oy = (th.saturating_sub(nh) / 2) as i64;
            image::imageops::overlay(&mut canvas, &scaled, ox, oy);
            canvas
        }
        // "fit" (default): stretch to the box, ignoring aspect.
        _ => image::imageops::resize(frame, tw, th, Triangle),
    }
}

/// Punch a `circle`/`rounded` alpha mask into a frame; `rect` (or anything else)
/// leaves it untouched.
fn apply_shape_mask(img: &mut RgbaImage, shape: &str) {
    if shape != "circle" && shape != "rounded" {
        return;
    }
    let (w, h) = img.dimensions();
    let side = w.min(h) as f32;
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let r = side / 2.0;
    let rad = side * 0.18;
    for (x, y, px) in img.enumerate_pixels_mut() {
        let fx = x as f32 + 0.5;
        let fy = y as f32 + 0.5;
        let outside = if shape == "circle" {
            let dx = fx - cx;
            let dy = fy - cy;
            dx * dx + dy * dy > r * r
        } else {
            // Rounded rect: only the corner quarter-circles clip.
            let dx = (rad - fx).max(fx - (w as f32 - rad)).max(0.0);
            let dy = (rad - fy).max(fy - (h as f32 - rad)).max(0.0);
            dx * dx + dy * dy > rad * rad
        };
        if outside {
            px.0[3] = 0;
        }
    }
}

/// Decode `filename` at source resolution, bounded by `DECODED_IMAGE_MAX_BYTES`.
fn decode_source_image(filename: &str) -> Option<Arc<DecodedImage>> {
    if halod_shared::types::validate_image_filename(filename).is_err() {
        log::warn!("[LCD custom] rejected image filename: {filename}");
        return None;
    }
    let path = crate::config::lcd_images_dir().join(filename);
    let data = std::fs::read(&path).ok()?;
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

/// Rasterize the bundled logo SVG to a `side`×`side` straight-alpha RGBA image.
fn rasterize_logo(side: u32) -> Option<RgbaImage> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_data(
        halod_shared::lcd_custom::LOGO_SVG,
        &usvg::Options::default(),
    )
    .ok()?;
    let size = tree.size().to_int_size();
    let long_edge = size.width().max(size.height()) as f32;
    if long_edge <= 0.0 || side == 0 {
        return None;
    }
    let scale = side as f32 / long_edge;
    let mut pixmap = tiny_skia::Pixmap::new(side, side)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut img = RgbaImage::new(side, side);
    for (dst, src) in img.pixels_mut().zip(pixmap.pixels()) {
        let c = src.demultiply();
        *dst = Rgba([c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Some(img)
}

// ── Procedural backgrounds ────────────────────────────────────────────────────

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
fn draw_ring(
    img: &mut RgbaImage,
    cx: f32,
    cy: f32,
    size: f32,
    accent: RgbColor,
    track: RgbColor,
    frac: f32,
) {
    let r_out = size / 2.0;
    let thickness = (r_out * RING_THICKNESS).max(2.0);
    let r_in = r_out - thickness;
    let (w, h) = img.dimensions();
    let (x0, x1) = (
        (cx - r_out).max(0.0) as u32,
        (cx + r_out).min(w as f32) as u32,
    );
    let (y0, y1) = (
        (cy - r_out).max(0.0) as u32,
        (cy + r_out).min(h as f32) as u32,
    );
    for y in y0..y1 {
        for x in x0..x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let r = (dx * dx + dy * dy).sqrt();
            if r < r_in || r > r_out {
                continue;
            }
            let offset = (angle_cw_from_top(dx, dy) - RING_START_DEG).rem_euclid(360.0);
            if offset > RING_SWEEP_DEG {
                continue;
            }
            let ft = offset / RING_SWEEP_DEG;
            let c = if ft <= frac { accent } else { track };
            img.put_pixel(x, y, rgba(c));
        }
    }
    let dot_ang = (RING_START_DEG + frac * RING_SWEEP_DEG).to_radians();
    let r_mid = (r_in + r_out) / 2.0;
    let dot_x = (cx + dot_ang.sin() * r_mid) as i32;
    let dot_y = (cy - dot_ang.cos() * r_mid) as i32;
    let dot_r = (thickness * RING_DOT).max(2.0) as i32;
    draw_filled_circle_mut(img, (dot_x, dot_y), dot_r, rgba(accent));
}

/// Flat progress bar: a rounded-rect `track` with a rounded-rect `fill`. The
/// fill's corner radius is clamped to half its own width, so a short fill reads
/// as a small rounded sliver rather than a full-height ball.
#[allow(clippy::too_many_arguments)] // primitive geometry/style parameters
fn draw_bar(
    img: &mut RgbaImage,
    cx: f32,
    cy: f32,
    size: f32,
    fill: RgbColor,
    track: RgbColor,
    frac: f32,
    inverted: bool,
    rounded: bool,
) {
    let thickness = (size * 0.10).max(2.0);
    let frac = frac.clamp(0.0, 1.0);
    let x0 = (cx - size / 2.0).round() as i32;
    let y0 = (cy - thickness / 2.0).round() as i32;
    let sw = size.max(1.0) as u32;
    let th = thickness.max(1.0) as u32;
    let r = if rounded { th as i32 / 2 } else { 0 };
    draw_round_rect(img, Rect::at(x0, y0).of_size(sw, th), r, track);
    let fill_w = (size * frac).round() as i32;
    if fill_w >= 1 {
        let fx = if inverted {
            (cx + size / 2.0).round() as i32 - fill_w // right-aligned
        } else {
            x0 // left-aligned
        };
        let fr = r.min(fill_w / 2);
        draw_round_rect(img, Rect::at(fx, y0).of_size(fill_w as u32, th), fr, fill);
    }
}

/// Fill a rectangle with rounded corners of radius `r` (clamped to fit). At
/// `r == 0` it's a plain rectangle; when `r` reaches half the shorter side it's
/// a full capsule.
fn draw_round_rect(img: &mut RgbaImage, rect: Rect, r: i32, color: RgbColor) {
    let c = rgba(color);
    let w = rect.width() as i32;
    let h = rect.height() as i32;
    let r = r.min(w / 2).min(h / 2).max(0);
    if r <= 0 {
        draw_filled_rect_mut(img, rect, c);
        return;
    }
    let (l, t) = (rect.left(), rect.top());
    // Cross of two rects (full-width band inset top/bottom, full-height band
    // inset left/right) plus the four corner circles. Zero-size bands are
    // skipped — imageproc's `Rect::of_size` panics on a zero dimension.
    let band_h = h - 2 * r;
    if band_h > 0 {
        draw_filled_rect_mut(img, Rect::at(l, t + r).of_size(w as u32, band_h as u32), c);
    }
    let band_w = w - 2 * r;
    if band_w > 0 {
        draw_filled_rect_mut(img, Rect::at(l + r, t).of_size(band_w as u32, h as u32), c);
    }
    for (cxp, cyp) in [
        (l + r, t + r),
        (l + w - 1 - r, t + r),
        (l + r, t + h - 1 - r),
        (l + w - 1 - r, t + h - 1 - r),
    ] {
        draw_filled_circle_mut(img, (cxp, cyp), r, c);
    }
}

/// A progress bar bent into an arc of `curve` degrees, opening downward, its
/// chord spanning `size` and centered on `(cx, cy)`. Rendered as a band of
/// radial quads (flat, square ends); `rounded` adds semicircular end caps.
/// `inverted` fills from the far (right) end instead of the near (left) end.
#[allow(clippy::too_many_arguments)]
fn draw_arc_bar(
    img: &mut RgbaImage,
    cx: f32,
    cy: f32,
    size: f32,
    curve: f32,
    fill: RgbColor,
    track: RgbColor,
    frac: f32,
    inverted: bool,
    rounded: bool,
) {
    use imageproc::point::Point;
    let thickness = (size * 0.10).max(2.0);
    let seg = thickness / 2.0;
    let half = (curve.clamp(5.0, 180.0) / 2.0).to_radians();
    let r = size / 2.0 / half.sin();
    let oy = cy - r * half.cos();
    let point = |t: f32| {
        let a = -half + 2.0 * half * t;
        (cx + r * a.sin(), oy + r * a.cos())
    };
    // Inner/outer edge points, offset along the radial normal by half-thickness.
    let edges = |t: f32| {
        let (px, py) = point(t);
        let (nx, ny) = ((px - cx) / r, (py - oy) / r);
        (
            (px + nx * seg, py + ny * seg),
            (px - nx * seg, py - ny * seg),
        )
    };
    let frac = frac.clamp(0.0, 1.0);
    let filled = |t: f32| if inverted { t >= 1.0 - frac } else { t <= frac };
    let steps = (curve.ceil().max(16.0)) as usize;
    for i in 0..steps {
        let (t0, t1) = (i as f32 / steps as f32, (i + 1) as f32 / steps as f32);
        let color = if filled((t0 + t1) / 2.0) { fill } else { track };
        let (o0, i0) = edges(t0);
        let (o1, i1) = edges(t1);
        let quad = [
            Point::new(o0.0.round() as i32, o0.1.round() as i32),
            Point::new(o1.0.round() as i32, o1.1.round() as i32),
            Point::new(i1.0.round() as i32, i1.1.round() as i32),
            Point::new(i0.0.round() as i32, i0.1.round() as i32),
        ];
        // draw_polygon_mut panics on a degenerate (repeated-point) polygon.
        if quad[0] != quad[1] && quad[2] != quad[3] {
            imageproc::drawing::draw_polygon_mut(img, &quad, rgba(color));
        }
    }
    if rounded {
        let cap = seg.max(1.0) as i32;
        for t in [0.0_f32, 1.0] {
            let (px, py) = point(t);
            let color = if filled(t) { fill } else { track };
            draw_filled_circle_mut(
                img,
                (px.round() as i32, py.round() as i32),
                cap,
                rgba(color),
            );
        }
    }
}

/// Fill color for a sensor gauge/meter from its `fill`/`gradient` params.
fn sensor_fill(widget: &WidgetDef, base: RgbColor, frac: f32) -> RgbColor {
    let fill = param_color(widget, "fill", base);
    let high = param_color(widget, "gradient_high", DEFAULT_GRADIENT_HIGH);
    let gradient = param_bool(widget, "gradient", false);
    sensor_fill_color(fill, high, gradient, frac)
}

#[allow(clippy::too_many_arguments)] // primitive text geometry/style parameters
fn draw_centered_text(
    img: &mut RgbaImage,
    color: RgbColor,
    cx: f32,
    cy: f32,
    px_size: f32,
    font: &FontRef,
    text: &str,
    weight: i32,
) {
    if text.is_empty() || px_size < 1.0 {
        return;
    }
    let scale = PxScale::from(px_size);
    let (text_w, line_h) = text_extent(font, scale, text);
    let x = (cx - text_w / 2.0).round() as i32;
    let y = (cy - line_h / 2.0).round() as i32;
    draw_text_bold(img, rgba(color), x, y, scale, font, text, weight);
}

/// Clockwise radians, or `None` for an insignificant multiple of a full turn.
fn rotation_theta(deg: f32) -> Option<f32> {
    let norm = deg.rem_euclid(360.0);
    (norm > 0.05 && norm < 359.95).then(|| deg.to_radians())
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
}

/// How long an [`EditorSession`] may sit unused before the LCD engine drops
/// it, releasing its decoded-image cache. The GUI polls at ~200ms while the
/// editor tab is open, so idleness this long means the tab was closed.
pub(crate) const EDITOR_SESSION_IDLE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Bounds `image_cache` growth from resize-drag key churn (each drag tick can
/// mint a new `(filename, side)` key) independent of how many distinct
/// filenames are actually referenced.
const IMAGE_CACHE_CAP: usize = 32;

/// Whether an idle [`EditorSession`] should be dropped, given `last_used` and
/// `now`. Pure so the 30s policy is unit-tested without a real sleep.
pub(crate) fn editor_session_is_idle(
    last_used: std::time::Instant,
    now: std::time::Instant,
    timeout: std::time::Duration,
) -> bool {
    now.saturating_duration_since(last_used) > timeout
}

/// What's still referenced by the current widget list, given the render ctx's
/// `cw`×`ch`: `Image` widgets contribute their exact `image_cache` key (mirrors
/// `render_image`'s computation, so a resize/param change evicts the old key
/// precisely); `Logo` widgets contribute the bundled-logo filename only — their
/// `side` comes from text metrics and isn't cheaply reproducible here, so they
/// stay matched by filename. Anything in `image_cache` matching neither is
/// stale and safe to evict.
fn referenced_image_cache(
    def: &CustomTemplateDef,
    cw: u32,
    ch: u32,
) -> (
    std::collections::HashSet<(String, u32)>,
    std::collections::HashSet<String>,
) {
    let mut exact_keys = std::collections::HashSet::new();
    let mut logo_filenames = std::collections::HashSet::new();
    for widget in &def.widgets {
        match widget.widget_type {
            WidgetType::Image => {
                if param_str(widget, "filename").is_empty() {
                    continue;
                }
                let (_, _, size) = widget_rect(widget.x, widget.y, widget.scale, cw, ch);
                exact_keys.insert(image_widget_cache_key(widget, size).key);
            }
            WidgetType::Logo => {
                logo_filenames.insert(halod_shared::lcd_custom::LOGO_IMAGE.to_string());
            }
            _ => {}
        }
    }
    (exact_keys, logo_filenames)
}

/// Keep referenced keys, cap count; tested.
fn prune_image_cache_keys(
    keys: Vec<(String, u32)>,
    exact_keys: &std::collections::HashSet<(String, u32)>,
    logo_filenames: &std::collections::HashSet<String>,
    cap: usize,
) -> Vec<(String, u32)> {
    let mut kept: Vec<_> = keys
        .into_iter()
        .filter(|key| {
            exact_keys.contains(key)
                || logo_filenames.contains(key.0.split('\u{1}').next().unwrap_or(""))
        })
        .collect();
    kept.truncate(cap);
    kept
}

impl CustomTemplate {
    /// Drop unreferenced `image_cache` entries and cap the remainder.
    fn prune_image_cache(&self, cw: u32, ch: u32) {
        let (exact_keys, logo_filenames) = referenced_image_cache(&self.def, cw, ch);
        let mut cache = self.image_cache.borrow_mut();
        let keys: Vec<(String, u32)> = cache.keys().cloned().collect();
        let keep = prune_image_cache_keys(keys, &exact_keys, &logo_filenames, IMAGE_CACHE_CAP);
        let keep: std::collections::HashSet<_> = keep.into_iter().collect();
        cache.retain(|k, _| keep.contains(k));

        // Source decodes survive only while some Image widget references the
        // file — dropping the last widget for a GIF releases its frames.
        let referenced: std::collections::HashSet<String> = self
            .def
            .widgets
            .iter()
            .filter(|w| w.widget_type == WidgetType::Image)
            .map(|w| param_str(w, "filename"))
            .collect();
        self.decoded_cache
            .borrow_mut()
            .retain(|filename, _| referenced.contains(filename));
    }

    /// Drop every cached decoded image. Called when the image library changes
    /// underneath a live [`EditorSession`] (upload/delete) so a stale decode
    /// isn't served until the next unrelated prune happens to evict it.
    pub(crate) fn invalidate_image_cache(&self) {
        self.image_cache.borrow_mut().clear();
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
    let reusable = matches!(session, Some(s) if s.device_id == device_id);
    if reusable {
        let s = session.as_mut().unwrap();
        s.tmpl.update_def(def.clone());
    } else {
        *session = Some(EditorSession {
            device_id: device_id.to_string(),
            tmpl: CustomTemplate::new(def.clone(), images_dir),
            last_used: std::time::Instant::now(),
        });
    }
    let s = session.as_mut().unwrap();
    s.last_used = std::time::Instant::now();

    let ctx = TemplateCtx {
        width: cw,
        height: ch,
        t: 0.0,
        frame: 0,
        sensors,
    };
    let result = s.tmpl.editor_sprites_delta(&ctx, known);
    s.tmpl.prune_image_cache(cw, ch);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::lcd_custom::{ScreenStyle, WIDGETS_JSON_PARAM};
    use halod_shared::types::{SensorType, SensorUnit};
    use proptest::prelude::*;

    /// `EditorSession` is held in a `std::sync::Mutex` field on `AppState`,
    /// which is shared across `tokio::spawn`ed tasks — it must be `Send`.
    #[test]
    fn editor_session_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<EditorSession>();
    }

    #[test]
    fn editor_session_is_idle_respects_timeout() {
        let t0 = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        assert!(!editor_session_is_idle(t0, t0, timeout));
        assert!(!editor_session_is_idle(
            t0,
            t0 + std::time::Duration::from_secs(29),
            timeout
        ));
        assert!(editor_session_is_idle(
            t0,
            t0 + std::time::Duration::from_secs(31),
            timeout
        ));
    }

    #[test]
    fn prune_image_cache_keys_drops_unreferenced_and_caps_count() {
        let exact: std::collections::HashSet<(String, u32)> =
            [("a.png\u{1}fit\u{1}rect\u{1}10".to_string(), 10)].into();
        let logo_filenames = std::collections::HashSet::new();
        let keys = vec![
            ("a.png\u{1}fit\u{1}rect\u{1}10".to_string(), 10),
            ("a.png\u{1}fit\u{1}rect\u{1}20".to_string(), 20),
            ("b.png\u{1}fit\u{1}rect\u{1}10".to_string(), 10),
        ];
        let kept = prune_image_cache_keys(keys, &exact, &logo_filenames, 32);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0, "a.png\u{1}fit\u{1}rect\u{1}10");

        let many: Vec<_> = (0..10)
            .map(|i| ("a.png\u{1}fit\u{1}rect\u{1}10".to_string(), i))
            .collect();
        let exact_many: std::collections::HashSet<(String, u32)> = many.iter().cloned().collect();
        let kept = prune_image_cache_keys(many, &exact_many, &logo_filenames, 3);
        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn prune_image_cache_keys_matches_logo_by_filename_only() {
        let exact = std::collections::HashSet::new();
        let logo_filenames: std::collections::HashSet<String> =
            [halod_shared::lcd_custom::LOGO_IMAGE.to_string()].into();
        let keys = vec![
            (halod_shared::lcd_custom::LOGO_IMAGE.to_string(), 40),
            ("other.png\u{1}fit\u{1}rect\u{1}10".to_string(), 10),
        ];
        let kept = prune_image_cache_keys(keys, &exact, &logo_filenames, 32);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0, halod_shared::lcd_custom::LOGO_IMAGE);
    }

    #[test]
    fn referenced_image_cache_covers_image_and_logo_widgets() {
        let mut img_widget = widget(WidgetType::Image);
        img_widget.params.insert(
            "filename".to_string(),
            EffectParamValue::Str("pic.png".to_string()),
        );
        let logo_widget = widget(WidgetType::Logo);
        let def = def_with(vec![img_widget.clone(), logo_widget], BgKind::Flow);
        let (exact_keys, logo_filenames) = referenced_image_cache(&def, 240, 240);
        let (_, _, size) = widget_rect(img_widget.x, img_widget.y, img_widget.scale, 240, 240);
        let expected_key = image_widget_cache_key(&img_widget, size).key;
        assert!(exact_keys.contains(&expected_key));
        assert!(logo_filenames.contains(halod_shared::lcd_custom::LOGO_IMAGE));
    }

    fn image_test_widget(filename: &str) -> WidgetDef {
        let mut w = widget(WidgetType::Image);
        w.params.insert(
            "filename".to_string(),
            EffectParamValue::Str(filename.to_string()),
        );
        w
    }

    #[test]
    fn prune_keeps_a_live_image_widget_entry() {
        let def = def_with(vec![image_test_widget("pic.png")], BgKind::Flow);
        let mut tmpl = CustomTemplate::new(def, Path::new("/tmp"));
        let sensors = HashMap::new();
        let mut buf = RgbaImage::new(240, 240);
        tmpl.render(&ctx(&sensors, 240, 240), &mut buf).unwrap();
        assert_eq!(tmpl.image_cache.borrow().len(), 1);

        tmpl.prune_image_cache(240, 240);

        assert_eq!(
            tmpl.image_cache.borrow().len(),
            1,
            "prune must not evict a still-referenced Image widget's cache entry"
        );
    }

    #[test]
    fn prune_evicts_on_widget_removal_and_size_change() {
        let def = def_with(vec![image_test_widget("pic.png")], BgKind::Flow);
        let mut tmpl = CustomTemplate::new(def, Path::new("/tmp"));
        let sensors = HashMap::new();
        let mut buf = RgbaImage::new(240, 240);
        tmpl.render(&ctx(&sensors, 240, 240), &mut buf).unwrap();
        assert_eq!(tmpl.image_cache.borrow().len(), 1);

        // Removing the widget drops its entry.
        tmpl.update_def(def_with(vec![], BgKind::Flow));
        tmpl.prune_image_cache(240, 240);
        assert!(tmpl.image_cache.borrow().is_empty());

        // Re-adding, then changing scale (so the widget's on-screen size
        // changes), drops the old size's key rather than accumulating it.
        let mut resized = image_test_widget("pic.png");
        tmpl.update_def(def_with(vec![resized.clone()], BgKind::Flow));
        tmpl.render(&ctx(&sensors, 240, 240), &mut buf).unwrap();
        assert_eq!(tmpl.image_cache.borrow().len(), 1);
        let old_key = tmpl.image_cache.borrow().keys().next().unwrap().clone();

        resized.scale *= 2.0;
        tmpl.update_def(def_with(vec![resized], BgKind::Flow));
        tmpl.render(&ctx(&sensors, 240, 240), &mut buf).unwrap();
        tmpl.prune_image_cache(240, 240);

        let cache = tmpl.image_cache.borrow();
        assert!(
            !cache.contains_key(&old_key),
            "old size's cache entry must be evicted once the widget resizes"
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn render_editor_sprites_reuses_session_across_calls_for_same_device() {
        let def = def_with(vec![text_test_widget("a")], BgKind::Flow);
        let sensors = HashMap::new();
        let mut session = None;
        render_editor_sprites(
            "dev1",
            &def,
            240,
            240,
            &sensors,
            Path::new("/tmp"),
            &HashMap::new(),
            &mut session,
        );
        assert!(session.is_some());
        render_editor_sprites(
            "dev1",
            &def,
            240,
            240,
            &sensors,
            Path::new("/tmp"),
            &HashMap::new(),
            &mut session,
        );
        assert_eq!(session.as_ref().unwrap().device_id, "dev1");
    }

    #[test]
    fn render_editor_sprites_replaces_session_for_a_different_device() {
        let def = def_with(vec![text_test_widget("a")], BgKind::Flow);
        let sensors = HashMap::new();
        let mut session = None;
        render_editor_sprites(
            "dev1",
            &def,
            240,
            240,
            &sensors,
            Path::new("/tmp"),
            &HashMap::new(),
            &mut session,
        );
        render_editor_sprites(
            "dev2",
            &def,
            240,
            240,
            &sensors,
            Path::new("/tmp"),
            &HashMap::new(),
            &mut session,
        );
        assert_eq!(session.as_ref().unwrap().device_id, "dev2");
    }

    fn text_test_widget(id: &str) -> WidgetDef {
        let mut w = widget(WidgetType::Text);
        w.id = id.to_string();
        w.params
            .insert("text".to_string(), EffectParamValue::Str("HI".to_string()));
        w
    }

    fn ctx<'a>(sensors: &'a HashMap<String, Sensor>, w: u32, h: u32) -> TemplateCtx<'a> {
        TemplateCtx {
            width: w,
            height: h,
            t: 0.0,
            frame: 0,
            sensors,
        }
    }

    /// Test convenience: a fresh (no session, empty `known`) full render, so
    /// every widget comes back as a sprite — matching the old non-delta API
    /// these tests were written against.
    fn full_render(
        def: &CustomTemplateDef,
        sensors: &HashMap<String, Sensor>,
    ) -> (Vec<WidgetSprite>, Vec<(String, u64)>) {
        let mut session = None;
        render_editor_sprites(
            "dev",
            def,
            240,
            240,
            sensors,
            Path::new("/tmp"),
            &HashMap::new(),
            &mut session,
        )
    }

    fn def_with(widgets: Vec<WidgetDef>, background: BgKind) -> CustomTemplateDef {
        CustomTemplateDef {
            widgets,
            style: ScreenStyle {
                background,
                ..Default::default()
            },
        }
    }

    fn params_for(def: &CustomTemplateDef) -> HashMap<String, EffectParamValue> {
        let mut p = HashMap::new();
        p.insert(
            WIDGETS_JSON_PARAM.to_string(),
            EffectParamValue::Str(serde_json::to_string(def).unwrap()),
        );
        p
    }

    fn widget(widget_type: WidgetType) -> WidgetDef {
        WidgetDef {
            id: "w1".to_string(),
            widget_type,
            x: 0.5,
            y: 0.5,
            scale: 1.0,
            rotation: 0.0,
            color: None,
            font: None,
            params: HashMap::new(),
        }
    }

    #[test]
    fn descriptor_has_no_generic_params() {
        assert!(CustomTemplate::descriptor().params.is_empty());
        assert_eq!(CustomTemplate::descriptor().id, "custom");
    }

    #[test]
    fn edge_widget_sprite_keeps_offcanvas_pixels_for_rotation() {
        use base64::Engine;
        let opaque = |s: &WidgetSprite| {
            base64::engine::general_purpose::STANDARD
                .decode(&s.rgba_b64)
                .unwrap()
                .chunks_exact(4)
                .filter(|p| p[3] != 0)
                .count()
        };
        let mut centered = widget(WidgetType::Shape);
        centered.scale = 2.0;
        centered.params.insert(
            "shape".to_string(),
            EffectParamValue::Str("circle".to_string()),
        );
        centered
            .params
            .insert("filled".to_string(), EffectParamValue::Bool(true));
        let mut edge = centered.clone();
        edge.x = 1.0; // centered on the right panel edge → half overhangs
        let sensors = HashMap::new();
        let (c, _) = full_render(&def_with(vec![centered], BgKind::Flow), &sensors);
        let (e, _) = full_render(&def_with(vec![edge], BgKind::Flow), &sensors);
        // The overhanging widget must retain the same drawn pixels as the
        // centered one — nothing is clipped away before the GUI rotates and
        // clips the sprite, so a rotated edge widget stays whole.
        assert_eq!(opaque(&c[0]), opaque(&e[0]));
    }

    #[test]
    fn text_sprite_box_hugs_glyphs_not_full_widget_box() {
        let mut w = widget(WidgetType::Text);
        w.params
            .insert("text".to_string(), EffectParamValue::Str("HI".to_string()));
        let def = def_with(vec![w], BgKind::Flow);
        let sensors = HashMap::new();
        let (sprites, _) = full_render(&def, &sensors);
        // A short glyph strip must be far shorter than the full square widget
        // box it would occupy under the old fallback-floor behaviour.
        let full = halod_shared::lcd_geometry::widget_size(1.0, 240.0) as u32;
        assert!(
            sprites[0].h < full,
            "text sprite height {} should hug glyphs, not fill the {} box",
            sprites[0].h,
            full
        );
    }

    #[test]
    fn build_custom_succeeds() {
        let _ = CustomTemplate::from_params(&HashMap::new(), Path::new("/tmp"));
    }

    #[test]
    fn garbage_widgets_json_still_renders() {
        let mut params = HashMap::new();
        params.insert(
            WIDGETS_JSON_PARAM.to_string(),
            EffectParamValue::Str("not json at all".to_string()),
        );
        let mut tmpl = CustomTemplate::from_params(&params, Path::new("/tmp"));
        let sensors = HashMap::new();
        let mut buf = RgbaImage::new(240, 240);
        tmpl.render(&ctx(&sensors, 240, 240), &mut buf).unwrap();
        assert_eq!(buf.dimensions(), (240, 240));
    }

    // `NowPlaying` acquires `media::shared()`, which spawns a tokio task —
    // needs a runtime, unlike `audio::shared()`'s plain std::thread.
    #[tokio::test]
    async fn every_widget_type_and_variant_smoke_renders() {
        let variants: &[(WidgetType, &[&str])] = &[
            (WidgetType::Clock, &["24h", "24h_seconds", "12h"]),
            (WidgetType::Date, &["short", "numeric"]),
            (WidgetType::Sensor, &["stat", "ring", "bar"]),
            (WidgetType::Text, &[""]),
            (WidgetType::Image, &[""]),
            (WidgetType::Logo, &[""]),
            (WidgetType::Debug, &[""]),
            (WidgetType::AudioSpectrum, &[""]),
            (WidgetType::AudioLevel, &["ring", "bar"]),
            (WidgetType::NowPlaying, &[""]),
            (WidgetType::Unknown, &[""]),
        ];
        let mut sensors = HashMap::new();
        sensors.insert(
            "cpu".to_string(),
            Sensor {
                id: "cpu".to_string(),
                name: "CPU".to_string(),
                value: 55.0,
                unit: SensorUnit::Celsius,
                sensor_type: SensorType::default(),
                visibility: Default::default(),
            },
        );
        for &(widget_type, variants) in variants {
            for &variant in variants {
                let mut w = widget(widget_type);
                w.params.insert(
                    "variant".to_string(),
                    EffectParamValue::Str(variant.to_string()),
                );
                w.params.insert(
                    "sensor".to_string(),
                    EffectParamValue::Str("cpu".to_string()),
                );
                w.params
                    .insert("text".to_string(), EffectParamValue::Str("hi".to_string()));
                let def = def_with(vec![w], BgKind::Flow);
                let params = params_for(&def);
                let mut tmpl = CustomTemplate::from_params(&params, Path::new("/tmp"));
                let mut buf = RgbaImage::new(240, 240);
                tmpl.render(&ctx(&sensors, 240, 240), &mut buf).unwrap();
                assert_eq!(
                    buf.dimensions(),
                    (240, 240),
                    "type={widget_type:?} variant={variant}"
                );
            }
        }
    }

    #[test]
    fn rotation_theta_ignores_full_turns_and_keeps_partial() {
        assert_eq!(rotation_theta(0.0), None);
        assert_eq!(rotation_theta(360.0), None);
        assert_eq!(rotation_theta(-360.0), None);
        assert!((rotation_theta(90.0).unwrap() - 90.0_f32.to_radians()).abs() < 1e-6);
        // Negative rotation keeps its sign (clockwise convention preserved).
        assert!(rotation_theta(-90.0).unwrap() < 0.0);
    }

    #[test]
    fn rotate_moves_content_clockwise() {
        // A pixel right of centre lands below centre after +90° (clockwise).
        let mut img = RgbaImage::from_pixel(5, 5, Rgba([0, 0, 0, 0]));
        img.put_pixel(4, 2, Rgba([255, 255, 255, 255]));
        let rotated = imageproc::geometric_transformations::rotate(
            &img,
            (2.0, 2.0),
            90.0_f32.to_radians(),
            imageproc::geometric_transformations::Interpolation::Nearest,
            Rgba([0, 0, 0, 0]),
        );
        assert!(
            rotated.get_pixel(2, 4)[3] > 0,
            "pixel should rotate to below centre"
        );
        assert_eq!(
            rotated.get_pixel(4, 2)[3],
            0,
            "original spot should be cleared"
        );
    }

    #[test]
    fn spectrum_max_bands_matches_audio_dsp_bands() {
        assert_eq!(halod_shared::lcd_geometry::SPECTRUM_MAX_BANDS, audio::BANDS);
    }

    #[test]
    fn rounded_straight_bar_actually_rounds_its_corners() {
        // A rounded straight meter must clear its corners; a square one fills
        // them. Guards the `r = th/2` fix (round()-vs-floor mismatch used to
        // silently downgrade some sizes to a plain rectangle).
        let fill = RgbColor { r: 255, g: 0, b: 0 };
        let track = RgbColor {
            r: 0,
            g: 80,
            b: 200,
        };
        // cx=60,size=80 → x∈[20,100]; cy=30,thickness=8 → y∈[26,34]. Corner (20,26).
        let mut rounded = RgbaImage::from_pixel(120, 60, Rgba([0, 0, 0, 0]));
        draw_bar(
            &mut rounded,
            60.0,
            30.0,
            80.0,
            fill,
            track,
            0.0,
            false,
            true,
        );
        let mut square = RgbaImage::from_pixel(120, 60, Rgba([0, 0, 0, 0]));
        draw_bar(
            &mut square,
            60.0,
            30.0,
            80.0,
            fill,
            track,
            0.0,
            false,
            false,
        );
        assert_eq!(
            rounded.get_pixel(20, 26)[3],
            0,
            "rounded bar corner must be empty"
        );
        assert!(
            square.get_pixel(20, 26)[3] > 0,
            "square bar corner must be filled"
        );
    }

    #[test]
    fn sensor_meter_styles_smoke_render() {
        let mut sensors = HashMap::new();
        sensors.insert("cpu".to_string(), sensor_at("cpu", 55.0));
        // Every meter/gauge styling combo must render at full panel size.
        // (variant, rounded, curve, inverted, gradient)
        let combos: &[(&str, bool, f64, bool, bool)] = &[
            ("bar", false, 0.0, false, false),
            ("bar", true, 0.0, true, true), // rounded + inverted + gradient
            ("bar", false, 90.0, false, true), // square arc + gradient
            ("bar", true, 130.0, true, false), // rounded arc + inverted
            ("ring", false, 0.0, false, true), // gauge + gradient
        ];
        for &(variant, rounded, curve, inverted, gradient) in combos {
            let mut w = sensor_widget();
            w.params
                .insert("variant".to_string(), EffectParamValue::Str(variant.into()));
            w.params
                .insert("rounded".to_string(), EffectParamValue::Bool(rounded));
            w.params
                .insert("curve".to_string(), EffectParamValue::Float(curve));
            w.params
                .insert("inverted".to_string(), EffectParamValue::Bool(inverted));
            w.params
                .insert("gradient".to_string(), EffectParamValue::Bool(gradient));
            let def = def_with(vec![w], BgKind::Solid);
            let mut tmpl = CustomTemplate::from_params(&params_for(&def), Path::new("/tmp"));
            let mut buf = RgbaImage::new(240, 240);
            tmpl.render(&ctx(&sensors, 240, 240), &mut buf).unwrap();
            assert_eq!(
                buf.dimensions(),
                (240, 240),
                "variant={variant} rounded={rounded} curve={curve}"
            );
        }
    }

    #[test]
    fn rotated_widget_smoke_renders() {
        let mut w = widget(WidgetType::Text);
        w.rotation = 37.0;
        w.params
            .insert("text".to_string(), EffectParamValue::Str("hi".to_string()));
        let def = def_with(vec![w], BgKind::Solid);
        let mut tmpl = CustomTemplate::from_params(&params_for(&def), Path::new("/tmp"));
        let sensors = HashMap::new();
        let mut buf = RgbaImage::new(240, 240);
        tmpl.render(&ctx(&sensors, 240, 240), &mut buf).unwrap();
        assert_eq!(buf.dimensions(), (240, 240));
    }

    #[test]
    fn each_background_kind_smoke_renders() {
        for background in [BgKind::Flow, BgKind::Solid, BgKind::Grid, BgKind::Glow] {
            let def = def_with(vec![], background);
            let params = params_for(&def);
            let mut tmpl = CustomTemplate::from_params(&params, Path::new("/tmp"));
            let sensors = HashMap::new();
            let mut buf = RgbaImage::new(240, 240);
            tmpl.render(&ctx(&sensors, 240, 240), &mut buf).unwrap();
            assert_eq!(buf.dimensions(), (240, 240));
        }
    }

    #[test]
    fn now_playing_geometry_matches_the_gui_editor() {
        // Pinned against `ui/src/device/lcd_editor.rs::now_playing_geometry`.
        // Art cell (side=size) + gap (size*0.25) + text block, centered on cx.
        // With art, text_w=60, cx=100, size=40: gap=10, total=110, left=45,
        // art_x0=45, text_x0=45+40+10=95.
        assert_eq!(now_playing_geometry(100.0, 40.0, true, 60.0), (45.0, 95.0));
        // No art: text-only block centered — text_x0 == left == cx - text_w/2.
        assert_eq!(now_playing_geometry(100.0, 40.0, false, 60.0), (70.0, 70.0));
    }

    #[test]
    fn now_playing_line_ys_center_single_line_split_two() {
        // Both lines: title above center, artist below.
        assert_eq!(now_playing_line_ys(100.0, 50.0, true, true), (94.0, 108.0));
        // One line only (title or artist): centered on cy.
        assert_eq!(
            now_playing_line_ys(100.0, 50.0, true, false),
            (100.0, 100.0)
        );
        assert_eq!(
            now_playing_line_ys(100.0, 50.0, false, true),
            (100.0, 100.0)
        );
    }

    #[test]
    fn spectrum_bar_heights_zero_bands_is_empty() {
        assert!(spectrum_bar_heights(&[0.0; audio::BANDS], 0, 100.0).is_empty());
    }

    #[test]
    fn spectrum_bar_heights_all_zero_bands_are_zero() {
        let heights = spectrum_bar_heights(&[0.0; audio::BANDS], 8, 100.0);
        assert_eq!(heights.len(), 8);
        assert!(heights.iter().all(|&h| h == 0.0));
    }

    #[test]
    fn spectrum_bar_heights_all_max_bands_hit_max_h() {
        let heights = spectrum_bar_heights(&[1.0; audio::BANDS], 8, 50.0);
        assert_eq!(heights.len(), 8);
        assert!(heights.iter().all(|&h| (h - 50.0).abs() < 1e-4));
    }

    #[test]
    fn spectrum_src_mirror_is_symmetric_for_any_band_count() {
        for n in 1..=64usize {
            for i in 0..n {
                let s = spectrum_src(i, n, true, false);
                assert!(s < n, "src out of range for n={n}, i={i}");
                assert_eq!(
                    s,
                    spectrum_src(n - 1 - i, n, true, false),
                    "mirror not symmetric at n={n}, i={i}"
                );
            }
            // Centre bar(s) render the lowest band.
            assert_eq!(spectrum_src((n - 1) / 2, n, true, false), 0);
        }
    }

    proptest! {
        #[test]
        fn widget_rect_center_stays_in_panel(
            x in -1.0f32..=2.0,
            y in -1.0f32..=2.0,
            scale in 0.0f32..=10.0,
            panel in 32u32..=640,
        ) {
            let (cx, cy, size) = widget_rect(x, y, scale, panel, panel);
            prop_assert!((0.0..=panel as f32).contains(&cx));
            prop_assert!((0.0..=panel as f32).contains(&cy));
            prop_assert!(size <= panel as f32 + 1e-3);
        }

        #[test]
        fn spectrum_bar_heights_stay_in_bounds_and_monotone(
            bands_vec in prop::collection::vec(0.0f32..=1.0, 64),
            deltas in prop::collection::vec(0.0f32..=1.0, 64),
            n in 1usize..=64,
            max_h in 1.0f32..=500.0,
        ) {
            let mut bands = [0.0f32; 64];
            bands.copy_from_slice(&bands_vec);

            let heights = spectrum_bar_heights(&bands, n, max_h);
            prop_assert_eq!(heights.len(), n);
            for &h in &heights {
                prop_assert!((0.0..=max_h + 1e-3).contains(&h));
            }

            // Monotone in input: raising every band cannot lower any output bar.
            let mut bumped = [0.0f32; 64];
            for i in 0..64 {
                bumped[i] = (bands[i] + deltas[i]).clamp(0.0, 1.0);
            }
            let heights_up = spectrum_bar_heights(&bumped, n, max_h);
            for (h, hu) in heights.iter().zip(heights_up.iter()) {
                prop_assert!(*hu >= *h - 1e-4);
            }
        }
    }

    #[test]
    fn marquee_offset_is_zero_when_text_fits() {
        // Fits (text_w <= area_w) → never scrolls, regardless of t.
        for t in [0.0, 1.0, 5.0, 100.0] {
            assert_eq!(marquee_offset_px(30.0, 50.0, 8.0, 20.0, t), 0.0);
        }
    }

    #[test]
    fn marquee_offset_holds_during_head_pause_then_advances() {
        // Overflows: 0 through the 2s head-pause, then grows.
        assert_eq!(marquee_offset_px(100.0, 40.0, 8.0, 20.0, 0.0), 0.0);
        assert_eq!(marquee_offset_px(100.0, 40.0, 8.0, 20.0, 1.9), 0.0);
        let after = marquee_offset_px(100.0, 40.0, 8.0, 20.0, 3.0);
        assert!(after > 0.0, "should advance after head pause: {after}");
    }

    proptest! {
        #[test]
        fn marquee_offset_stays_within_one_cycle(
            text_w in 0.1f32..500.0,
            area_w in 0.1f32..500.0,
            sep_w in 0.0f32..50.0,
            pps in 0.1f32..200.0,
            t in 0.0f64..1000.0,
        ) {
            let off = marquee_offset_px(text_w, area_w, sep_w, pps, t);
            prop_assert!(off >= 0.0);
            if text_w <= area_w {
                prop_assert_eq!(off, 0.0);
            } else {
                prop_assert!(off < text_w + sep_w);
            }
        }
    }

    #[test]
    fn now_playing_layout_no_player_renders_placeholder() {
        let layout = now_playing_layout(None, true, true, true);
        assert_eq!(layout.title.as_deref(), Some("-"));
        assert!(layout.artist.is_none());
        assert!(layout.art.is_none());
    }

    #[test]
    fn now_playing_layout_hides_title_when_disabled() {
        let info = MediaInfo {
            title: "Track".to_string(),
            artist: "Artist".to_string(),
            status: crate::services::media::PlaybackStatus::Playing,
            art: None,
        };
        // show_art only: no title, no artist.
        let layout = now_playing_layout(Some(&info), true, false, false);
        assert!(layout.title.is_none());
        assert!(layout.artist.is_none());
    }

    #[test]
    fn now_playing_layout_with_media_info_shows_full_title_and_artist() {
        let info = MediaInfo {
            title: "A Very Long Track Title That Would Scroll".to_string(),
            artist: "Artist".to_string(),
            status: crate::services::media::PlaybackStatus::Playing,
            art: None,
        };
        let layout = now_playing_layout(Some(&info), true, true, true);
        // The full title is kept — scrolling/clipping happens at draw time.
        assert_eq!(layout.title.as_deref(), Some(info.title.as_str()));
        assert_eq!(layout.artist.as_deref(), Some("Artist"));
    }

    #[test]
    fn now_playing_layout_hides_artist_when_disabled() {
        let info = MediaInfo {
            title: "Track".to_string(),
            artist: "Artist".to_string(),
            status: crate::services::media::PlaybackStatus::Playing,
            art: None,
        };
        let layout = now_playing_layout(Some(&info), true, true, false);
        assert!(layout.title.is_some());
        assert!(layout.artist.is_none());
    }

    #[tokio::test]
    async fn now_playing_smoke_renders_with_no_media_handle() {
        let w = widget(WidgetType::NowPlaying);
        let def = def_with(vec![w], BgKind::Flow);
        let params = params_for(&def);
        let mut tmpl = CustomTemplate::from_params(&params, Path::new("/tmp"));
        let sensors = HashMap::new();
        let mut buf = RgbaImage::new(240, 240);
        tmpl.render(&ctx(&sensors, 240, 240), &mut buf).unwrap();
        assert_eq!(buf.dimensions(), (240, 240));
    }

    fn tctx<'a>(
        w: u32,
        h: u32,
        t: f64,
        frame: u64,
        sensors: &'a HashMap<String, Sensor>,
    ) -> TemplateCtx<'a> {
        TemplateCtx {
            width: w,
            height: h,
            t,
            frame,
            sensors,
        }
    }

    fn sensor_at(id: &str, value: f64) -> Sensor {
        Sensor {
            id: id.to_string(),
            name: id.to_string(),
            value,
            unit: SensorUnit::Celsius,
            sensor_type: SensorType::default(),
            visibility: Default::default(),
        }
    }

    fn sensor_widget() -> WidgetDef {
        let mut w = widget(WidgetType::Sensor);
        w.params.insert(
            "sensor".to_string(),
            EffectParamValue::Str("cpu".to_string()),
        );
        w
    }

    #[test]
    fn content_signature_stable_for_static_solid_and_text_across_time() {
        let def = def_with(vec![widget(WidgetType::Text)], BgKind::Solid);
        let tmpl = CustomTemplate::from_params(&params_for(&def), Path::new("/tmp"));
        let sensors = HashMap::new();
        let sig1 = tmpl.content_signature(&tctx(240, 240, 0.0, 0, &sensors));
        let sig2 = tmpl.content_signature(&tctx(240, 240, 999.0, 5, &sensors));
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn content_signature_changes_with_sensor_value() {
        let def = def_with(vec![sensor_widget()], BgKind::Solid);
        let tmpl = CustomTemplate::from_params(&params_for(&def), Path::new("/tmp"));
        let mut sensors1 = HashMap::new();
        sensors1.insert("cpu".to_string(), sensor_at("cpu", 50.0));
        let mut sensors2 = HashMap::new();
        sensors2.insert("cpu".to_string(), sensor_at("cpu", 80.0));
        let sig1 = tmpl.content_signature(&tctx(240, 240, 0.0, 0, &sensors1));
        let sig2 = tmpl.content_signature(&tctx(240, 240, 0.0, 0, &sensors2));
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn content_signature_changes_with_debug_frame() {
        let def = def_with(vec![widget(WidgetType::Debug)], BgKind::Solid);
        let tmpl = CustomTemplate::from_params(&params_for(&def), Path::new("/tmp"));
        let sensors = HashMap::new();
        let sig1 = tmpl.content_signature(&tctx(240, 240, 0.0, 1, &sensors));
        let sig2 = tmpl.content_signature(&tctx(240, 240, 0.0, 2, &sensors));
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn content_signature_changes_with_clock_variant() {
        let mut w1 = widget(WidgetType::Clock);
        w1.params.insert(
            "variant".to_string(),
            EffectParamValue::Str("24h".to_string()),
        );
        let mut w2 = widget(WidgetType::Clock);
        w2.params.insert(
            "variant".to_string(),
            EffectParamValue::Str("24h_seconds".to_string()),
        );
        let sensors = HashMap::new();
        let tmpl1 = CustomTemplate::from_params(
            &params_for(&def_with(vec![w1], BgKind::Solid)),
            Path::new("/tmp"),
        );
        let tmpl2 = CustomTemplate::from_params(
            &params_for(&def_with(vec![w2], BgKind::Solid)),
            Path::new("/tmp"),
        );
        let ctx = tctx(240, 240, 0.0, 0, &sensors);
        assert_ne!(tmpl1.content_signature(&ctx), tmpl2.content_signature(&ctx));
    }

    #[test]
    fn content_signature_changes_with_date_variant() {
        let mut w1 = widget(WidgetType::Date);
        w1.params.insert(
            "variant".to_string(),
            EffectParamValue::Str("short".to_string()),
        );
        let mut w2 = widget(WidgetType::Date);
        w2.params.insert(
            "variant".to_string(),
            EffectParamValue::Str("numeric".to_string()),
        );
        let sensors = HashMap::new();
        let tmpl1 = CustomTemplate::from_params(
            &params_for(&def_with(vec![w1], BgKind::Solid)),
            Path::new("/tmp"),
        );
        let tmpl2 = CustomTemplate::from_params(
            &params_for(&def_with(vec![w2], BgKind::Solid)),
            Path::new("/tmp"),
        );
        let ctx = tctx(240, 240, 0.0, 0, &sensors);
        assert_ne!(tmpl1.content_signature(&ctx), tmpl2.content_signature(&ctx));
    }

    #[test]
    fn content_signature_changes_with_flow_background_time() {
        let def = def_with(vec![], BgKind::Flow);
        let tmpl = CustomTemplate::from_params(&params_for(&def), Path::new("/tmp"));
        let sensors = HashMap::new();
        let sig1 = tmpl.content_signature(&tctx(240, 240, 0.0, 0, &sensors));
        let sig2 = tmpl.content_signature(&tctx(240, 240, 5.0, 0, &sensors));
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn content_signature_stable_across_time_for_procedural_static_backgrounds() {
        for background in [BgKind::Solid, BgKind::Grid, BgKind::Glow] {
            let def = def_with(vec![], background);
            let tmpl = CustomTemplate::from_params(&params_for(&def), Path::new("/tmp"));
            let sensors = HashMap::new();
            let sig1 = tmpl.content_signature(&tctx(240, 240, 0.0, 0, &sensors));
            let sig2 = tmpl.content_signature(&tctx(240, 240, 500.0, 0, &sensors));
            assert_eq!(sig1, sig2);
        }
    }

    #[test]
    fn static_bg_cache_returns_pixel_identical_images_across_calls() {
        let def = def_with(vec![], BgKind::Glow);
        let mut tmpl = CustomTemplate::from_params(&params_for(&def), Path::new("/tmp"));
        let sensors = HashMap::new();
        let mut buf = RgbaImage::new(240, 240);
        tmpl.render(&tctx(240, 240, 0.0, 0, &sensors), &mut buf)
            .unwrap();
        let mut buf2 = RgbaImage::new(240, 240);
        tmpl.render(&tctx(240, 240, 500.0, 3, &sensors), &mut buf2)
            .unwrap();
        assert_eq!(buf, buf2);
    }

    #[test]
    fn static_bg_cache_repaints_on_dimension_change() {
        let def = def_with(vec![], BgKind::Grid);
        let mut tmpl = CustomTemplate::from_params(&params_for(&def), Path::new("/tmp"));
        let sensors = HashMap::new();
        let mut buf1 = RgbaImage::new(240, 240);
        tmpl.render(&tctx(240, 240, 0.0, 0, &sensors), &mut buf1)
            .unwrap();
        let mut buf2 = RgbaImage::new(120, 120);
        tmpl.render(&tctx(120, 120, 0.0, 0, &sensors), &mut buf2)
            .unwrap();
        assert_eq!(buf1.dimensions(), (240, 240));
        assert_eq!(buf2.dimensions(), (120, 120));
    }

    fn arb_widget_def() -> impl Strategy<Value = WidgetDef> {
        (
            "[a-z0-9]{1,8}",
            prop_oneof![
                Just(WidgetType::Clock),
                Just(WidgetType::Text),
                Just(WidgetType::Sensor),
            ],
            -1.0f32..2.0,
            -1.0f32..2.0,
            0.1f32..3.0,
            0.0f32..360.0,
        )
            .prop_map(|(id, widget_type, x, y, scale, rotation)| WidgetDef {
                id,
                widget_type,
                x,
                y,
                scale,
                rotation,
                color: None,
                font: None,
                params: HashMap::new(),
            })
    }

    proptest! {
        #[test]
        fn editor_sprite_signature_invariant_under_position_and_rotation(
            w in arb_widget_def(),
            x2 in -1.0f32..2.0,
            y2 in -1.0f32..2.0,
            rot2 in 0.0f32..360.0,
        ) {
            let mut moved = w.clone();
            moved.x = x2;
            moved.y = y2;
            moved.rotation = rot2;
            prop_assert_eq!(
                editor_sprite_signature(&w, 240, 240, None),
                editor_sprite_signature(&moved, 240, 240, None)
            );
        }

        #[test]
        fn editor_sprite_signature_changes_with_scale(w in arb_widget_def(), scale2 in 0.1f32..3.0) {
            prop_assume!((w.scale - scale2).abs() > 1e-6);
            let mut rescaled = w.clone();
            rescaled.scale = scale2;
            prop_assert_ne!(
                editor_sprite_signature(&w, 240, 240, None),
                editor_sprite_signature(&rescaled, 240, 240, None)
            );
        }
    }

    #[test]
    fn editor_sprite_signature_changes_with_param() {
        let mut w1 = widget(WidgetType::Text);
        w1.params
            .insert("text".to_string(), EffectParamValue::Str("a".to_string()));
        let mut w2 = w1.clone();
        w2.params
            .insert("text".to_string(), EffectParamValue::Str("b".to_string()));
        assert_ne!(
            editor_sprite_signature(&w1, 240, 240, None),
            editor_sprite_signature(&w2, 240, 240, None)
        );
    }

    #[test]
    fn editor_sprite_signature_changes_with_color_font_and_canvas() {
        let base = widget(WidgetType::Text);
        let mut colored = base.clone();
        colored.color = Some(RgbColor { r: 1, g: 2, b: 3 });
        let mut fonted = base.clone();
        fonted.font = Some(FontKind::Mono);
        assert_ne!(
            editor_sprite_signature(&base, 240, 240, None),
            editor_sprite_signature(&colored, 240, 240, None)
        );
        assert_ne!(
            editor_sprite_signature(&base, 240, 240, None),
            editor_sprite_signature(&fonted, 240, 240, None)
        );
        assert_ne!(
            editor_sprite_signature(&base, 240, 240, None),
            editor_sprite_signature(&base, 120, 120, None)
        );
    }

    #[test]
    fn editor_sprite_signature_is_stable_across_json_roundtrip() {
        let mut w = widget(WidgetType::Sensor);
        w.params.insert(
            "sensor".to_string(),
            EffectParamValue::Str("cpu".to_string()),
        );
        w.params.insert(
            "label".to_string(),
            EffectParamValue::Str("CPU".to_string()),
        );
        let roundtripped: WidgetDef =
            serde_json::from_str(&serde_json::to_string(&w).unwrap()).unwrap();
        assert_eq!(
            editor_sprite_signature(&w, 240, 240, None),
            editor_sprite_signature(&roundtripped, 240, 240, None)
        );
    }

    /// Diagnostic: simulates editor churn to measure memory retention (ignored).
    #[test]
    #[ignore]
    #[cfg(target_os = "linux")]
    fn editor_churn_rss_diagnostic() {
        fn rss_mb() -> f64 {
            let s = std::fs::read_to_string("/proc/self/status").unwrap();
            let kb: f64 = s
                .lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap()
                .parse()
                .unwrap();
            kb / 1024.0
        }
        fn heap_mb() -> (f64, f64) {
            let mi = unsafe { libc::mallinfo2() };
            (
                (mi.uordblks as f64) / 1e6, // in-use bytes
                (mi.fordblks as f64) / 1e6, // free (retained) bytes
            )
        }

        let (Ok(dir), Ok(file)) = (
            std::env::var("HALOD_CHURN_DIR"),
            std::env::var("HALOD_CHURN_FILE"),
        ) else {
            eprintln!("HALOD_CHURN_DIR / HALOD_CHURN_FILE not set; skipping");
            return;
        };
        let images_dir = std::path::PathBuf::from(shellexpand(&dir));

        let noimg = std::env::var("HALOD_CHURN_MODE").as_deref() == Ok("noimg");
        let sensors = HashMap::new();
        let mut session = None;

        eprintln!("baseline: rss={:.1}MB", rss_mb());
        let iters = if noimg { 100 } else { 8 };
        for i in 0..iters {
            // Vary the scale each call, like a resize-drag tick in the GUI.
            let widgets = if noimg {
                let mut ws = vec![
                    widget(WidgetType::Clock),
                    widget(WidgetType::Sensor),
                    widget(WidgetType::Text),
                ];
                for (j, w) in ws.iter_mut().enumerate() {
                    w.id = format!("w{j}");
                    w.x = 0.2 + j as f32 * 0.3;
                    w.scale = 1.0 + (i % 10) as f32 * 0.02;
                }
                ws[2].params.insert(
                    "text".to_string(),
                    EffectParamValue::Str("hello".to_string()),
                );
                ws
            } else {
                let mut w = widget(WidgetType::Image);
                w.params
                    .insert("filename".to_string(), EffectParamValue::Str(file.clone()));
                w.scale = 1.0 + i as f32 * 0.05;
                vec![w]
            };
            let def = CustomTemplateDef {
                widgets,
                style: ScreenStyle {
                    background: BgKind::Solid,
                    ..Default::default()
                },
            };
            let t0 = std::time::Instant::now();
            let _ = render_editor_sprites(
                "dev",
                &def,
                480,
                480,
                &sensors,
                &images_dir,
                &HashMap::new(),
                &mut session,
            );
            let (used, free) = heap_mb();
            let keys: Vec<String> = session
                .as_ref()
                .unwrap()
                .tmpl
                .image_cache
                .borrow()
                .keys()
                .map(|(k, w)| format!("{}x{w}", k.replace('\u{1}', "|")))
                .collect();
            if !noimg || i % 10 == 0 {
                eprintln!(
                    "iter {i}: {:>6.0}ms rss={:.1}MB heap_used={:.1}MB heap_free={:.1}MB cache={keys:?}",
                    t0.elapsed().as_millis(),
                    rss_mb(),
                    used,
                    free
                );
            }
        }

        drop(session);
        let (used, free) = heap_mb();
        eprintln!(
            "after drop(session): rss={:.1}MB heap_used={:.1}MB heap_free={:.1}MB",
            rss_mb(),
            used,
            free
        );
        unsafe { libc::malloc_trim(0) };
        let (used, free) = heap_mb();
        eprintln!(
            "after malloc_trim:   rss={:.1}MB heap_used={:.1}MB heap_free={:.1}MB",
            rss_mb(),
            used,
            free
        );
    }

    #[cfg(target_os = "linux")]
    fn shellexpand(p: &str) -> String {
        match p.strip_prefix("~/") {
            Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap()),
            None => p.to_string(),
        }
    }
}
