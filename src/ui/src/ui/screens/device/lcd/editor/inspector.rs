// SPDX-License-Identifier: GPL-3.0-or-later
//! Inspector column: the selected-widget param card and the screen-style card
//! (accent, background, brightness, raw streaming).

use egui::Rect;
use halod_shared::commands::DaemonCommand;
use halod_shared::lcd_custom::{
    param_bool, param_variant, widget_schema, BgKind, FontKind, WidgetDef, WidgetType,
};
use halod_shared::lcd_geometry::MAX_SCALE;
use halod_shared::types::LcdStatus;

use super::super::library::image_picker;
use super::super::params::render_param;
use super::super::preview::raw_streaming_row;
use super::{send_def, DeviceUi, TabCtx};
use crate::ui::components as widgets;
use crate::ui::theme;

/// Shown in the inspector column instead of [`selected_widget_card`] when no
/// widget is selected on the stage.
pub(super) fn empty_selection_card(ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(14.0);
        ui.label(
            egui::RichText::new(t!("lcd.empty_selection_hint"))
                .font(theme::body(11.5))
                .color(theme::TEXT_FAINT),
        );
        ui.add_space(14.0);
    });
}

/// "SELECTED · {type}" caption matching the design's per-variant labels.
fn selected_type_label(w: &WidgetDef) -> std::borrow::Cow<'static, str> {
    match w.widget_type {
        WidgetType::Clock => t!("lcd.type_clock"),
        WidgetType::Date => t!("lcd.type_date"),
        WidgetType::Sensor if param_variant(w, "stat") == "ring" => t!("lcd.type_ring_gauge"),
        WidgetType::Sensor if param_variant(w, "stat") == "bar" => t!("lcd.type_progress_bar"),
        WidgetType::Sensor => t!("lcd.type_sensor_stat"),
        WidgetType::Text => t!("lcd.type_text"),
        WidgetType::Image => t!("lcd.type_image"),
        WidgetType::Logo => t!("lcd.type_logo"),
        WidgetType::Debug => t!("lcd.type_debug"),
        WidgetType::AudioSpectrum => t!("lcd.type_spectrum"),
        WidgetType::AudioLevel if param_variant(w, "ring") == "bar" => t!("lcd.type_vu_bar"),
        WidgetType::AudioLevel => t!("lcd.type_vu_ring"),
        WidgetType::NowPlaying => t!("lcd.type_now_playing"),
        WidgetType::Shape => t!("lcd.type_shape"),
        WidgetType::Unknown => t!("lcd.type_widget"),
    }
}

/// Widget types whose only color is the widget-level `color` field (text
/// glyphs). Others carry their own color params in [`widget_schema`].
fn uses_widget_color(t: WidgetType) -> bool {
    matches!(
        t,
        WidgetType::Text | WidgetType::Clock | WidgetType::Date | WidgetType::Debug
    )
}

/// Widget types that draw text through the widget-level `font` field, so the
/// inspector offers a per-widget font override (falling back to the screen font).
fn uses_widget_font(t: WidgetType) -> bool {
    matches!(
        t,
        WidgetType::Text
            | WidgetType::Clock
            | WidgetType::Date
            | WidgetType::Debug
            | WidgetType::Sensor
            | WidgetType::NowPlaying
    )
}

/// Font-picker pill row over every bundled typeface (by real name), resolving
/// `None` to the screen default. Returns the chosen font when the user switches.
fn font_picker(
    ui: &mut egui::Ui,
    current: Option<FontKind>,
    default: FontKind,
) -> Option<FontKind> {
    let effective = current.unwrap_or(default);
    let mut chosen = None;
    ui.label(
        egui::RichText::new(t!("lcd.font"))
            .font(theme::body(11.0))
            .color(theme::TEXT_MUT),
    );
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(7.0, 7.0);
        for kind in FontKind::ALL {
            if widgets::pill(ui, kind.label(), effective == kind) && effective != kind {
                chosen = Some(kind);
            }
        }
    });
    chosen
}

