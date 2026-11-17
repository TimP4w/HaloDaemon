// SPDX-License-Identifier: GPL-3.0-or-later
//! The right-hand sidebar: sampling-radius card and the effect-layer rack
//! (base layer, named layers with param editing and zone assignment).

use std::collections::{HashMap, HashSet};

use crate::domain::topic_store::TopicStore;
use egui::{Color32, Sense, Vec2};
use halod_shared::{
    commands::DaemonCommand,
    effect_designer::DESIGNER_PIXMAP_EFFECT_ID,
    types::{DeviceCapability, EffectDef, PlacedZone, SamplingMode},
};

use crate::runtime::ipc::CommandTx;
use crate::ui::components as widgets;
use crate::ui::theme;

use super::params::{build_instance_params, instance_params, unroll_cmd, upsert_instance_cmd};
use super::{CanvasUi, DEBOUNCE};
use crate::domain::state::Page;
use crate::domain::tour;
use crate::domain::tour::AnchorId;
use crate::ui::screens::effect_designer::DesignerUi;

const PANEL_PAD: egui::Margin = egui::Margin::same(16);

pub(super) fn right_panel(
    ui: &mut egui::Ui,
    state: &TopicStore,
    canvas_ui: &mut CanvasUi,
    cmd: &CommandTx,
    time: f64,
    designer_ui: &mut DesignerUi,
    page: &mut Page,
) {
    let effects = &state.lighting.canvas.effects;
    let mut ids: Vec<String> = effects.keys().cloned().collect();
    ids.sort();
    prune_stale_instance_refs(canvas_ui, &ids);

    if let Some(id) = canvas_ui.selected_instance.clone() {
        let idx = ids.iter().position(|i| *i == id).unwrap_or(0);
        layer_params(ui, state, canvas_ui, cmd, time, &id, idx, designer_ui, page);
        return;
    }
    default_effect_card(
        ui,
        effects,
        &ids,
        state.lighting.canvas.default_effect.clone(),
        cmd,
    );
    ui.add_space(theme::SPACE_6);
    let rack = ui.scope(|ui| layers_card(ui, state, canvas_ui, &ids));
    tour::anchor(ui.ctx(), AnchorId::CanvasInstanceRack, rack.response.rect);
    ui.add_space(theme::SPACE_6);
    sampling_card(ui, state, canvas_ui, time);
}

// ── Sampling card ─────────────────────────────────────────────────────────────
/// Sampling-radius control: how large an area around each LED is averaged from
/// the canvas (0.5–32 px).
fn sampling_card(ui: &mut egui::Ui, state: &TopicStore, canvas_ui: &mut CanvasUi, time: f64) {
    // Re-seed from daemon state unless the user is mid-edit (mirrors LiveGuard).
    if time - canvas_ui.sample_edit_at >= 1.0 {
        canvas_ui.sample_radius = state.lighting.canvas.sample_radius;
    }
    widgets::card_with_margin(ui, PANEL_PAD, |ui| {
        let mut v = canvas_ui.sample_radius;
        let readout = format!("{v:.1} px");
        if widgets::slider_row(
            ui,
            &t!("canvas.sampling_area"),
            &mut v,
            0.5..=32.0,
            &readout,
        ) {
            let snapped = widgets::snap_to_step(v, 0.5, 32.0, 0.5);
            canvas_ui.sample_radius = snapped;
            canvas_ui.sample_edit_at = time;
            canvas_ui.pending.queue(
                super::SAMPLE_KEY,
                DaemonCommand::CanvasSetSampleRadius {
                    radius: snapped as f64,
                },
                time,
                DEBOUNCE,
            );
        }
    });
}

// ── Animation card ────────────────────────────────────────────────────────────
/// Distinct swatch colours per instance, indexed by rack position.
const INSTANCE_PALETTE: [u32; 8] = [
    0x5ad1e8, 0xfbbf24, 0xfb7185, 0x818cf8, 0x2dd4bf, 0xa3e635, 0xf97316, 0x22d3ee,
];

