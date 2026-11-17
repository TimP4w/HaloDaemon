// SPDX-License-Identifier: GPL-3.0-or-later
//! Widget library palette, preset picker, "my templates" list + delete modal,
//! and the pure spawn/preset logic behind them.

use std::collections::HashMap;

use egui::{Rect, Sense, Vec2};
use halod_shared::commands::DaemonCommand;
use halod_shared::lcd_custom::WidgetDef;
use halod_shared::types::EffectParamValue;

use super::{send_def, DeviceUi, TabCtx};
use crate::domain::tour;
use crate::domain::tour::AnchorId;
use crate::runtime::ipc;
use crate::ui::components as widgets;
use crate::ui::theme;

pub(super) fn library_card(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, id: &str) {
    tour::anchor(ui.ctx(), AnchorId::LcdEditorPalette, ui.max_rect());
    ui.set_width(ui.available_width());
    ui.spacing_mut().item_spacing.y = theme::SPACE_5;
    egui::Sides::new().show(
        ui,
        |ui| {
            ui.label(
                egui::RichText::new(t!("lcd.add_a_widget"))
                    .font(theme::heading())
                    .color(theme::TEXT),
            );
        },
        |ui| {
            ui.label(
                egui::RichText::new(t!("lcd.drag_onto_screen"))
                    .font(theme::body_sm())
                    .color(theme::TEXT_MUT),
            );
        },
    );
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
        for descriptor in &ctx.state.lcd.engine.available_widgets {
            let asset_key = format!("lcd/{}", descriptor.id);
            if !ctx.plugin_assets.contains_key(&asset_key)
                && st
                    .lcd
                    .editor
                    .requested_widget_icons
                    .insert(descriptor.id.clone())
            {
                ipc::send(
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
            let label = descriptor.localized_name(&ctx.state.gui.language);
            let resp = library_tile(ui, st.lcd.editor.widget_icon_tex.get(&descriptor.id), label);
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
    if !ctx.state.lcd.engine.available_presets.is_empty() {
        widgets::section_label(ui, &t!("lcd.start_from_preset"));
        widgets::pill_strip(ui, |ui| {
            for preset in &ctx.state.lcd.engine.available_presets {
                if widgets::pill(ui, &preset.name, false) {
                    ipc::send(
                        ctx.cmd,
                        DaemonCommand::GetLcdPluginPreset {
                            catalog_id: preset.id.clone(),
                        },
                    );
                }
            }
        });
    }
    my_templates_section(ui, ctx, st);
}

/// The user's saved templates: click loads (round-trips back on
/// `ctx.lcd_template`, drained in `show`), × asks to delete.
pub(super) fn my_templates_section(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    if ctx.state.lcd.templates.is_empty() {
        return;
    }
    widgets::section_label(ui, &t!("lcd.my_templates"));
    widgets::pill_strip(ui, |ui| {
        for name in &ctx.state.lcd.templates {
            let (load, delete) = widgets::chip_closable(ui, name);
            if load {
                ipc::send(
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

/// "Save as template" dialog: names the current layout and saves it.
pub(super) fn save_template_modal(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    if !st.lcd.editor.save_open {
        return;
    }
    let mut name = std::mem::take(&mut st.lcd.editor.template_name);
    let can_save = !name.trim().is_empty();
    let (mut entered, mut saved, mut cancelled) = (false, false, false);
    let dismissed = widgets::dialog(
        ui.ctx(),
        "lcd_save_template",
        &t!("lcd.save_as_template"),
        360.0,
        |ui| {
            widgets::field_label(ui, &t!("lcd.template_name"));
            let resp = widgets::text_field(
                ui,
                &mut name,
                &t!("lcd.template_name_hint"),
                ui.available_width(),
            );
            entered = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        },
        |ui| {
            saved = widgets::button_enabled(
                ui,
                &t!("lcd.save"),
                widgets::ButtonKind::Primary,
                egui::vec2(72.0, 32.0),
                can_save,
            );
            cancelled = widgets::button(
                ui,
                &t!("lcd.cancel"),
                widgets::ButtonKind::Ghost,
                egui::vec2(72.0, 32.0),
            )
            .clicked();
        },
    );
    st.lcd.editor.template_name = name;
    let save = saved || entered;
    let mut close = cancelled;
    let name = st.lcd.editor.template_name.trim().to_string();
    if save && !name.is_empty() {
        ipc::send(
            ctx.cmd,
            DaemonCommand::SaveLcdTemplate {
                name,
                def: st.lcd.editor.def.clone(),
            },
        );
        close = true;
    }
    if dismissed || close {
        st.lcd.editor.save_open = false;
    }
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
        ipc::send(
            ctx.cmd,
            halod_shared::commands::DaemonCommand::DeleteLcdTemplate { name },
        );
    }
}

/// A widget-library chip: the plugin's own icon and the widget name.
/// Click adds at the centre; press-and-drag drops it onto the stage.
fn library_tile(
    ui: &mut egui::Ui,
    icon: Option<&egui::TextureHandle>,
    label: &str,
) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::body_md(), theme::TEXT_SOFT);
    let w = 8.0 + 18.0 + 8.0 + galley.size().x + 12.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 32.0), Sense::click_and_drag());
    let (fill, border) = if resp.hovered() {
        (theme::RAISED_BG, theme::BORDER_INNER)
    } else {
        (theme::INNER_BG, theme::TRACK)
    };
    let p = ui.painter();
    theme::paint_surface(p, rect, 6.0, fill, border);
    let icon_rect = Rect::from_min_size(
        egui::pos2(rect.left() + 8.0, rect.center().y - 9.0),
        Vec2::splat(18.0),
    );
    if let Some(texture) = icon {
        p.image(
            texture.id(),
            icon_rect,
            Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    } else {
        p.rect_filled(icon_rect, 4.0, theme::TILE_BG);
    }
    p.galley(
        egui::pos2(
            icon_rect.right() + 8.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        theme::TEXT_SOFT,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }
    resp
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
            name_key: None,
            translations: Default::default(),
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
