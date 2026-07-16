// SPDX-License-Identifier: GPL-3.0-or-later
//! Canvas modals: assign-zones, new-instance picker, FPS adjust.

use std::collections::HashMap;

use egui::Vec2;
use halod_shared::{
    commands::{DaemonCommand, EngineKind},
    effect_designer::DESIGNER_PIXMAP_EFFECT_ID,
    types::{AppState, DeviceCapability, EffectParamValue, PlacedZone},
};

use crate::runtime::ipc::CommandTx;
use crate::ui::components as widgets;
use crate::ui::theme;

use super::params::{assign_zone_cmd, upsert_instance_cmd};
use super::rack::{instance_color_for, instance_name};
use super::{CanvasUi, DEBOUNCE};

/// Modal assigning device zones to one effect instance (design: "Assign zones
/// · <name>"). Lists every RGB device's zones; ● marks zones driven by a
/// different instance — clicking one reassigns it here.
pub(super) fn zones_assign_modal(
    ctx: &egui::Context,
    state: &AppState,
    canvas_ui: &mut CanvasUi,
    cmd: &CommandTx,
) {
    let Some(inst_id) = canvas_ui.zones_modal.clone() else {
        return;
    };
    let Some(def) = state.lighting.canvas.effects.get(&inst_id) else {
        canvas_ui.zones_modal = None;
        return;
    };
    let is_default = state.lighting.canvas.default_effect.as_deref() == Some(inst_id.as_str());
    let color = instance_color_for(state, Some(inst_id.as_str()));
    let eff_name = state
        .lighting
        .canvas
        .available_effects
        .iter()
        .find(|e| e.id == def.effect_id)
        .map(|e| e.name.as_str())
        .unwrap_or(def.effect_id.as_str());
    let assigned = state
        .lighting
        .canvas
        .placed_zones
        .iter()
        .filter(|z| match z.effect.as_deref() {
            Some(e) => e == inst_id,
            None => is_default,
        })
        .count();
    let title = t!(
        "canvas.assign_zones_title",
        name = instance_name(&state.lighting.canvas.effects, &inst_id)
    );
    let zone_word = if assigned == 1 {
        t!("canvas.unit_zone")
    } else {
        t!("canvas.unit_zones")
    };
    let subtitle = t!(
        "canvas.assign_zones_sub",
        eff = eff_name,
        n = assigned,
        zones = zone_word
    );

    let mut done = false;
    let closed = widgets::dialog(
        ctx,
        "canvas_assign_zones",
        &title,
        600.0,
        |ui| {
            ui.label(
                egui::RichText::new(subtitle.to_string())
                    .font(theme::body_sm())
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(6.0);
            for dev in &state.devices {
                if crate::domain::models::device::is_hidden(dev) {
                    continue;
                }
                let Some(rgb) = dev.capabilities.iter().find_map(|c| match c {
                    DeviceCapability::Rgb(r) => Some(r),
                    _ => None,
                }) else {
                    continue;
                };
                if rgb.descriptor.zones.is_empty() {
                    continue;
                }
                let assign = (!is_default).then_some(inst_id.as_str());
                let placed_of = |zone_id: &str| {
                    state
                        .lighting
                        .canvas
                        .placed_zones
                        .iter()
                        .find(|p| p.device_id == dev.id && p.zone_id == zone_id)
                };
                let on_count = rgb
                    .descriptor
                    .zones
                    .iter()
                    .filter(|z| modal_zone_state(placed_of(&z.id), &inst_id, is_default).0)
                    .count();
                let total = rgb.descriptor.zones.len();
                let all_on = on_count == total;

                ui.add_space(10.0);
                egui::Sides::new().show(
                    ui,
                    |ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new(&dev.name)
                                    .font(theme::heading())
                                    .color(theme::TEXT),
                            );
                            let zw = if total == 1 {
                                t!("canvas.unit_zone")
                            } else {
                                t!("canvas.unit_zones")
                            };
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} · {total} {zw}",
                                    crate::domain::models::device::type_label(dev),
                                ))
                                .font(theme::caption())
                                .color(theme::TEXT_FAINT),
                            );
                        });
                    },
                    |ui| {
                        let sel_all_label = if all_on {
                            t!("canvas.clear")
                        } else {
                            t!("canvas.select_all")
                        };
                        if widgets::button(
                            ui,
                            &sel_all_label,
                            widgets::ButtonKind::Ghost,
                            egui::vec2(78.0, 26.0),
                        )
                        .clicked()
                        {
                            for z in &rgb.descriptor.zones {
                                let placed = placed_of(&z.id);
                                let (on, _) = modal_zone_state(placed, &inst_id, is_default);
                                // Select all assigns the missing zones; Clear removes them all.
                                if on == all_on {
                                    crate::runtime::ipc::send(
                                        cmd,
                                        assign_zone_cmd(&dev.id, &z.id, assign, on, placed),
                                    );
                                }
                            }
                        }
                        ui.label(
                            egui::RichText::new(format!("{on_count}/{total}"))
                                .font(theme::mono(10.5))
                                .color(theme::TEXT_FAINT),
                        );
                    },
                );
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                    for z in &rgb.descriptor.zones {
                        let placed = placed_of(&z.id);
                        let (on, marked) = modal_zone_state(placed, &inst_id, is_default);
                        let label = if marked {
                            format!("● {}", z.name)
                        } else {
                            z.name.clone()
                        };
                        if widgets::pill_styled(ui, &label, on, color, theme::INNER_BG) {
                            crate::runtime::ipc::send(
                                cmd,
                                assign_zone_cmd(&dev.id, &z.id, assign, on, placed),
                            );
                        }
                    }
                });
            }
        },
        |ui| {
            done = widgets::button(
                ui,
                &t!("canvas.done"),
                widgets::ButtonKind::Primary,
                Vec2::new(90.0, 32.0),
            )
            .clicked();
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(t!("canvas.reassign_hint").to_string())
                        .font(theme::caption())
                        .color(theme::TEXT_FAINT),
                );
            });
        },
    );
    if done || closed {
        canvas_ui.zones_modal = None;
    }
}

