// SPDX-License-Identifier: GPL-3.0-or-later
//! Inspector column: the selected-widget param card and the screen-style card
//! (accent, background, brightness, raw streaming).

use egui::Rect;
use halod_shared::commands::DaemonCommand;
use halod_shared::lcd_custom::{BgKind, FONT_INTER, FONT_MONO, FONT_SANS};
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
        ui.add_space(theme::SPACE_7);
        ui.label(
            egui::RichText::new(t!("lcd.empty_selection_hint"))
                .font(theme::body_sm())
                .color(theme::TEXT_FAINT),
        );
        ui.add_space(theme::SPACE_7);
    });
}

/// "SELECTED · {type}" caption matching the design's per-variant labels.
fn font_picker(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    current: Option<&str>,
    default: &str,
    fonts: &[String],
) -> Option<String> {
    let effective = current.unwrap_or(default);
    widgets::field_label(ui, &t!("lcd.font"));
    let mut options = vec![
        (FONT_SANS.to_owned(), "Noto Sans".to_owned()),
        (FONT_MONO.to_owned(), "JetBrains Mono".to_owned()),
        (FONT_INTER.to_owned(), "Inter Tight".to_owned()),
    ];
    options.extend(
        fonts
            .iter()
            .filter(|font| !matches!(font.as_str(), FONT_SANS | FONT_MONO | FONT_INTER))
            .map(|font| (font.clone(), font.clone())),
    );
    widgets::combo_picker_full(ui, id_salt, &options, effective, None)
}

