// SPDX-License-Identifier: GPL-3.0-or-later
//! Widget library palette, preset picker, "my templates" list + delete modal,
//! and the pure spawn/preset logic behind them.

use std::collections::HashMap;

use egui::{Rect, Sense, Stroke, Vec2};
use halod_shared::lcd_custom::{widget_schema, WidgetDef, WidgetType};
use halod_shared::types::EffectParamValue;

use super::{send_def, DeviceUi, TabCtx};
use crate::ui::components as widgets;
use crate::ui::theme;

/// Reduce the delete-confirmation modal's outcome: `Some(name)` means the
/// delete was confirmed and should be sent; the pending state is cleared on
/// any outcome.
use widgets::resolve_delete_confirm;

/// Widget library tiles: (type, badge glyph). Display text comes from
/// `library_label()`, keyed off the variant, so it's always translated.
pub(super) const LIBRARY: &[(WidgetType, &str)] = &[
    (WidgetType::Clock, "◷"),
    (WidgetType::Date, "▤"),
    (WidgetType::Sensor, "◔"),
    (WidgetType::Text, "Ab"),
    (WidgetType::Image, "▧"),
    (WidgetType::Logo, "◈"),
    (WidgetType::Debug, "⚙"),
    (WidgetType::AudioSpectrum, "▄█▆"),
    (WidgetType::AudioLevel, "◐"),
    (WidgetType::NowPlaying, "♪"),
    (WidgetType::Shape, "◆"),
];

const PRESETS: &[&str] = &["Stats", "Clock", "Cooler", "Gauge"];

/// Translated display label for a library widget type.
pub(super) fn library_label(t: WidgetType) -> std::borrow::Cow<'static, str> {
    match t {
        WidgetType::Clock => t!("lcd.widget_clock"),
        WidgetType::Date => t!("lcd.widget_date"),
        WidgetType::Sensor => t!("lcd.widget_sensor"),
        WidgetType::Text => t!("lcd.widget_text"),
        WidgetType::Image => t!("lcd.widget_image"),
        WidgetType::Debug => t!("lcd.widget_debug"),
        WidgetType::AudioSpectrum => t!("lcd.widget_spectrum"),
        WidgetType::AudioLevel => t!("lcd.widget_vu_meter"),
        WidgetType::NowPlaying => t!("lcd.widget_now_playing"),
        WidgetType::Logo => t!("lcd.widget_logo"),
        WidgetType::Shape => t!("lcd.widget_shape"),
        WidgetType::Unknown => std::borrow::Cow::Borrowed(""),
    }
}

/// Translated display label for a preset key (the key itself stays English —
/// [`apply_preset`] matches on it).
fn preset_label(key: &str) -> std::borrow::Cow<'static, str> {
    match key {
        "Stats" => t!("lcd.preset_stats"),
        "Clock" => t!("lcd.preset_clock"),
        "Cooler" => t!("lcd.preset_cooler"),
        "Gauge" => t!("lcd.preset_gauge"),
        _ => std::borrow::Cow::Borrowed(""),
    }
}

pub(super) fn library_card(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, id: &str) {
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::LcdEditorPalette,
        ui.max_rect(),
    );
    if library_header(ui, st.lcd.editor.library_collapsed).clicked() {
        st.lcd.editor.library_collapsed = !st.lcd.editor.library_collapsed;
    }
    if st.lcd.editor.library_collapsed {
        return;
    }
    ui.add_space(theme::SPACE_7);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(9.0, 9.0);
        for &(widget_type, badge) in LIBRARY {
            let resp = library_tile(ui, badge, &library_label(widget_type));
            if resp.drag_started() {
                st.lcd.editor.dragging_new = Some(widget_type);
            }
        }
    });
    ui.add_space(theme::SPACE_7);
    ui.label(
        egui::RichText::new(t!("lcd.start_from_preset"))
            .font(theme::micro())
            .color(theme::TEXT_FAINT),
    );
    ui.add_space(theme::SPACE_3);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(7.0, 7.0);
        for &preset in PRESETS {
            if widgets::pill(ui, &preset_label(preset), false) {
                apply_preset(st, preset);
                send_def(ctx, st, id, true);
            }
        }
    });
    ui.add_space(theme::SPACE_7);
    my_templates_section(ui, ctx, st);
}

