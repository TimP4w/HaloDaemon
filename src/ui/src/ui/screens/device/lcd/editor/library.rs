// SPDX-License-Identifier: GPL-3.0-or-later
//! Widget library palette, preset picker, "my templates" list + delete modal,
//! and the pure spawn/preset logic behind them.

use std::collections::HashMap;

use egui::{Rect, Sense, Stroke, Vec2};
use halod_shared::commands::DaemonCommand;
use halod_shared::lcd_custom::WidgetDef;
use halod_shared::types::EffectParamValue;

use super::{send_def, DeviceUi, TabCtx};
use crate::ui::components as widgets;
use crate::ui::theme;

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
        for descriptor in &ctx.state.lcd.engine.available_widgets {
            let asset_key = format!("lcd/{}", descriptor.id);
            if !ctx.plugin_assets.contains_key(&asset_key)
                && st
                    .lcd
                    .editor
                    .requested_widget_icons
                    .insert(descriptor.id.clone())
            {
                crate::runtime::ipc::send(
                    ctx.cmd,
                    DaemonCommand::GetLcdWidgetIcon {
                        catalog_id: descriptor.id.clone(),
                    },
                );
            }
            if !st.lcd.editor.widget_icon_tex.contains_key(&descriptor.id) {
                if let Some(bytes) = ctx.plugin_assets.get(&asset_key) {
                    if let Some(texture) = widgets::tex_from_bytes(
                        ui.ctx(),
                        bytes,
                        &format!("lcd_widget_icon_{}", descriptor.id),
                    ) {
                        st.lcd
                            .editor
                            .widget_icon_tex
                            .insert(descriptor.id.clone(), texture);
                    }
                }
            }
            let label = std::borrow::Cow::Owned(descriptor.name.clone());
            let resp = match st.lcd.editor.widget_icon_tex.get(&descriptor.id) {
                Some(texture) => library_texture_tile(ui, texture, &label),
                None => library_tile(ui, "…", &label),
            };
            if resp.drag_started() {
                st.lcd.editor.dragging_new = Some(descriptor.id.clone());
            }
            if resp.clicked() {
                let new_id = spawn_plugin_widget(st, descriptor, 0.5, 0.5);
                st.lcd.editor.select_only(new_id);
                send_def(ctx, st, id, true);
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
    widgets::pill_strip(ui, |ui| {
        for preset in &ctx.state.lcd.engine.available_presets {
            if widgets::pill(ui, &preset.name, false) {
                crate::runtime::ipc::send(
                    ctx.cmd,
                    DaemonCommand::GetLcdPluginPreset {
                        catalog_id: preset.id.clone(),
                    },
                );
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
        widgets::text_field(
            ui,
            &mut st.lcd.editor.template_name,
            &t!("lcd.template_name_hint"),
            160.0,
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
    widgets::pill_strip(ui, |ui| {
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
    if let Some(name) = widgets::confirm_delete_dialog(
        ui.ctx(),
        "lcd_delete_template",
        &t!("lcd.delete_template_title"),
        &t!("lcd.delete_confirm_body", name = name),
        &t!("lcd.delete"),
        &mut st.lcd.editor.confirm_delete,
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
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
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

fn library_texture_tile(
    ui: &mut egui::Ui,
    texture: &egui::TextureHandle,
    label: &str,
) -> egui::Response {
    let size = egui::vec2(80.0, 74.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
    theme::paint_well(ui.painter(), rect, theme::RADIUS_MD);
    let icon = Rect::from_center_size(rect.center() - egui::vec2(0.0, 10.0), Vec2::splat(34.0));
    ui.painter().image(
        texture.id(),
        icon,
        Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
    ui.painter().text(
        rect.center() + egui::vec2(0.0, 22.0),
        egui::Align2::CENTER_CENTER,
        label,
        theme::caption(),
        theme::TEXT_MUT,
    );
    response
}

/// Push a catalog widget at normalized `(x, y)`, seeded from its descriptor.
pub(super) fn spawn_plugin_widget(
    st: &mut DeviceUi,
    descriptor: &halod_shared::types::LcdWidgetDescriptor,
    x: f32,
    y: f32,
) -> String {
    let new_id = next_widget_id(&st.lcd.editor.def.widgets);
    let mut params: HashMap<String, EffectParamValue> = descriptor
        .params
        .iter()
        .map(|param| (param.id.clone(), param.default.clone()))
        .collect();
    params.insert(
        halod_shared::lcd_custom::OPACITY_PARAM.to_owned(),
        EffectParamValue::Float(100.0),
    );
    if descriptor.resize == halod_shared::types::LcdWidgetResize::Box {
        params.insert(
            halod_shared::lcd_custom::SCALE_Y_PARAM.to_owned(),
            EffectParamValue::Float(f64::from(
                descriptor.default_scale * descriptor.default_aspect,
            )),
        );
    }
    st.lcd.editor.def.widgets.push(WidgetDef {
        id: new_id.clone(),
        widget: descriptor.id.clone(),
        x,
        y,
        scale: descriptor.default_scale,
        rotation: 0.0,
        color: descriptor
            .uses_color
            .then_some(halod_shared::types::RgbColor {
                r: 255,
                g: 255,
                b: 255,
            }),
        font: descriptor.default_font.clone(),
        params,
    });
    new_id
}

/// Preset widget arrangements (sensor left unbound — the user picks it in the
/// inspector), matching the design's `lcdPresets` chips.
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

    fn descriptor(uses_color: bool) -> halod_shared::types::LcdWidgetDescriptor {
        halod_shared::types::LcdWidgetDescriptor {
            id: "halo_lcd:text".to_owned(),
            plugin_id: "halo_lcd".to_owned(),
            name: "Text".to_owned(),
            icon: "text.svg".to_owned(),
            assets: Vec::new(),
            params: Vec::new(),
            resize: halod_shared::types::LcdWidgetResize::Uniform,
            default_scale: 1.0,
            min_scale: 0.6,
            default_aspect: 1.0,
            auto_width_param: None,
            param_visibility: HashMap::new(),
            uses_color,
            uses_font: uses_color,
            font_controls: true,
            default_font: None,
            fixed_text_weight: None,
            updates: halod_shared::types::LcdWidgetUpdates::default(),
        }
    }

    #[test]
    fn new_text_widgets_start_white_without_changing_screen_accent() {
        let mut state = DeviceUi::new("lcd".to_owned());
        state.lcd.editor.def.widgets.clear();
        let accent = state.lcd.editor.def.style.accent;

        spawn_plugin_widget(&mut state, &descriptor(true), 0.5, 0.5);

        assert_eq!(
            state.lcd.editor.def.widgets[0].color,
            Some(halod_shared::types::RgbColor {
                r: 255,
                g: 255,
                b: 255,
            })
        );
        assert_eq!(state.lcd.editor.def.style.accent, accent);
    }

    #[test]
    fn non_text_widgets_do_not_gain_a_widget_color() {
        let mut state = DeviceUi::new("lcd".to_owned());
        state.lcd.editor.def.widgets.clear();

        spawn_plugin_widget(&mut state, &descriptor(false), 0.5, 0.5);

        assert_eq!(state.lcd.editor.def.widgets[0].color, None);
    }
}