pub(super) fn instance_color(idx: usize) -> Color32 {
    theme::hex(INSTANCE_PALETTE[idx % INSTANCE_PALETTE.len()])
}

/// Clear any per-instance UI selection (rename target, edit-channels modal,
/// buffered param edits, pending debounced upsert) that no longer names a
/// live instance — `new_instance_id` can reuse an id after the original
/// instance was removed elsewhere (another client, profile switch).
fn prune_stale_instance_refs(canvas_ui: &mut CanvasUi, ids: &[String]) {
    let is_live = |id: &str| ids.iter().any(|i| i == id);
    if canvas_ui
        .rename_instance
        .as_deref()
        .is_some_and(|id| !is_live(id))
    {
        canvas_ui.rename_instance = None;
    }
    if canvas_ui
        .zones_modal
        .as_deref()
        .is_some_and(|id| !is_live(id))
    {
        canvas_ui.zones_modal = None;
    }
    if canvas_ui
        .selected_instance
        .as_deref()
        .is_some_and(|id| !is_live(id))
    {
        canvas_ui.selected_instance = None;
    }
    canvas_ui.param_edits.retain(|id, _| is_live(id));
    canvas_ui
        .pending
        .retain(|k, _| k.strip_prefix("effect:").is_none_or(is_live));
}

/// An instance's display label: its user-set name, falling back to the id.
pub(super) fn instance_name<'a>(effects: &'a HashMap<String, EffectDef>, id: &'a str) -> &'a str {
    effects
        .get(id)
        .and_then(|d| d.name.as_deref())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(id)
}

fn device_name<'a>(state: &'a TopicStore, device_id: &'a str) -> &'a str {
    state
        .devices
        .iter()
        .find(|d| d.id == device_id)
        .map(|d| d.name.as_str())
        .unwrap_or(device_id)
}

pub(super) fn zone_name<'a>(
    state: &'a TopicStore,
    device_id: &'a str,
    channel_id: &'a str,
) -> &'a str {
    state
        .devices
        .iter()
        .find(|d| d.id == device_id)
        .and_then(|d| {
            d.capabilities.iter().find_map(|c| match c {
                DeviceCapability::Lighting(r) => {
                    r.descriptor.channels.iter().find(|z| z.id == channel_id)
                }
                _ => None,
            })
        })
        .map(|z| z.name.as_str())
        .unwrap_or(channel_id)
}

/// Lookup an `LightingChannel` descriptor by device and zone id.
pub(super) fn rgb_zone_descriptor<'a>(
    state: &'a TopicStore,
    device_id: &str,
    channel_id: &str,
) -> Option<&'a halod_shared::types::LightingChannel> {
    state
        .devices
        .iter()
        .find(|d| d.id == device_id)
        .and_then(|d| {
            d.capabilities.iter().find_map(|c| match c {
                DeviceCapability::Lighting(r) => {
                    r.descriptor.channels.iter().find(|z| z.id == channel_id)
                }
                _ => None,
            })
        })
}

/// The name to store for a rename-field submission: trimmed, blank → `None`.
fn rename_value(buf: &str) -> Option<String> {
    let trimmed = buf.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Context menu for a ring zone chip: "Unroll zone" / "Roll zone".
pub(super) fn ring_zone_context_menu(ui: &mut egui::Ui, cmd: &CommandTx, z: &PlacedZone) {
    ui.set_min_width(140.0);
    let unrolled = z.sampling_mode == SamplingMode::Unrolled;
    let label = if unrolled {
        t!("canvas.roll_zone")
    } else {
        t!("canvas.unroll_zone")
    };
    if widgets::context_menu_item(ui, &label, theme::TEXT).clicked() {
        unroll_cmd(cmd, z, unrolled);
        ui.close();
    }
}

/// Zones driven by layer `id`: its own assignments, plus the unassigned ones
/// when it is the base layer.
fn layer_zones<'a>(state: &'a TopicStore, id: &str) -> Vec<&'a PlacedZone> {
    let is_default = state.lighting.canvas.default_effect.as_deref() == Some(id);
    state
        .lighting
        .canvas
        .placed_zones
        .iter()
        .filter(|z| match z.effect.as_deref() {
            Some(e) => e == id,
            None => is_default,
        })
        .collect()
}