/// Grid picker for creating a new effect instance — blank effects or from saved presets.
pub(super) fn new_instance_modal(
    ctx: &egui::Context,
    state: &AppState,
    canvas_ui: &mut CanvasUi,
    cmd: &CommandTx,
) {
    let effects = &state.lighting.canvas.effects;
    let mut created = false;
    let mut cancelled = false;
    let closed = widgets::dialog(
        ctx,
        "canvas_new_instance",
        &t!("canvas.new_instance_title"),
        560.0,
        |ui| {
            // Build a unified list: built-in effects first, then designer
            // presets with a ★ icon prefix (like firmware effects show ⚙).
            struct Entry {
                label: String,
                effect_id: String,
                params: HashMap<String, EffectParamValue>,
                /// Clean preset name (without the ★ prefix), if this is a
                /// designer preset; `None` for built-in effects.
                preset_name: Option<String>,
            }
            let mut entries: Vec<Entry> = state
                .lighting
                .canvas
                .available_effects
                .iter()
                .map(|eff| Entry {
                    label: eff.name.clone(),
                    effect_id: eff.id.clone(),
                    params: eff
                        .params
                        .iter()
                        .map(|p| (p.id.clone(), p.default.clone()))
                        .collect(),
                    preset_name: None,
                })
                .collect();
            for preset in &state.lighting.canvas.custom_direct_effects {
                let name = preset
                    .name
                    .clone()
                    .unwrap_or_else(|| t!("canvas.untitled").to_string());
                entries.push(Entry {
                    label: format!("\u{2605} {name}"),
                    effect_id: DESIGNER_PIXMAP_EFFECT_ID.to_string(),
                    params: preset.params.clone(),
                    preset_name: Some(name),
                });
            }

            widgets::caps_label(ui, &t!("canvas.effect_caps"));
            ui.add_space(8.0);
            let gap = 10.0;
            let cols = 4usize;
            let cell_w =
                ((ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32).max(60.0);
            for (row, chunk) in entries.chunks(cols).enumerate() {
                if row > 0 {
                    ui.add_space(gap);
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for entry in chunk {
                        if widgets::effect_cell(
                            ui,
                            &entry.label,
                            false,
                            widgets::CellPreview::Spectrum,
                            cell_w,
                            66.0,
                            34.0,
                            false,
                        ) {
                            let id = new_instance_id(effects);
                            crate::runtime::ipc::send(
                                cmd,
                                upsert_instance_cmd(
                                    id,
                                    entry.effect_id.clone(),
                                    entry.preset_name.clone(),
                                    entry.params.clone(),
                                ),
                            );
                            created = true;
                        }
                    }
                });
            }
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("canvas.cancel"),
                widgets::ButtonKind::Ghost,
                egui::vec2(90.0, 32.0),
            )
            .clicked()
            {
                cancelled = true;
            }
        },
    );
    if created || cancelled || closed {
        canvas_ui.new_instance_modal = false;
    }
}