pub(super) fn selected_widget_card(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    sel: &str,
) {
    let Some(idx) = st.lcd.editor.def.widgets.iter().position(|w| w.id == sel) else {
        return;
    };
    let card_rect =
        Rect::from_min_size(ui.cursor().min, egui::Vec2::new(ui.available_width(), 28.0));
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::LcdEditorVariant,
        card_rect,
    );
    ui.label(
        egui::RichText::new(t!(
            "lcd.selected_caption",
            kind = selected_type_label(&st.lcd.editor.def.widgets[idx])
        ))
        .font(theme::body(10.0))
        .color(theme::TEXT_FAINT2),
    );
    ui.add_space(14.0);

    let widget_type = st.lcd.editor.def.widgets[idx].widget_type;
    let schema = widget_schema(widget_type);

    let mut changed = false;
    {
        let widget = &mut st.lcd.editor.def.widgets[idx];
        let variant = param_variant(widget, "stat");
        let bar_variant = variant == "bar";
        let gauge_variant = variant == "ring" || variant == "bar";
        let is_vu_bar =
            widget_type == WidgetType::AudioLevel && param_variant(widget, "ring") == "bar";
        let is_vu_gauge =
            widget_type == WidgetType::AudioLevel && param_variant(widget, "ring") != "bar";
        let spectrum_gradient =
            widget_type == WidgetType::AudioSpectrum && param_bool(widget, "gradient", false);

        let mut last_group: Option<&str> = None;
        for p in &schema {
            // Conditional visibility:
            let show = match (widget_type, p.id.as_str()) {
                // Sensor: gauge/bar-only params
                (
                    WidgetType::Sensor,
                    "fill" | "track" | "gradient" | "gradient_high" | "min" | "max",
                ) => gauge_variant,
                // Sensor: bar-only params
                (WidgetType::Sensor, "rounded" | "inverted" | "curve") => bar_variant,
                // AudioLevel: gauge/bar params (both need fill/track)
                (WidgetType::AudioLevel, "fill" | "track" | "gradient" | "gradient_high") => {
                    is_vu_gauge || is_vu_bar
                }
                // AudioLevel: bar-only params
                (WidgetType::AudioLevel, "rounded" | "inverted" | "curve") => is_vu_bar,
                // AudioSpectrum: gradient_high only when gradient is on
                (WidgetType::AudioSpectrum, "gradient_high") => spectrum_gradient,
                _ => true,
            };
            if show {
                // Group headings (hardcoded per param id for now).
                let group: Option<&str> = match (widget_type, p.id.as_str()) {
                    (
                        WidgetType::Sensor | WidgetType::AudioLevel,
                        "rounded" | "inverted" | "curve",
                    ) => Some("lcd.group_shape"),
                    (
                        WidgetType::Sensor | WidgetType::AudioLevel,
                        "fill" | "track" | "gradient" | "gradient_high",
                    ) => Some("lcd.group_colors"),
                    (WidgetType::Sensor, "min" | "max") => Some("lcd.group_range"),
                    _ => None,
                };
                if group != last_group {
                    if let Some(g) = group {
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(t!(g))
                                .font(theme::semibold(10.5))
                                .color(theme::TEXT_FAINT2),
                        );
                        ui.add_space(2.0);
                    }
                    last_group = group;
                }
                changed |= render_param(ui, ctx, widget_type, p, &mut widget.params);
                ui.add_space(4.0);
            }
        }
    }

    // Text-drawn widgets carry their single color in the widget-level `color`
    // field (glyphs), so expose a picker here; typed widgets with their own
    // color params (sensor/shape/audio) manage color through those instead.
    if uses_widget_color(widget_type) {
        let cur = st.lcd.editor.def.widgets[idx]
            .color
            .unwrap_or(st.lcd.editor.def.style.accent);
        ui.label(
            egui::RichText::new(t!("lcd.color"))
                .font(theme::body(11.0))
                .color(theme::TEXT_MUT),
        );
        ui.add_space(6.0);
        if let Some(c) = widgets::color_swatch_row(ui, cur) {
            st.lcd.editor.def.widgets[idx].color = Some(c);
            changed = true;
        }
        ui.add_space(6.0);
    }

    // Per-widget font override for every text-drawing widget.
    if uses_widget_font(widget_type) {
        let default_font = st.lcd.editor.def.style.font;
        let cur = st.lcd.editor.def.widgets[idx].font;
        if let Some(f) = font_picker(ui, cur, default_font) {
            st.lcd.editor.def.widgets[idx].font = Some(f);
            changed = true;
        }
        ui.add_space(6.0);
    }

    // The `filename` param is a `ParamKind::Image` no-op above — the image is
    // chosen here from the shared library picker (with file browse) instead.
    if widget_type == WidgetType::Image {
        let current =
            halod_shared::lcd_custom::param_str(&st.lcd.editor.def.widgets[idx], "filename");
        if let Some(pick) = image_picker(ui, ctx, st, id, &current) {
            st.lcd.editor.def.widgets[idx].params.insert(
                "filename".to_string(),
                halod_shared::types::EffectParamValue::Str(pick),
            );
            changed = true;
        }
        ui.add_space(6.0);
    }

    // Size.
    let mut scale = st.lcd.editor.def.widgets[idx].scale;
    let readout = format!("{}%", (scale * 100.0).round() as i32);
    if widgets::slider_row(ui, &t!("lcd.size"), &mut scale, 0.6..=MAX_SCALE, &readout) {
        st.lcd.editor.def.widgets[idx].scale = scale;
        changed = true;
    }
    ui.add_space(6.0);

    if changed {
        send_def(ctx, st, id, false);
    }

    ui.horizontal(|ui| {
        if widgets::button(
            ui,
            &t!("lcd.center"),
            widgets::ButtonKind::Ghost,
            egui::vec2(118.0, 28.0),
        )
        .clicked()
        {
            st.lcd.editor.def.widgets[idx].x = 0.5;
            st.lcd.editor.def.widgets[idx].y = 0.5;
            send_def(ctx, st, id, false);
        }

        if widgets::button(
            ui,
            &t!("lcd.delete"),
            widgets::ButtonKind::Danger,
            egui::vec2(118.0, 28.0),
        )
        .clicked()
        {
            st.lcd.editor.def.widgets.remove(idx);
            st.lcd.editor.clear_selection();
            send_def(ctx, st, id, true);
        }
    });
}