/// Save-as field plus the list of the user's own saved templates, sitting
/// alongside the built-in presets. Loading/deleting go straight to the
/// daemon; a load round-trips back on `ctx.lcd_template` (drained in `show`).
pub(super) fn my_templates_section(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    ui.label(
        egui::RichText::new(t!("lcd.my_templates"))
            .font(theme::micro())
            .color(theme::TEXT_FAINT),
    );
    ui.add_space(theme::SPACE_3);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut st.lcd.editor.template_name)
                .hint_text(t!("lcd.template_name_hint").to_string())
                .margin(egui::vec2(12.0, 11.0))
                .desired_width(160.0),
        );
        let can_save = !st.lcd.editor.template_name.trim().is_empty();
        if widgets::button(
            ui,
            &t!("lcd.save"),
            widgets::ButtonKind::Ghost,
            egui::vec2(64.0, 35.0),
        )
        .clicked()
            && can_save
        {
            crate::runtime::ipc::send(
                ctx.cmd,
                halod_shared::commands::DaemonCommand::SaveLcdTemplate {
                    name: st.lcd.editor.template_name.trim().to_string(),
                    def: st.lcd.editor.def.clone(),
                },
            );
        }
    });
    if ctx.state.lcd.templates.is_empty() {
        return;
    }
    ui.add_space(theme::SPACE_4);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(7.0, 7.0);
        for name in &ctx.state.lcd.templates {
            let (load, delete) = widgets::chip_closable(ui, name);
            if load {
                crate::runtime::ipc::send(
                    ctx.cmd,
                    halod_shared::commands::DaemonCommand::LoadLcdTemplate { name: name.clone() },
                );
            }
            if delete {
                st.lcd.editor.confirm_delete = Some(name.clone());
            }
        }
    });
}

pub(super) fn delete_template_modal(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    let Some(name) = st.lcd.editor.confirm_delete.clone() else {
        return;
    };
    let (mut confirm, mut cancel) = (false, false);
    let dismissed = widgets::dialog(
        ui.ctx(),
        "lcd_delete_template",
        &t!("lcd.delete_template_title"),
        420.0,
        |ui| {
            ui.label(
                egui::RichText::new(t!("lcd.delete_confirm_body", name = name))
                    .font(theme::body_md())
                    .color(theme::TEXT_MUT),
            );
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("lcd.delete"),
                widgets::ButtonKind::Danger,
                egui::vec2(96.0, 32.0),
            )
            .clicked()
            {
                confirm = true;
            }
            ui.add_space(theme::SPACE_4);
            if widgets::button(
                ui,
                &t!("lcd.cancel"),
                widgets::ButtonKind::Ghost,
                egui::vec2(96.0, 32.0),
            )
            .clicked()
            {
                cancel = true;
            }
        },
    );
    if let Some(name) = resolve_delete_confirm(
        &mut st.lcd.editor.confirm_delete,
        confirm,
        cancel || dismissed,
    ) {
        crate::runtime::ipc::send(
            ctx.cmd,
            halod_shared::commands::DaemonCommand::DeleteLcdTemplate { name },
        );
    }
}