/// Modal to adjust the canvas engine frame rate. Mirrors the GTK FPS control.
pub(crate) fn fps_modal(
    ctx: &egui::Context,
    state: &AppState,
    canvas_ui: &mut CanvasUi,
    time: f64,
) {
    // Re-seed unless mid-edit, else the slider snaps back every frame.
    if time - canvas_ui.fps_edit_at >= 1.0 {
        canvas_ui.canvas_fps = state.lighting.config.canvas_fps as f32;
    }
    let mut done = false;
    let closed = widgets::dialog(
        ctx,
        "canvas_fps",
        &t!("canvas.fps_title"),
        316.0,
        |ui| {
            ui.label(
                egui::RichText::new(t!("canvas.fps_desc").to_string())
                    .font(theme::body_sm())
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(16.0);
            let mut fps = canvas_ui.canvas_fps;
            let readout = format!("{} fps", fps.round() as u32);
            if widgets::slider_row(ui, &t!("canvas.frame_rate"), &mut fps, 1.0..=60.0, &readout) {
                canvas_ui.canvas_fps = fps;
                canvas_ui.fps_edit_at = time;
                canvas_ui.pending.fps = Some((
                    DaemonCommand::set_engine_fps(EngineKind::Canvas, fps.round() as u64),
                    time + DEBOUNCE,
                ));
            }
        },
        |ui| {
            done = widgets::button(
                ui,
                &t!("canvas.done"),
                widgets::ButtonKind::Primary,
                Vec2::new(90.0, 32.0),
            )
            .clicked();
        },
    );
    if done || closed {
        canvas_ui.fps_modal_open = false;
    }
}

/// Display state of a zone pill in the assign-zones modal: `on` when the zone
/// is driven by this instance, `marked` when it is placed under another one.
fn modal_zone_state(placed: Option<&PlacedZone>, inst_id: &str, is_default: bool) -> (bool, bool) {
    let on = placed.is_some_and(|p| {
        if is_default {
            p.effect.is_none()
        } else {
            p.effect.as_deref() == Some(inst_id)
        }
    });
    (on, placed.is_some() && !on)
}

/// A fresh instance id not already present in `effects`.
fn new_instance_id(effects: &HashMap<String, halod_shared::types::EffectDef>) -> String {
    (1..)
        .map(|n| format!("effect-{n}"))
        .find(|id| !effects.contains_key(id))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screens::canvas::test_fixtures::zone_with;
    use halod_shared::types::EffectDef;

    #[test]
    fn new_instance_id_skips_existing() {
        let mut effects: HashMap<String, EffectDef> = HashMap::new();
        assert_eq!(new_instance_id(&effects), "effect-1");
        effects.insert(
            "effect-1".into(),
            EffectDef {
                effect_id: "static_color".into(),
                name: None,
                params: HashMap::new(),
            },
        );
        effects.insert(
            "effect-2".into(),
            EffectDef {
                effect_id: "static_color".into(),
                name: None,
                params: HashMap::new(),
            },
        );
        assert_eq!(new_instance_id(&effects), "effect-3");
    }

    #[test]
    fn modal_zone_state_tracks_owner() {
        let mine = zone_with("z", Some("effect-1"));
        let other = zone_with("z", Some("effect-2"));
        let unowned = zone_with("z", None);
        // Non-default instance: owns only its explicit zones.
        assert_eq!(
            modal_zone_state(Some(&mine), "effect-1", false),
            (true, false)
        );
        assert_eq!(
            modal_zone_state(Some(&other), "effect-1", false),
            (false, true)
        );
        assert_eq!(
            modal_zone_state(Some(&unowned), "effect-1", false),
            (false, true)
        );
        assert_eq!(modal_zone_state(None, "effect-1", false), (false, false));
        // Default instance: owns the unassigned placed zones.
        assert_eq!(
            modal_zone_state(Some(&unowned), "effect-1", true),
            (true, false)
        );
        assert_eq!(
            modal_zone_state(Some(&other), "effect-1", true),
            (false, true)
        );
    }
}
