// SPDX-License-Identifier: GPL-3.0-or-later
//! Canvas modals: assign-channels, new-instance picker, FPS adjust.

use std::collections::{HashMap, HashSet};

use crate::domain::topic_store::TopicStore;
use egui::Vec2;
use halod_shared::{
    commands::{DaemonCommand, EngineKind},
    effect_designer::DESIGNER_PIXMAP_EFFECT_ID,
    types::{DeviceCapability, EffectParamValue, PlacedZone},
};

use crate::runtime::ipc::CommandTx;
use crate::ui::components as widgets;
use crate::ui::theme;

use super::params::{assign_zone_cmd, upsert_instance_cmd};
use super::rack::instance_name;
use super::{CanvasUi, DEBOUNCE};
use crate::domain::models::device;
use halod_shared::ids::DeviceId;

type ZoneKey = (DeviceId, String);

/// Every listable RGB zone as `(device id, device name, zones)`.
fn rgb_zone_groups(
    state: &TopicStore,
) -> Vec<(&DeviceId, &str, &[halod_shared::types::LightingChannel])> {
    state
        .devices
        .iter()
        .filter(|d| !device::is_hidden(d))
        .filter_map(|d| {
            d.capabilities.iter().find_map(|c| match c {
                DeviceCapability::Lighting(r) if !r.descriptor.channels.is_empty() => {
                    Some((&d.id, d.name.as_str(), r.descriptor.channels.as_slice()))
                }
                _ => None,
            })
        })
        .collect()
}

fn placed_of<'a>(state: &'a TopicStore, dev: &DeviceId, zone: &str) -> Option<&'a PlacedZone> {
    state
        .lighting
        .canvas
        .placed_zones
        .iter()
        .find(|p| p.device_id == dev && p.channel_id == zone)
}

/// The zones layer `inst_id` currently drives.
fn owned_zones(state: &TopicStore, inst_id: &str, is_default: bool) -> HashSet<ZoneKey> {
    state
        .lighting
        .canvas
        .placed_zones
        .iter()
        .filter(|p| modal_zone_state(Some(p), inst_id, is_default).0)
        .map(|p| (p.device_id.clone(), p.channel_id.clone()))
        .collect()
}

/// Commands turning the layer's current zones into `draft`: one toggle per
/// zone whose membership differs.
fn draft_commands(
    state: &TopicStore,
    inst_id: &str,
    is_default: bool,
    draft: &HashSet<ZoneKey>,
) -> Vec<DaemonCommand> {
    let assign = (!is_default).then_some(inst_id);
    let owned = owned_zones(state, inst_id, is_default);
    let mut keys: Vec<&ZoneKey> = owned.symmetric_difference(draft).collect();
    keys.sort();
    keys.into_iter()
        .map(|(dev, zone)| {
            assign_zone_cmd(
                dev,
                zone,
                assign,
                owned.contains(&(dev.clone(), zone.clone())),
                placed_of(state, dev, zone),
            )
        })
        .collect()
}

/// Modal choosing which zones one layer drives. Selections are staged and
/// applied on confirm; zones driven by another layer move to this one.
pub(super) fn zones_assign_modal(
    ctx: &egui::Context,
    state: &TopicStore,
    canvas_ui: &mut CanvasUi,
    cmd: &CommandTx,
) {
    let Some(inst_id) = canvas_ui.zones_modal.clone() else {
        return;
    };
    if !state.lighting.canvas.effects.contains_key(&inst_id) {
        canvas_ui.zones_modal = None;
        canvas_ui.zones_draft = None;
        return;
    }
    let is_default = state.lighting.canvas.default_effect.as_deref() == Some(inst_id.as_str());
    let effects = &state.lighting.canvas.effects;
    let draft = canvas_ui
        .zones_draft
        .get_or_insert_with(|| owned_zones(state, &inst_id, is_default));
    let n = draft.len();
    let title = t!(
        "canvas.assign_zones_title",
        name = instance_name(effects, &inst_id)
    );

    let (mut assign, mut cancel) = (false, false);
    let closed = widgets::dialog_with_subtitle(
        ctx,
        "canvas_assign_zones",
        &title,
        &t!("canvas.assign_zones_sub"),
        600.0,
        |ui| {
            for (dev_id, dev_name, zones) in rgb_zone_groups(state) {
                let on = zones
                    .iter()
                    .filter(|z| draft.contains(&(dev_id.clone(), z.id.clone())))
                    .count();
                ui.add_space(theme::SPACE_5);
                egui::Sides::new().show(
                    ui,
                    |ui| {
                        ui.label(
                            egui::RichText::new(dev_name)
                                .font(theme::subhead())
                                .color(theme::TEXT),
                        );
                    },
                    |ui| {
                        ui.label(
                            egui::RichText::new(t!(
                                "canvas.n_of_total",
                                n = on,
                                total = zones.len()
                            ))
                            .font(theme::body_md())
                            .color(theme::TEXT_MUT),
                        );
                    },
                );
                ui.add_space(theme::SPACE_3);
                for z in zones {
                    let key = (dev_id.clone(), z.id.clone());
                    let checked = draft.contains(&key);
                    let used_by = placed_of(state, dev_id, &z.id)
                        .filter(|p| !checked && modal_zone_state(Some(p), &inst_id, is_default).1)
                        .and_then(|p| {
                            p.effect
                                .as_deref()
                                .or(state.lighting.canvas.default_effect.as_deref())
                        })
                        .map(|other| {
                            t!("canvas.used_by", name = instance_name(effects, other)).to_string()
                        });
                    let leds = if z.leds.len() == 1 {
                        t!("canvas.led_one").to_string()
                    } else {
                        t!("canvas.led_count", n = z.leds.len()).to_string()
                    };
                    let meta = used_by.as_deref().map(|m| (m, theme::WARN_TEXT));
                    if widgets::check_row(ui, &z.name, Some(&leds), meta, checked).clicked() {
                        if checked {
                            draft.remove(&key);
                        } else {
                            draft.insert(key);
                        }
                    }
                    ui.add_space(theme::SPACE_3);
                }
            }
        },
        |ui| {
            assign = widgets::button(
                ui,
                &t!("canvas.assign_n", n = n),
                widgets::ButtonKind::Primary,
                Vec2::new(0.0, 36.0),
            )
            .clicked();
            cancel = widgets::button(
                ui,
                &t!("canvas.cancel"),
                widgets::ButtonKind::Secondary,
                Vec2::new(0.0, 36.0),
            )
            .clicked();
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(t!("canvas.n_selected", n = n))
                        .font(theme::body_md())
                        .color(theme::TEXT_DIM),
                );
            });
        },
    );
    if assign {
        if let Some(draft) = &canvas_ui.zones_draft {
            for c in draft_commands(state, &inst_id, is_default, draft) {
                crate::runtime::ipc::send(cmd, c);
            }
        }
    }
    if assign || cancel || closed {
        canvas_ui.zones_modal = None;
        canvas_ui.zones_draft = None;
    }
}