fn zone_count_label(n: usize) -> String {
    let word = if n == 1 {
        t!("canvas.unit_zone")
    } else {
        t!("canvas.unit_zones")
    };
    format!("{n} {word}")
}

fn effect_name<'a>(state: &'a TopicStore, def: &'a EffectDef) -> std::borrow::Cow<'a, str> {
    match state
        .lighting
        .canvas
        .available_effects
        .iter()
        .find(|e| e.id == def.effect_id)
    {
        Some(e) => std::borrow::Cow::Borrowed(e.name.as_str()),
        None if def.effect_id == DESIGNER_PIXMAP_EFFECT_ID => t!("canvas.designer"),
        None => std::borrow::Cow::Borrowed(def.effect_id.as_str()),
    }
}

/// Distinct device names across `zones`, in first-seen order.
fn target_devices(state: &TopicStore, zones: &[&PlacedZone]) -> String {
    let mut seen = HashSet::new();
    zones
        .iter()
        .filter(|z| seen.insert(z.device_id.as_str()))
        .map(|z| device_name(state, &z.device_id))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn swatch(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(28.0, 16.0), Sense::hover());
    ui.painter().rect_filled(rect, 3.0, color);
}

/// The base layer: a dropdown selecting the instance unassigned zones use.
fn default_effect_card(
    ui: &mut egui::Ui,
    effects: &HashMap<String, EffectDef>,
    ids: &[String],
    current: Option<String>,
    cmd: &CommandTx,
) {
    widgets::card_with_margin(ui, PANEL_PAD, |ui| {
        ui.spacing_mut().item_spacing.y = theme::SPACE_5;
        ui.label(
            egui::RichText::new(t!("canvas.default_effect_title"))
                .font(theme::heading())
                .color(theme::TEXT),
        );
        let opts: Vec<(String, String)> = ids
            .iter()
            .map(|id| (id.clone(), instance_name(effects, id).to_string()))
            .collect();
        let selected = widgets::combo_picker_full(
            ui,
            "canvas_default_effect",
            &opts,
            current.as_deref().unwrap_or(""),
            Some(&t!("canvas.off")),
        )
        .map(|id| (!id.is_empty()).then_some(id))
        .unwrap_or_else(|| current.clone());
        if selected != current {
            crate::runtime::ipc::send(
                cmd,
                DaemonCommand::CanvasSetDefaultEffect {
                    instance_id: selected,
                },
            );
        }
        ui.label(
            egui::RichText::new(t!("canvas.default_effect_hint"))
                .font(theme::body_sm())
                .color(theme::TEXT_MUT),
        );
    });
}