// ── Screen style ───────────────────────────────────────────────────────────────

/// Background-kind keys — the match in `bg_kind_key`/[`apply_preset`] switches
/// on them. Display text comes from `bg_label()`, keyed off the same string.
const BG_KINDS: &[&str] = &["flow", "solid", "grid", "glow", "image"];

/// Translated display label for a background-kind key (the key stays English —
/// the match below switches on it).
fn bg_label(key: &str) -> std::borrow::Cow<'static, str> {
    match key {
        "flow" => t!("lcd.bg_flow"),
        "solid" => t!("lcd.bg_solid"),
        "grid" => t!("lcd.bg_grid"),
        "glow" => t!("lcd.bg_glow"),
        "image" => t!("lcd.bg_image"),
        _ => std::borrow::Cow::Borrowed(""),
    }
}

fn bg_kind_key(bg: &BgKind) -> &'static str {
    match bg {
        BgKind::Flow => "flow",
        BgKind::Solid => "solid",
        BgKind::Grid => "grid",
        BgKind::Glow => "glow",
        BgKind::Image { .. } => "image",
    }
}

pub(super) fn screen_style_card(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    lcd: &LcdStatus,
) {
    ui.label(
        egui::RichText::new(t!("lcd.screen_style"))
            .font(theme::semibold(13.0))
            .color(theme::TEXT),
    );
    ui.add_space(14.0);

    let mut changed = false;

    ui.label(
        egui::RichText::new(t!("lcd.accent"))
            .font(theme::body(11.0))
            .color(theme::TEXT_MUT),
    );
    ui.add_space(6.0);
    if let Some(c) = widgets::color_swatch_row(ui, st.lcd.editor.def.style.accent) {
        st.lcd.editor.def.style.accent = c;
        changed = true;
    }
    ui.add_space(12.0);

    ui.label(
        egui::RichText::new(t!("lcd.background"))
            .font(theme::body(11.0))
            .color(theme::TEXT_MUT),
    );
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(7.0, 7.0);
        let current = bg_kind_key(&st.lcd.editor.def.style.background);
        for &key in BG_KINDS {
            if widgets::pill(ui, &bg_label(key), current == key) && current != key {
                st.lcd.editor.def.style.background = match key {
                    "solid" => BgKind::Solid,
                    "grid" => BgKind::Grid,
                    "glow" => BgKind::Glow,
                    "image" => BgKind::Image {
                        filename: String::new(),
                        dim: 0.0,
                    },
                    _ => BgKind::Flow,
                };
                changed = true;
            }
        }
    });

    if let BgKind::Image { filename, dim } = st.lcd.editor.def.style.background.clone() {
        ui.add_space(8.0);
        background_image_picker(ui, ctx, st, id, &filename, dim, &mut changed);
    }

    if changed {
        send_def(ctx, st, id, false);
    }

    // Screen brightness — same underlying command/scratch key as the Display
    // card's slider (the design duplicates it here too; both edit one value).
    ui.add_space(14.0);
    let key = "lcd_bright";
    let mut b = st.guarded(key, lcd.brightness as f32, ctx.time);
    let readout = format!("{}%", b.round() as i32);
    if widgets::slider_row(ui, &t!("lcd.brightness"), &mut b, 0.0..=100.0, &readout) {
        st.set(key, b, ctx.time);
        st.queue(
            key,
            DaemonCommand::SetScreenBrightness {
                id: id.to_string(),
                brightness: b.round() as u8,
            },
            ctx.time,
        );
    }

    ui.add_space(14.0);
    raw_streaming_row(ui, ctx, id, lcd);
}