fn library_header(ui: &mut egui::Ui, collapsed: bool) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), Sense::click());
    let cy = rect.center().y;
    let p = ui.painter();
    p.text(
        egui::pos2(rect.left(), cy),
        egui::Align2::LEFT_CENTER,
        t!("lcd.add_a_widget"),
        theme::heading(),
        theme::TEXT,
    );
    let box_rect = Rect::from_min_size(
        egui::pos2(rect.right() - 22.0, cy - 11.0),
        Vec2::splat(22.0),
    );
    p.rect_stroke(
        box_rect,
        6.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Middle,
    );
    p.text(
        box_rect.center(),
        egui::Align2::CENTER_CENTER,
        if collapsed { "▸" } else { "▾" },
        theme::mono(12.0),
        theme::TEXT_MUT,
    );
    p.text(
        egui::pos2(box_rect.left() - 10.0, cy),
        egui::Align2::RIGHT_CENTER,
        t!("lcd.drag_onto_screen"),
        theme::caption(),
        theme::TEXT_FAINT,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// A library tile: an icon badge over a label, matching the design's
/// `lcd.palette` tiles. Press-and-drag onto the stage to spawn a widget there.
fn library_tile(ui: &mut egui::Ui, badge: &str, label: &str) -> egui::Response {
    let size = egui::vec2(80.0, 74.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::drag());
    let p = ui.painter();
    theme::paint_well(p, rect, theme::RADIUS_MD);
    let badge_rect =
        Rect::from_center_size(rect.center() - egui::vec2(0.0, 10.0), Vec2::splat(34.0));
    p.rect_filled(badge_rect, 9.0, theme::a(theme::CYAN, 0.12));
    p.text(
        badge_rect.center(),
        egui::Align2::CENTER_CENTER,
        badge,
        theme::mono(17.0),
        theme::CYAN,
    );
    p.text(
        rect.center() + egui::vec2(0.0, 22.0),
        egui::Align2::CENTER_CENTER,
        label,
        theme::caption(),
        theme::TEXT_MUT,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }
    resp
}

/// Push a new widget of `widget_type` at normalized `(x, y)`, seeded with its
/// schema defaults, and return its id.
pub(super) fn spawn_widget(st: &mut DeviceUi, widget_type: WidgetType, x: f32, y: f32) -> String {
    let new_id = next_widget_id(&st.lcd.editor.def.widgets);
    let mut params = HashMap::new();
    for p in widget_schema(widget_type) {
        params.insert(p.id, p.default);
    }
    st.lcd.editor.def.widgets.push(WidgetDef {
        id: new_id.clone(),
        widget_type,
        x,
        y,
        scale: 1.0,
        rotation: 0.0,
        color: None,
        font: None,
        params,
    });
    new_id
}

/// Preset widget arrangements (sensor left unbound — the user picks it in the
/// inspector), matching the design's `lcdPresets` chips.
pub(super) fn apply_preset(st: &mut DeviceUi, preset: &str) {
    st.lcd.editor.def.widgets.clear();
    st.lcd.editor.clear_selection();
    match preset {
        "Clock" => {
            spawn_widget(st, WidgetType::Clock, 0.5, 0.4);
            spawn_widget(st, WidgetType::Date, 0.5, 0.62);
        }
        "Stats" => {
            spawn_widget(st, WidgetType::Sensor, 0.3, 0.35);
            spawn_widget(st, WidgetType::Sensor, 0.7, 0.35);
            spawn_widget(st, WidgetType::Sensor, 0.5, 0.65);
        }
        "Cooler" => {
            let id = spawn_widget(st, WidgetType::Sensor, 0.5, 0.4);
            if let Some(w) = st.lcd.editor.def.widgets.iter_mut().find(|w| w.id == id) {
                w.params.insert(
                    "label".to_string(),
                    EffectParamValue::Str("Coolant".to_string()),
                );
            }
            spawn_widget(st, WidgetType::Text, 0.5, 0.68);
        }
        "Gauge" => {
            let id = spawn_widget(st, WidgetType::Sensor, 0.5, 0.5);
            if let Some(w) = st.lcd.editor.def.widgets.iter_mut().find(|w| w.id == id) {
                w.params.insert(
                    "variant".to_string(),
                    EffectParamValue::Str("ring".to_string()),
                );
            }
        }
        _ => {}
    }
}

/// `"w{n}"` where `n` is one past the highest existing numeric suffix.
fn next_widget_id(widgets: &[WidgetDef]) -> String {
    let max = widgets
        .iter()
        .filter_map(|w| w.id.strip_prefix('w')?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("w{}", max + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::lcd_custom::{param_bool, param_f64, param_variant, CustomTemplateDef};

    #[test]
    fn library_starts_expanded() {
        assert!(!super::super::EditorState::default().library_collapsed);
    }

    #[test]
    fn resolve_delete_confirm_only_deletes_on_confirm() {
        let mut pending = Some("t".to_string());
        assert_eq!(resolve_delete_confirm(&mut pending, false, false), None);
        assert_eq!(pending.as_deref(), Some("t"));

        assert_eq!(resolve_delete_confirm(&mut pending, false, true), None);
        assert_eq!(pending, None);

        pending = Some("t".to_string());
        assert_eq!(
            resolve_delete_confirm(&mut pending, true, false).as_deref(),
            Some("t")
        );
        assert_eq!(pending, None);
    }

    #[test]
    fn spawn_widget_seeds_schema_defaults_and_increments_id() {
        let mut st = DeviceUi::new("lcd".into());
        st.lcd.editor.def.widgets.clear();
        let id1 = spawn_widget(&mut st, WidgetType::Sensor, 0.5, 0.5);
        assert_eq!(id1, "w1");
        let w = &st.lcd.editor.def.widgets[0];
        // The sensor schema seeds a "variant" default of "stat".
        assert_eq!(param_variant(w, "MISSING_DEFAULT"), "stat");
        assert!(w.params.contains_key("sensor"));

        let id2 = spawn_widget(&mut st, WidgetType::Clock, 0.1, 0.1);
        assert_eq!(id2, "w2");
    }

    #[test]
    fn spawn_widget_seeds_audio_widget_schema_defaults() {
        let mut st = DeviceUi::new("lcd".into());
        st.lcd.editor.def.widgets.clear();
        spawn_widget(&mut st, WidgetType::AudioSpectrum, 0.5, 0.5);
        let w = &st.lcd.editor.def.widgets[0];
        assert_eq!(param_f64(w, "bands", -1.0), 32.0);

        spawn_widget(&mut st, WidgetType::AudioLevel, 0.5, 0.5);
        let w = &st.lcd.editor.def.widgets[1];
        assert_eq!(param_variant(w, "MISSING_DEFAULT"), "ring");
        assert!(w.params.contains_key("track"));
    }

    #[test]
    fn spawn_widget_seeds_now_playing_schema_defaults() {
        let mut st = DeviceUi::new("lcd".into());
        st.lcd.editor.def.widgets.clear();
        spawn_widget(&mut st, WidgetType::NowPlaying, 0.5, 0.5);
        let w = &st.lcd.editor.def.widgets[0];
        assert!(param_bool(w, "show_art", false));
        assert!(param_bool(w, "show_title", false));
        assert!(param_bool(w, "show_artist", false));
    }

    #[test]
    fn library_covers_every_widget_type() {
        for wt in [
            WidgetType::Clock,
            WidgetType::Date,
            WidgetType::Sensor,
            WidgetType::Text,
            WidgetType::Image,
            WidgetType::Debug,
            WidgetType::AudioSpectrum,
            WidgetType::AudioLevel,
            WidgetType::NowPlaying,
        ] {
            assert!(
                LIBRARY.iter().any(|&(t, _)| t == wt),
                "LIBRARY must list {wt:?}"
            );
        }
    }

    #[test]
    fn next_widget_id_is_unique_and_increments() {
        assert_eq!(next_widget_id(&[]), "w1");
        let widgets = vec![
            WidgetDef {
                id: "w1".into(),
                widget_type: WidgetType::Text,
                x: 0.0,
                y: 0.0,
                scale: 1.0,
                rotation: 0.0,
                color: None,
                font: None,
                params: HashMap::new(),
            },
            WidgetDef {
                id: "w3".into(),
                widget_type: WidgetType::Text,
                x: 0.0,
                y: 0.0,
                scale: 1.0,
                rotation: 0.0,
                color: None,
                font: None,
                params: HashMap::new(),
            },
        ];
        assert_eq!(next_widget_id(&widgets), "w4");
    }

    #[test]
    fn next_widget_id_ignores_non_numeric_ids() {
        let widgets = vec![WidgetDef {
            id: "custom-id".into(),
            widget_type: WidgetType::Text,
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            rotation: 0.0,
            color: None,
            font: None,
            params: HashMap::new(),
        }];
        assert_eq!(next_widget_id(&widgets), "w1");
    }

    #[test]
    fn presets_round_trip_through_json() {
        let mut def = CustomTemplateDef::default();
        def.widgets.push(WidgetDef {
            id: "w1".into(),
            widget_type: WidgetType::Text,
            x: 0.5,
            y: 0.5,
            scale: 1.0,
            rotation: 0.0,
            color: None,
            font: None,
            params: HashMap::new(),
        });
        let json = serde_json::to_string(&def).unwrap();
        let back: CustomTemplateDef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, def);
    }

    #[test]
    fn every_preset_spawns_in_bounds_widgets_with_unbound_sensors() {
        for &preset in PRESETS {
            let mut st = DeviceUi::new("lcd".into());
            apply_preset(&mut st, preset);
            assert!(
                !st.lcd.editor.def.widgets.is_empty(),
                "preset {preset} must spawn at least one widget"
            );
            for w in &st.lcd.editor.def.widgets {
                assert!((0.0..=1.0).contains(&w.x));
                assert!((0.0..=1.0).contains(&w.y));
                if w.widget_type == WidgetType::Sensor {
                    let sensor = w.params.get("sensor");
                    assert!(
                        sensor.is_none()
                            || matches!(sensor, Some(EffectParamValue::Str(s)) if s.is_empty()),
                        "preset {preset} must leave the sensor unbound"
                    );
                }
            }
        }
    }
}