/// Grid picker for creating a new effect instance — blank effects or from saved presets.
pub(super) fn new_instance_modal(
    ctx: &egui::Context,
    state: &TopicStore,
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

            widgets::section_label(ui, &t!("canvas.effect"));
            ui.add_space(theme::SPACE_4);
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
                        ) {
                            let id = new_instance_id(effects);
                            crate::runtime::ipc::send(
                                cmd,
                                upsert_instance_cmd(
                                    id,
                                    entry.effect_id.clone(),
                                    Some(default_instance_name(
                                        effects,
                                        entry.preset_name.as_deref().unwrap_or(&entry.label),
                                    )),
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
                widgets::ButtonKind::Secondary,
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
    state: &TopicStore,
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
            ui.add_space(theme::SPACE_8);
            let mut fps = canvas_ui.canvas_fps;
            let readout = format!("{} fps", fps.round() as u32);
            if widgets::slider_row(ui, &t!("canvas.frame_rate"), &mut fps, 1.0..=60.0, &readout) {
                canvas_ui.canvas_fps = fps;
                canvas_ui.fps_edit_at = time;
                canvas_ui.pending.queue(
                    super::FPS_KEY,
                    DaemonCommand::set_engine_fps(EngineKind::Canvas, fps.round() as u64),
                    time,
                    DEBOUNCE,
                );
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

/// Display state of a zone pill in the assign-channels modal: `on` when the zone
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

fn default_instance_name(
    effects: &HashMap<String, halod_shared::types::EffectDef>,
    effect_name: &str,
) -> String {
    let taken: std::collections::HashSet<&str> =
        effects.values().filter_map(|d| d.name.as_deref()).collect();
    let mut name = effect_name.to_string();
    let mut n = 1;
    while taken.contains(name.as_str()) {
        n += 1;
        name = format!("{effect_name} {n}");
    }
    name
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
    use halod_shared::commands::DeviceOp;
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
    fn a_new_layer_is_named_after_its_effect_without_clashing() {
        let named = |name: &str| EffectDef {
            effect_id: "rainbow".into(),
            name: Some(name.into()),
            params: HashMap::new(),
        };
        let mut effects: HashMap<String, EffectDef> = HashMap::new();
        assert_eq!(default_instance_name(&effects, "Rainbow"), "Rainbow");
        effects.insert("effect-1".into(), named("Rainbow"));
        effects.insert("effect-2".into(), named("Rainbow 2"));
        assert_eq!(default_instance_name(&effects, "Rainbow"), "Rainbow 3");
        assert_eq!(default_instance_name(&effects, "Breathing"), "Breathing");
    }

    #[test]
    fn draft_commands_toggle_only_changed_zones() {
        let mut state = TopicStore::default();
        state.lighting.canvas.placed_zones = vec![
            zone_with("a", Some("effect-1")),
            zone_with("b", Some("effect-2")),
            zone_with("c", Some("effect-1")),
        ];
        let key = |z: &str| ("d".parse::<DeviceId>().unwrap(), z.to_string());
        let unchanged: HashSet<_> = [key("a"), key("c")].into();
        assert!(draft_commands(&state, "effect-1", false, &unchanged).is_empty());

        let draft: HashSet<_> = [key("a"), key("b")].into();
        let cmds = draft_commands(&state, "effect-1", false, &draft);
        assert_eq!(cmds.len(), 2);
        assert!(matches!(&cmds[0],
            DaemonCommand::Device { op: DeviceOp::CanvasMoveZone { channel_id, effect: Some(e), .. }, .. }
                if channel_id == "b" && e == "effect-1"));
        assert!(matches!(&cmds[1],
            DaemonCommand::Device { op: DeviceOp::CanvasRemoveZone { channel_id, .. }, .. } if channel_id == "c"));
    }

    #[test]
    fn modal_zone_state_tracks_owner() {
        let mine = zone_with("z", Some("effect-1"));
        let other = zone_with("z", Some("effect-2"));
        let unowned = zone_with("z", None);
        // Non-default instance: owns only its explicit channels.
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
        // Default instance: owns the unassigned placed channels.
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