fn sync_system_fonts(ui: &egui::Ui, st: &mut DeviceUi) {
    let selected: Vec<String> = std::iter::once(st.lcd.editor.def.style.font.clone())
        .chain(
            st.lcd
                .editor
                .def
                .widgets
                .iter()
                .filter_map(|widget| widget.font.clone()),
        )
        .collect();
    if selected
        .iter()
        .any(|family| !st.lcd.editor.attempted_fonts.contains(family))
    {
        let requested: std::collections::HashSet<String> = st
            .lcd
            .editor
            .registered_fonts
            .iter()
            .cloned()
            .chain(selected)
            .collect();
        st.lcd
            .editor
            .attempted_fonts
            .extend(requested.iter().cloned());
        st.lcd.editor.registered_fonts = crate::ui::theme::install_fonts_with_system(
            ui.ctx(),
            requested.iter().map(String::as_str),
        );
    }
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
    ui.label({
        let widget = &st.lcd.editor.def.widgets[idx];
        let plugin_label = Some(widget.widget.clone()).and_then(|catalog_id| {
            ctx.state
                .lcd
                .engine
                .available_widgets
                .iter()
                .find(|descriptor| descriptor.id == catalog_id)
                .map(|descriptor| {
                    descriptor
                        .localized_name(&ctx.state.gui.language)
                        .to_owned()
                })
        });
        egui::RichText::new(t!(
            "lcd.selected_caption",
            kind = plugin_label
                .map(std::borrow::Cow::Owned)
                .unwrap_or_else(|| std::borrow::Cow::Borrowed("Missing widget"))
        ))
        .font(theme::caption())
        .color(theme::TEXT_FAINT2)
    });
    ui.add_space(theme::SPACE_7);

    let plugin_descriptor =
        Some(st.lcd.editor.def.widgets[idx].widget.clone()).and_then(|catalog_id| {
            ctx.state
                .lcd
                .engine
                .available_widgets
                .iter()
                .find(|descriptor| descriptor.id == catalog_id)
        });
    let mut schema = plugin_descriptor
        .map(|descriptor| descriptor.params.clone())
        .unwrap_or_default();
    let min_scale = plugin_descriptor.map_or(0.6, |descriptor| descriptor.min_scale);
    if let Some(descriptor) = plugin_descriptor {
        let params = &st.lcd.editor.def.widgets[idx].params;
        schema.retain(|param| {
            descriptor
                .param_visibility
                .get(&param.id)
                .is_none_or(|rule| {
                    matches!(
                        params.get(&rule.param),
                        Some(halod_shared::types::EffectParamValue::Str(value))
                            if value == &rule.equals
                    )
                })
        });
    }
    if plugin_descriptor.is_some_and(|descriptor| descriptor.uses_font && descriptor.font_controls)
    {
        for style_param in halod_shared::lcd_custom::text_style_params() {
            if !schema.iter().any(|param| param.id == style_param.id) {
                schema.push(style_param);
            }
        }
    }
    schema.push(halod_shared::types::EffectParamDescriptor {
        id: halod_shared::lcd_custom::OPACITY_PARAM.to_owned(),
        label: "Opacity".to_owned(),
        kind: halod_shared::types::ParamKind::Range {
            min: 0.0,
            max: 100.0,
            step: 5.0,
        },
        default: halod_shared::types::EffectParamValue::Float(100.0),
    });

    let mut changed = false;
    {
        let widget = &mut st.lcd.editor.def.widgets[idx];
        for p in &schema {
            changed |= render_param(ui, ctx, p, &mut widget.params);
            ui.add_space(theme::SPACE_2);
        }
    }

    // Text-drawn widgets carry their single color in the widget-level `color`
    // field (glyphs), so expose a picker here; typed widgets with their own
    // color params (sensor/shape/audio) manage color through those instead.
    if plugin_descriptor.is_some_and(|descriptor| descriptor.uses_color) {
        let cur = st.lcd.editor.def.widgets[idx]
            .color
            .unwrap_or(st.lcd.editor.def.style.accent);
        widgets::field_label(ui, &t!("lcd.color"));
        if let Some(c) = widgets::color_swatch_row(
            ui,
            (
                "lcd_widget_color",
                st.lcd.editor.def.widgets[idx].id.as_str(),
            ),
            cur,
        ) {
            st.lcd.editor.def.widgets[idx].color = Some(c);
            changed = true;
        }
        ui.add_space(theme::SPACE_3);
    }

    // Per-widget font override for every text-drawing widget.
    if plugin_descriptor.is_some_and(|descriptor| descriptor.uses_font && descriptor.font_controls)
    {
        let fonts = halod_shared::system_fonts::families();
        let default_font = st.lcd.editor.def.style.font.clone();
        let cur = st.lcd.editor.def.widgets[idx].font.as_deref();
        if let Some(f) = font_picker(ui, ("lcd_widget_font", id, sel), cur, &default_font, fonts) {
            st.lcd.editor.def.widgets[idx].font = Some(f);
            changed = true;
        }
        ui.add_space(theme::SPACE_3);
    }

    // The `filename` param is a `ParamKind::Image` no-op above — the image is
    // chosen here from the shared library picker (with file browse) instead.
    if schema
        .iter()
        .any(|param| matches!(param.kind, halod_shared::types::ParamKind::Image))
    {
        let current =
            halod_shared::lcd_custom::param_str(&st.lcd.editor.def.widgets[idx], "filename");
        if let Some(pick) = image_picker(ui, ctx, st, id, &current) {
            st.lcd.editor.def.widgets[idx].params.insert(
                "filename".to_string(),
                halod_shared::types::EffectParamValue::Str(pick),
            );
            changed = true;
        }
        ui.add_space(theme::SPACE_3);
    }

    // Size.
    let mut scale = st.lcd.editor.def.widgets[idx].scale;
    let readout = format!("{}%", (scale * 100.0).round() as i32);
    if widgets::slider_row(
        ui,
        &t!("lcd.size"),
        &mut scale,
        min_scale..=MAX_SCALE,
        &readout,
    ) {
        st.lcd.editor.def.widgets[idx].scale = scale;
        changed = true;
    }
    ui.add_space(theme::SPACE_3);

    if changed {
        if let Some(descriptor) = plugin_descriptor {
            if let Some(param_id) = descriptor.auto_width_param.as_deref() {
                let widget = &mut st.lcd.editor.def.widgets[idx];
                if let Some(halod_shared::types::EffectParamValue::Str(text)) =
                    widget.params.get(param_id)
                {
                    let width = text.chars().count() as f32
                        * halod_shared::lcd_custom::scale_y(widget)
                        * 0.22;
                    widget.scale = width
                        .max(descriptor.default_scale)
                        .clamp(descriptor.min_scale, MAX_SCALE);
                }
            }
        }
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
            .font(theme::heading())
            .color(theme::TEXT),
    );
    ui.add_space(theme::SPACE_7);

    let mut changed = false;

    widgets::field_label(ui, &t!("lcd.accent"));
    if let Some(c) =
        widgets::color_swatch_row(ui, "lcd_screen_accent", st.lcd.editor.def.style.accent)
    {
        st.lcd.editor.def.style.accent = c;
        changed = true;
    }
    ui.add_space(theme::SPACE_6);

    sync_system_fonts(ui, st);
    let current_font = st.lcd.editor.def.style.font.clone();
    if let Some(font) = font_picker(
        ui,
        "lcd_screen_font",
        Some(&current_font),
        FONT_SANS,
        halod_shared::system_fonts::families(),
    ) {
        st.lcd.editor.def.style.font = font;
        changed = true;
    }
    ui.add_space(theme::SPACE_6);

    ui.label(
        egui::RichText::new(t!("lcd.background"))
            .font(theme::body_sm())
            .color(theme::TEXT_MUT),
    );
    widgets::pill_strip(ui, |ui| {
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
        ui.add_space(theme::SPACE_4);
        background_image_picker(ui, ctx, st, id, &filename, dim, &mut changed);
    }

    if changed {
        send_def(ctx, st, id, false);
    }

    // Screen brightness — same underlying command/scratch key as the Display
    // card's slider (the design duplicates it here too; both edit one value).
    ui.add_space(theme::SPACE_7);
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

    ui.add_space(theme::SPACE_7);
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
    ui.add_space(theme::SPACE_3);
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