/// The layer list: one row per instance with its targets and the two drill-in
/// actions (zones modal, parameters view).
fn layers_card(ui: &mut egui::Ui, state: &TopicStore, canvas_ui: &mut CanvasUi, ids: &[String]) {
    widgets::card_with_margin(ui, PANEL_PAD, |ui| {
        ui.spacing_mut().item_spacing.y = theme::SPACE_5;
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.label(
                    egui::RichText::new(t!("canvas.effect_instances"))
                        .font(theme::heading())
                        .color(theme::TEXT),
                );
            },
            |ui| {
                if widgets::button(
                    ui,
                    &t!("canvas.new_button"),
                    widgets::ButtonKind::Secondary,
                    egui::vec2(0.0, 30.0),
                )
                .clicked()
                {
                    canvas_ui.new_instance_modal = true;
                }
            },
        );
        if ids.is_empty() {
            ui.label(
                egui::RichText::new(t!("canvas.no_instances"))
                    .font(theme::body_sm())
                    .color(theme::TEXT_MUT),
            );
            return;
        }
        for (idx, id) in ids.iter().enumerate() {
            let def = &state.lighting.canvas.effects[id];
            let zones = layer_zones(state, id);
            widgets::inset(ui, egui::Margin::same(12), |ui| {
                ui.spacing_mut().item_spacing.y = theme::SPACE_5;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = theme::SPACE_5;
                    swatch(ui, instance_color(idx));
                    ui.label(
                        egui::RichText::new(instance_name(&state.lighting.canvas.effects, id))
                            .font(theme::medium(14.0))
                            .color(theme::TEXT),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        widgets::tag(
                            ui,
                            &zone_count_label(zones.len()),
                            widgets::TagTone::Neutral,
                        );
                    });
                });
                let targets = target_devices(state, &zones);
                ui.label(
                    egui::RichText::new(if targets.is_empty() {
                        effect_name(state, def).into_owned()
                    } else {
                        targets
                    })
                    .font(theme::body_sm())
                    .color(theme::TEXT_DIM),
                );
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = theme::SPACE_3;
                    let size = egui::vec2(0.0, 30.0);
                    let kind = widgets::ButtonKind::Secondary;
                    if widgets::button(ui, &t!("canvas.edit_zones"), kind, size).clicked() {
                        canvas_ui.zones_modal = Some(id.clone());
                    }
                    if widgets::button(ui, &t!("canvas.parameters"), kind, size).clicked() {
                        canvas_ui.selected_instance = Some(id.clone());
                    }
                });
            });
        }
    });
}