fn background_image_picker(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    filename: &str,
    dim: f64,
    changed: &mut bool,
) {
    if let Some(pick) = image_picker(ui, ctx, st, id, filename) {
        st.lcd.editor.def.style.background = BgKind::Image {
            filename: pick,
            dim,
        };
        *changed = true;
    }
    ui.add_space(6.0);
    let mut dim_pct = dim as f32;
    let readout = format!("{}%", dim_pct.round() as i32);
    if widgets::slider_row(ui, &t!("lcd.dim"), &mut dim_pct, 0.0..=100.0, &readout) {
        st.lcd.editor.def.style.background = BgKind::Image {
            filename: filename.to_string(),
            dim: dim_pct as f64,
        };
        *changed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::EffectParamValue;
    use std::collections::HashMap;

    fn widget(id: &str, x: f32, y: f32) -> WidgetDef {
        WidgetDef {
            id: id.to_string(),
            widget_type: WidgetType::Text,
            x,
            y,
            scale: 1.0,
            rotation: 0.0,
            color: None,
            font: None,
            params: HashMap::new(),
        }
    }

    fn sensor_widget(variant: &str) -> WidgetDef {
        let mut w = widget("w1", 0.5, 0.5);
        w.widget_type = WidgetType::Sensor;
        w.params.insert(
            "variant".to_string(),
            EffectParamValue::Str(variant.to_string()),
        );
        w
    }

    #[test]
    fn selected_type_label_distinguishes_sensor_and_text_variants() {
        assert_eq!(selected_type_label(&sensor_widget("stat")), "Sensor stat");
        assert_eq!(selected_type_label(&sensor_widget("ring")), "Ring gauge");
        assert_eq!(selected_type_label(&sensor_widget("bar")), "Progress bar");

        let text = widget("w1", 0.0, 0.0);
        assert_eq!(selected_type_label(&text), "Text");
    }

    #[test]
    fn selected_type_label_covers_logo() {
        let mut w = widget("w1", 0.0, 0.0);
        w.widget_type = WidgetType::Logo;
        assert_eq!(selected_type_label(&w), "Logo");
    }

    #[test]
    fn selected_type_label_distinguishes_audio_level_variants() {
        let mut level = widget("w1", 0.0, 0.0);
        level.widget_type = WidgetType::AudioLevel;
        assert_eq!(selected_type_label(&level), "VU ring");
        level.params.insert(
            "variant".to_string(),
            EffectParamValue::Str("bar".to_string()),
        );
        assert_eq!(selected_type_label(&level), "VU bar");

        let mut spectrum = widget("w1", 0.0, 0.0);
        spectrum.widget_type = WidgetType::AudioSpectrum;
        assert_eq!(selected_type_label(&spectrum), "Spectrum");
    }

    #[test]
    fn selected_type_label_covers_now_playing() {
        let mut w = widget("w1", 0.0, 0.0);
        w.widget_type = WidgetType::NowPlaying;
        assert_eq!(selected_type_label(&w), "Now Playing");
    }

    #[test]
    fn bg_kind_key_maps_every_variant() {
        assert_eq!(bg_kind_key(&BgKind::Flow), "flow");
        assert_eq!(bg_kind_key(&BgKind::Solid), "solid");
        assert_eq!(bg_kind_key(&BgKind::Grid), "grid");
        assert_eq!(bg_kind_key(&BgKind::Glow), "glow");
        assert_eq!(
            bg_kind_key(&BgKind::Image {
                filename: "x".into(),
                dim: 0.0
            }),
            "image"
        );
    }
}