/// The parameters view for one layer, replacing the sidebar: effect picker,
/// its params, and the layer actions. The layer name renames inline on click.
#[allow(clippy::too_many_arguments)]
fn layer_params(
    ui: &mut egui::Ui,
    state: &TopicStore,
    canvas_ui: &mut CanvasUi,
    cmd: &CommandTx,
    time: f64,
    id: &str,
    idx: usize,
    designer_ui: &mut DesignerUi,
    page: &mut Page,
) {
    let Some(def) = state.lighting.canvas.effects.get(id) else {
        return;
    };
    let eff = state
        .lighting
        .canvas
        .available_effects
        .iter()
        .find(|e| e.id == def.effect_id);
    let zone_count = layer_zones(state, id).len();
    // Tracks the name across a same-frame rename commit so a param edit or
    // effect switch below can't resend the pre-rename value from `def`.
    let mut effective_name = def.name.clone();

    widgets::card_with_margin(ui, PANEL_PAD, |ui| {
        ui.spacing_mut().item_spacing.y = theme::SPACE_7;
        if widgets::back_link(ui, &t!("canvas.effect_instances"), None) {
            canvas_ui.selected_instance = None;
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = theme::SPACE_5;
            swatch(ui, instance_color(idx));
            if canvas_ui.rename_instance.as_deref() == Some(id) {
                let te = ui.add(
                    egui::TextEdit::singleline(&mut canvas_ui.rename_buf)
                        .font(theme::title())
                        .desired_width(ui.available_width() - 90.0),
                );
                if canvas_ui.rename_just_started {
                    te.request_focus();
                    canvas_ui.rename_just_started = false;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    canvas_ui.rename_instance = None;
                } else if te.lost_focus() {
                    // Blank (or exactly the id) clears the name back to the id.
                    let new_name = rename_value(&canvas_ui.rename_buf).filter(|n| n != id);
                    if new_name != def.name {
                        effective_name = new_name.clone();
                        // Fold in buffered edits and drop the pending upsert
                        // (built with the old name) so it can't undo the rename.
                        let params = eff
                            .map(|e| build_instance_params(e, def, id, canvas_ui))
                            .unwrap_or_else(|| def.params.clone());
                        canvas_ui.pending.remove(&super::effect_key(id));
                        crate::runtime::ipc::send(
                            cmd,
                            upsert_instance_cmd(
                                id.to_string(),
                                def.effect_id.clone(),
                                new_name,
                                params,
                            ),
                        );
                    }
                    canvas_ui.rename_instance = None;
                }
            } else {
                let name = instance_name(&state.lighting.canvas.effects, id);
                let resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new(name)
                            .font(theme::title())
                            .color(theme::TEXT),
                    )
                    .sense(Sense::click()),
                );
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
                }
                if resp.clicked() {
                    canvas_ui.rename_instance = Some(id.to_string());
                    canvas_ui.rename_just_started = true;
                    canvas_ui.rename_buf = name.to_string();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                widgets::tag(ui, &zone_count_label(zone_count), widgets::TagTone::Accent);
            });
        });

        ui.vertical(|ui| {
            widgets::field_label(ui, &t!("canvas.effect"));
            let opts: Vec<(String, String)> = state
                .lighting
                .canvas
                .available_effects
                .iter()
                .map(|e| (e.id.clone(), e.name.clone()))
                .collect();
            let sel =
                widgets::combo_picker_full(ui, ("inst_effect", id), &opts, &def.effect_id, None)
                    .unwrap_or_else(|| def.effect_id.clone());
            if sel != def.effect_id {
                canvas_ui.param_edits.remove(id);
                let params = state
                    .lighting
                    .canvas
                    .available_effects
                    .iter()
                    .find(|e| e.id == sel)
                    .map(|e| {
                        e.params
                            .iter()
                            .map(|p| (p.id.clone(), p.default.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                crate::runtime::ipc::send(
                    cmd,
                    upsert_instance_cmd(id.to_string(), sel, effective_name.clone(), params),
                );
            }
        });

        if let Some(eff) = eff {
            let changed = ui
                .vertical(|ui| instance_params(ui, canvas_ui, id, def, eff))
                .inner;
            if changed {
                let params = build_instance_params(eff, def, id, canvas_ui);
                canvas_ui.pending.queue(
                    &super::effect_key(id),
                    upsert_instance_cmd(
                        id.to_string(),
                        def.effect_id.clone(),
                        effective_name.clone(),
                        params,
                    ),
                    time,
                    DEBOUNCE,
                );
            }
        }

        let line_y = ui.cursor().top();
        ui.painter().hline(
            ui.max_rect().x_range(),
            line_y,
            egui::Stroke::new(1.0, theme::BORDER_SOFT),
        );
        ui.add_space(theme::SPACE_1);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(theme::SPACE_3, theme::SPACE_3);
            let size = egui::vec2(0.0, 30.0);
            let kind = widgets::ButtonKind::Secondary;
            if widgets::button(ui, &t!("canvas.edit_zones"), kind, size).clicked() {
                canvas_ui.zones_modal = Some(id.to_string());
            }
            if def.effect_id == DESIGNER_PIXMAP_EFFECT_ID
                && widgets::button(ui, &t!("canvas.edit_in_designer"), kind, size).clicked()
            {
                *designer_ui = DesignerUi::edit_for_canvas_instance(
                    def,
                    id.to_string(),
                    instance_name(&state.lighting.canvas.effects, id).to_string(),
                );
                *page = Page::EffectDesigner;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::button(
                    ui,
                    &t!("canvas.delete_instance"),
                    widgets::ButtonKind::Danger,
                    size,
                )
                .clicked()
                {
                    canvas_ui.selected_instance = None;
                    canvas_ui.param_edits.remove(id);
                    canvas_ui.rename_instance = None;
                    if canvas_ui.zones_modal.as_deref() == Some(id) {
                        canvas_ui.zones_modal = None;
                    }
                    crate::runtime::ipc::send(
                        cmd,
                        DaemonCommand::CanvasRemoveEffect {
                            instance_id: id.to_string(),
                        },
                    );
                }
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screens::canvas::test_fixtures::zone_with;

    #[test]
    fn base_layer_also_drives_unassigned_zones() {
        let mut s = TopicStore::default();
        s.lighting.canvas.placed_zones = vec![
            zone_with("a", Some("x")),
            zone_with("b", None),
            zone_with("c", Some("y")),
        ];
        let ids = |id: &str, s: &TopicStore| -> Vec<String> {
            layer_zones(s, id)
                .iter()
                .map(|z| z.channel_id.clone())
                .collect()
        };
        assert_eq!(ids("x", &s), ["a"]);
        s.lighting.canvas.default_effect = Some("x".into());
        assert_eq!(ids("x", &s), ["a", "b"]);
        assert_eq!(ids("y", &s), ["c"]);
        assert_eq!(target_devices(&s, &layer_zones(&s, "x")), "d");
    }

    #[test]
    fn instance_name_prefers_name_and_falls_back_to_id() {
        let mut effects: HashMap<String, EffectDef> = HashMap::new();
        effects.insert(
            "effect-1".into(),
            EffectDef {
                effect_id: "static_color".into(),
                name: Some("Desk glow".into()),
                params: HashMap::new(),
            },
        );
        effects.insert(
            "effect-2".into(),
            EffectDef {
                effect_id: "static_color".into(),
                name: Some("   ".into()),
                params: HashMap::new(),
            },
        );
        assert_eq!(instance_name(&effects, "effect-1"), "Desk glow");
        assert_eq!(instance_name(&effects, "effect-2"), "effect-2");
        assert_eq!(instance_name(&effects, "missing"), "missing");
    }

    #[test]
    fn prune_stale_instance_refs_clears_dead_selections_only() {
        let mut ui = CanvasUi {
            rename_instance: Some("dead".into()),
            ..Default::default()
        };
        ui.zones_modal = Some("dead".into());
        ui.selected_instance = Some("live".into());
        ui.param_edits.insert("dead".into(), HashMap::new());
        ui.param_edits.insert("live".into(), HashMap::new());
        ui.pending.queue(
            &super::super::effect_key("dead"),
            DaemonCommand::CanvasStop,
            0.0,
            0.0,
        );

        prune_stale_instance_refs(&mut ui, &["live".to_string()]);

        assert_eq!(ui.rename_instance, None);
        assert_eq!(ui.zones_modal, None);
        assert_eq!(ui.selected_instance, Some("live".to_string()));
        assert!(!ui.param_edits.contains_key("dead"));
        assert!(ui.param_edits.contains_key("live"));
        assert!(ui.pending.is_empty());
    }

    #[test]
    fn prune_stale_instance_refs_keeps_pending_for_live_instance() {
        let mut ui = CanvasUi::default();
        ui.pending.queue(
            &super::super::effect_key("live"),
            DaemonCommand::CanvasStop,
            0.0,
            0.0,
        );
        ui.pending.queue(
            super::super::SAMPLE_KEY,
            DaemonCommand::CanvasStop,
            0.0,
            0.0,
        );
        prune_stale_instance_refs(&mut ui, &["live".to_string()]);
        assert_eq!(ui.pending.len(), 2);
    }

    #[test]
    fn rename_value_trims_and_blanks_to_none() {
        assert_eq!(rename_value("  Desk glow "), Some("Desk glow".into()));
        assert_eq!(rename_value("   "), None);
        assert_eq!(rename_value(""), None);
    }

    #[test]
    fn instance_color_cycles_the_palette() {
        assert_eq!(instance_color(0), instance_color(INSTANCE_PALETTE.len()));
        assert_ne!(instance_color(0), instance_color(1));
    }
}
