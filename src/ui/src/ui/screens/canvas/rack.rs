// SPDX-License-Identifier: GPL-3.0-or-later
//! The right-hand sidebar: sampling-radius card and the effect-layer rack
//! (base layer, named layers with param editing and zone assignment).

use std::collections::{HashMap, HashSet};

use egui::{Align2, Color32, Pos2, Rect, Sense, Vec2};
use halod_shared::{
    commands::DaemonCommand,
    effect_designer::DESIGNER_PIXMAP_EFFECT_ID,
    types::{AppState, DeviceCapability, EffectDef, PlacedZone, SamplingMode, ZoneTopology},
};

use crate::runtime::ipc::CommandTx;
use crate::ui::components as widgets;
use crate::ui::theme;

use super::params::{build_instance_params, instance_params, unroll_cmd, upsert_instance_cmd};
use super::{CanvasUi, DEBOUNCE, MAX_ZONE_CHIPS};

pub(super) fn right_panel(
    ui: &mut egui::Ui,
    state: &AppState,
    canvas_ui: &mut CanvasUi,
    cmd: &CommandTx,
    time: f64,
    designer_ui: &mut crate::ui::screens::effect_designer::DesignerUi,
    page: &mut crate::domain::state::Page,
) {
    let rack = ui.scope(|ui| instance_rack(ui, state, canvas_ui, cmd, time, designer_ui, page));
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::CanvasInstanceRack,
        rack.response.rect,
    );
    ui.add_space(theme::SPACE_7);
    sampling_card(ui, state, canvas_ui, time);
}

// ── Sampling card ─────────────────────────────────────────────────────────────
/// Sampling-radius control: how large an area around each LED is averaged from
/// the canvas. Mirrors the GTK "Sampling Radius" control (0.5–32 px).
fn sampling_card(ui: &mut egui::Ui, state: &AppState, canvas_ui: &mut CanvasUi, time: f64) {
    // Re-seed from daemon state unless the user is mid-edit (mirrors LiveGuard).
    if time - canvas_ui.sample_edit_at >= 1.0 {
        canvas_ui.sample_radius = state.lighting.canvas.sample_radius;
    }
    widgets::card_titled(
        ui,
        &t!("canvas.sampling"),
        |_ui| {},
        |ui| {
            ui.label(
                egui::RichText::new(t!("canvas.sampling_desc").to_string())
                    .font(theme::body_sm())
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(theme::SPACE_5);
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
                canvas_ui.pending.sample = Some((
                    DaemonCommand::CanvasSetSampleRadius {
                        radius: snapped as f64,
                    },
                    time + DEBOUNCE,
                ));
            }
        },
    );
}

// ── Animation card ────────────────────────────────────────────────────────────
/// Distinct swatch colours per instance, indexed by rack position.
const INSTANCE_PALETTE: [u32; 8] = [
    0x5ad1e8, 0xfbbf24, 0xfb7185, 0x818cf8, 0x2dd4bf, 0xa3e635, 0xf97316, 0x22d3ee,
];

pub(super) fn instance_color(idx: usize) -> Color32 {
    theme::hex(INSTANCE_PALETTE[idx % INSTANCE_PALETTE.len()])
}

/// The swatch colour for a zone's resolved effect instance (its own, else the
/// canvas default), matching the rack. Gray when the zone resolves to nothing.
pub(super) fn instance_color_for(state: &AppState, effect: Option<&str>) -> Color32 {
    let Some(id) = effect.or(state.lighting.canvas.default_effect.as_deref()) else {
        return theme::hex(0x39414f);
    };
    instance_indices(state)
        .get(id)
        .map(|&i| instance_color(i))
        .unwrap_or(theme::hex(0x39414f))
}

pub(super) fn instance_indices(state: &AppState) -> HashMap<&str, usize> {
    let mut ids: Vec<&str> = state
        .lighting
        .canvas
        .effects
        .keys()
        .map(String::as_str)
        .collect();
    ids.sort_unstable();
    ids.into_iter().enumerate().map(|(i, id)| (id, i)).collect()
}

use crate::ui::components::truncate_to_width;

/// Clear any per-instance UI selection (rename target, edit-zones modal,
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
    if canvas_ui
        .pending
        .effect
        .as_ref()
        .is_some_and(|(id, _, _)| !is_live(id))
    {
        canvas_ui.pending.effect = None;
    }
}

/// An instance's display label: its user-set name, falling back to the id.
pub(super) fn instance_name<'a>(effects: &'a HashMap<String, EffectDef>, id: &'a str) -> &'a str {
    effects
        .get(id)
        .and_then(|d| d.name.as_deref())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(id)
}

fn device_name<'a>(state: &'a AppState, device_id: &'a str) -> &'a str {
    state
        .devices
        .iter()
        .find(|d| d.id == device_id)
        .map(|d| d.name.as_str())
        .unwrap_or(device_id)
}

fn zone_name<'a>(state: &'a AppState, device_id: &'a str, zone_id: &'a str) -> &'a str {
    state
        .devices
        .iter()
        .find(|d| d.id == device_id)
        .and_then(|d| {
            d.capabilities.iter().find_map(|c| match c {
                DeviceCapability::Rgb(r) => r.descriptor.zones.iter().find(|z| z.id == zone_id),
                _ => None,
            })
        })
        .map(|z| z.name.as_str())
        .unwrap_or(zone_id)
}

/// Lookup an `RgbZone` descriptor by device and zone id.
pub(super) fn rgb_zone_descriptor<'a>(
    state: &'a AppState,
    device_id: &str,
    zone_id: &str,
) -> Option<&'a halod_shared::types::RgbZone> {
    state
        .devices
        .iter()
        .find(|d| d.id == device_id)
        .and_then(|d| {
            d.capabilities.iter().find_map(|c| match c {
                DeviceCapability::Rgb(r) => r.descriptor.zones.iter().find(|z| z.id == zone_id),
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

/// Layer-first authoring: a rack of named pixmap-effect layers, each assigned
/// to zones; zones with no layer use the base effect.
fn instance_rack(
    ui: &mut egui::Ui,
    state: &AppState,
    canvas_ui: &mut CanvasUi,
    cmd: &CommandTx,
    time: f64,
    designer_ui: &mut crate::ui::screens::effect_designer::DesignerUi,
    page: &mut crate::domain::state::Page,
) {
    let effects = &state.lighting.canvas.effects;
    let mut ids: Vec<String> = effects.keys().cloned().collect();
    ids.sort();
    prune_stale_instance_refs(canvas_ui, &ids);

    // Widget 1: default / fallback effect dropdown.
    default_effect_card(
        ui,
        effects,
        &ids,
        state.lighting.canvas.default_effect.clone(),
        cmd,
    );
    ui.add_space(theme::SPACE_8);

    // "Effect instances" header + New — a bare row, outside any card. Each
    // instance below is then its own widget.
    egui::Sides::new().show(
        ui,
        |ui| {
            ui.label(
                egui::RichText::new(t!("canvas.effect_instances").to_string())
                    .font(theme::semibold(12.5))
                    .color(theme::TEXT),
            );
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("canvas.new_button"),
                widgets::ButtonKind::Ghost,
                egui::vec2(94.0, 24.0),
            )
            .clicked()
            {
                canvas_ui.new_instance_modal = true;
            }
        },
    );
    ui.add_space(theme::SPACE_5);

    if ids.is_empty() {
        ui.label(
            egui::RichText::new(t!("canvas.no_instances").to_string())
                .font(theme::body_sm())
                .color(theme::TEXT_FAINT),
        );
        return;
    }
    for (idx, id) in ids.iter().enumerate() {
        let def = &effects[id];
        widgets::card(ui, |ui| {
            instance_row(
                ui,
                state,
                canvas_ui,
                cmd,
                time,
                id,
                def,
                idx,
                designer_ui,
                page,
            );
        });
        ui.add_space(theme::SPACE_4);
    }
}

/// The default / fallback effect: a dropdown selecting the instance unassigned
/// zones use (and the canvas background shows).
fn default_effect_card(
    ui: &mut egui::Ui,
    effects: &HashMap<String, EffectDef>,
    ids: &[String],
    current: Option<String>,
    cmd: &CommandTx,
) {
    widgets::card_titled(
        ui,
        &t!("canvas.default_effect_title"),
        |_ui| {},
        |ui| {
            let mut selected = current.clone();
            let label = current
                .as_deref()
                .map(|id| instance_name(effects, id).to_string())
                .unwrap_or_else(|| t!("canvas.off").to_string());
            let w = ui.available_width();
            egui::ComboBox::from_id_salt("canvas_default_effect")
                .width(w)
                .selected_text(label)
                .show_ui(ui, |ui| {
                    ui.set_max_width(w);
                    ui.selectable_value(&mut selected, None, t!("canvas.off").to_string());
                    for id in ids {
                        ui.selectable_value(
                            &mut selected,
                            Some(id.clone()),
                            instance_name(effects, id),
                        );
                    }
                });
            if selected != current {
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::CanvasSetDefaultEffect {
                        instance_id: selected,
                    },
                );
            }
            ui.add_space(theme::SPACE_3);
            ui.label(
                egui::RichText::new(t!("canvas.default_effect_hint").to_string())
                    .font(theme::caption())
                    .color(theme::TEXT_FAINT),
            );
        },
    );
}

/// One instance card: swatch, name, effect + assigned-zone count; expands to an
/// effect picker, param sliders, per-zone assignment chips, and delete.
#[allow(clippy::too_many_arguments)]
fn instance_row(
    ui: &mut egui::Ui,
    state: &AppState,
    canvas_ui: &mut CanvasUi,
    cmd: &CommandTx,
    time: f64,
    id: &str,
    def: &EffectDef,
    idx: usize,
    designer_ui: &mut crate::ui::screens::effect_designer::DesignerUi,
    page: &mut crate::domain::state::Page,
) {
    let color = instance_color(idx);
    let expanded = canvas_ui.selected_instance.as_deref() == Some(id);
    let eff = state
        .lighting
        .canvas
        .available_effects
        .iter()
        .find(|e| e.id == def.effect_id);
    let eff_name: std::borrow::Cow<str> = match eff {
        Some(e) => std::borrow::Cow::Borrowed(e.name.as_str()),
        None if def.effect_id == DESIGNER_PIXMAP_EFFECT_ID => t!("canvas.designer"),
        None => std::borrow::Cow::Borrowed(def.effect_id.as_str()),
    };
    let is_default = state.lighting.canvas.default_effect.as_deref() == Some(id);
    let zones: Vec<&PlacedZone> = state
        .lighting
        .canvas
        .placed_zones
        .iter()
        .filter(|z| match z.effect.as_deref() {
            Some(e) => e == id,
            None => is_default,
        })
        .collect();
    let zone_count = zones.len();
    let device_count = zones
        .iter()
        .map(|z| z.device_id.as_str())
        .collect::<HashSet<_>>()
        .len();
    let synced = zone_count > 1;
    // Tracks the name across a same-frame rename commit so a param edit or
    // effect switch below can't resend the pre-rename value from `def`.
    let mut effective_name = def.name.clone();

    let (head, head_resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());
    let p = ui.painter().clone();
    let top_y = head.top() + 13.0;
    let bot_y = head.top() + 30.0;
    // Row 1: swatch, name, chevron.
    p.rect_filled(
        Rect::from_center_size(Pos2::new(head.left() + 11.0, top_y), Vec2::splat(13.0)),
        theme::RADIUS_XS,
        color,
    );
    p.text(
        Pos2::new(head.right() - 6.0, top_y),
        Align2::RIGHT_CENTER,
        if expanded { "▾" } else { "▸" },
        theme::micro(),
        theme::TEXT_FAINT,
    );
    // Row 2: effect · zones · devices, truncated to leave room for SYNCED.
    let sub_font = theme::value_xs();
    let synced_font = theme::mono(8.5);
    let synced_label = format!("⛓ {}", t!("canvas.synced"));
    let synced_w = if synced {
        p.layout_no_wrap(synced_label.clone(), synced_font.clone(), color)
            .size()
            .x
    } else {
        0.0
    };
    let sub_max_w =
        (head.right() - 6.0 - (head.left() + 26.0) - if synced { synced_w + 10.0 } else { 0.0 })
            .max(20.0);
    let zone_word = if zone_count == 1 {
        t!("canvas.unit_zone")
    } else {
        t!("canvas.unit_zones")
    };
    let device_word = if device_count == 1 {
        t!("canvas.unit_device")
    } else {
        t!("canvas.unit_devices")
    };
    let sub_full = format!("{eff_name} · {zone_count} {zone_word} · {device_count} {device_word}");
    let sub = truncate_to_width(&sub_full, sub_max_w, |s| {
        p.layout_no_wrap(s.to_string(), sub_font.clone(), theme::TEXT_FAINT)
            .size()
            .x
    });
    p.text(
        Pos2::new(head.left() + 26.0, bot_y),
        Align2::LEFT_CENTER,
        sub,
        sub_font,
        theme::TEXT_FAINT,
    );
    if synced {
        p.text(
            Pos2::new(head.right() - 6.0, bot_y),
            Align2::RIGHT_CENTER,
            &synced_label,
            synced_font,
            color,
        );
    }
    // Name area: inline rename editor when active, else painted name + pencil.
    let renaming = canvas_ui.rename_instance.as_deref() == Some(id);
    let mut pencil_clicked = false;
    if renaming {
        let left = head.left() + 22.0;
        let input_rect = Rect::from_min_size(
            Pos2::new(left, top_y - 12.0),
            Vec2::new((head.right() - 40.0 - left).max(120.0), 24.0),
        );
        let te_resp = ui.put(
            input_rect,
            egui::TextEdit::singleline(&mut canvas_ui.rename_buf)
                .font(theme::semibold(12.5))
                .margin(egui::Margin::symmetric(8, 4)),
        );
        if canvas_ui.rename_just_started {
            te_resp.request_focus();
            canvas_ui.rename_just_started = false;
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            canvas_ui.rename_instance = None;
        } else if te_resp.lost_focus() {
            // Blank (or exactly the id) clears the name back to the id.
            let new_name = rename_value(&canvas_ui.rename_buf).filter(|n| n != id);
            if new_name != def.name {
                effective_name = new_name.clone();
                // Fold in any buffered param edits and drop this instance's
                // pending upsert (built with the old name) so it can't
                // overwrite the rename; other instances' pending edits are
                // left untouched since the slot is tagged by instance id.
                let params = eff
                    .map(|e| build_instance_params(e, def, id, canvas_ui))
                    .unwrap_or_else(|| def.params.clone());
                if canvas_ui
                    .pending
                    .effect
                    .as_ref()
                    .is_some_and(|(pid, _, _)| pid == id)
                {
                    canvas_ui.pending.effect = None;
                }
                crate::runtime::ipc::send(
                    cmd,
                    upsert_instance_cmd(id.to_string(), def.effect_id.clone(), new_name, params),
                );
            }
            canvas_ui.rename_instance = None;
        }
    } else {
        let display = instance_name(&state.lighting.canvas.effects, id);
        let name_left = head.left() + 26.0;
        let reserved_right = 22.0;
        let max_name_w = (head.right() - reserved_right - name_left).max(20.0);
        let name_font = theme::semibold(12.5);
        let shown = truncate_to_width(display, max_name_w, |s| {
            p.layout_no_wrap(s.to_string(), name_font.clone(), theme::TEXT)
                .size()
                .x
        });
        let name_w = p
            .layout_no_wrap(shown.clone(), name_font.clone(), theme::TEXT)
            .size()
            .x;
        let pencil_rect = Rect::from_center_size(
            Pos2::new(
                (name_left + name_w + 16.0).min(head.right() - reserved_right + 8.0),
                top_y,
            ),
            Vec2::splat(20.0),
        );
        let pencil_resp = ui.interact(
            pencil_rect,
            egui::Id::new(("inst_rename_btn", id)),
            Sense::click(),
        );
        let t = ui
            .ctx()
            .animate_bool_with_time(pencil_resp.id, pencil_resp.hovered(), 0.12);
        let icon_col = theme::lerp_color(theme::TEXT_FAINT, theme::CYAN, t);
        if pencil_resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if pencil_resp.clicked() {
            pencil_clicked = true;
            canvas_ui.rename_instance = Some(id.to_string());
            canvas_ui.rename_just_started = true;
            canvas_ui.rename_buf = display.to_string();
        }
        p.text(
            Pos2::new(name_left, top_y),
            Align2::LEFT_CENTER,
            shown,
            name_font,
            theme::TEXT,
        );
        crate::ui::icons::draw_pencil(&p, pencil_rect.shrink(3.0), icon_col);
    }

    if head_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if head_resp.clicked() && !pencil_clicked && !renaming {
        canvas_ui.selected_instance = if expanded { None } else { Some(id.to_string()) };
    }
    if !expanded {
        return;
    }

    // Effect dropdown.
    ui.add_space(theme::SPACE_4);
    widgets::caps_label(ui, &t!("canvas.effect_caps"));
    ui.add_space(theme::SPACE_3);
    {
        let mut sel = def.effect_id.clone();
        let w = ui.available_width();
        egui::ComboBox::from_id_salt(("inst_effect", id))
            .width(w)
            .selected_text(eff_name.to_string())
            .show_ui(ui, |ui| {
                ui.set_max_width(w);
                for e in &state.lighting.canvas.available_effects {
                    ui.selectable_value(&mut sel, e.id.clone(), &e.name);
                }
            });
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
    }

    // Params (color, speed, …).
    if let Some(eff) = eff {
        if instance_params(ui, canvas_ui, id, def, eff) {
            let params = build_instance_params(eff, def, id, canvas_ui);
            canvas_ui.pending.effect = Some((
                id.to_string(),
                upsert_instance_cmd(
                    id.to_string(),
                    def.effect_id.clone(),
                    effective_name.clone(),
                    params,
                ),
                time + DEBOUNCE,
            ));
        }
    }

    // Assigned zones: chips only; assignment happens in the Edit zones modal.
    ui.add_space(theme::SPACE_5);
    egui::Sides::new().show(
        ui,
        |ui| widgets::caps_label(ui, &t!("canvas.assigned_zones")),
        |ui| {
            let link = ui.add(
                egui::Label::new(
                    egui::RichText::new(t!("canvas.edit_zones").to_string())
                        .font(theme::semibold(10.5))
                        .color(theme::CYAN),
                )
                .sense(Sense::click()),
            );
            if link.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if link.clicked() {
                canvas_ui.zones_modal = Some(id.to_string());
            }
        },
    );
    ui.add_space(theme::SPACE_3);
    if zones.is_empty() {
        if widgets::button(
            ui,
            &t!("canvas.add_zones"),
            widgets::ButtonKind::Ghost,
            egui::vec2(ui.available_width(), 34.0),
        )
        .clicked()
        {
            canvas_ui.zones_modal = Some(id.to_string());
        }
    } else {
        // Render up to MAX_ZONE_CHIPS chips inline; ring zones get a
        // right-click context menu for unroll/roll. Overflow folds into a
        // single "+N more" chip.
        let max = MAX_ZONE_CHIPS.min(zones.len());
        let shown = &zones[..max];
        let overflow = if zones.len() > max {
            Some(t!("canvas.overflow_more", n = zones.len() - max).to_string())
        } else {
            None
        };
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(5.0, 5.0);
            for z in shown {
                let label = format!(
                    "{} · {}",
                    device_name(state, &z.device_id),
                    zone_name(state, &z.device_id, &z.zone_id)
                );
                let is_ring = rgb_zone_descriptor(state, &z.device_id, &z.zone_id)
                    .map(|rz| {
                        matches!(rz.topology, ZoneTopology::Ring | ZoneTopology::Rings { .. })
                    })
                    .unwrap_or(false);
                let chip_resp = widgets::chip_colored(ui, &label, color);
                if is_ring {
                    chip_resp.context_menu(|ui| ring_zone_context_menu(ui, cmd, z));
                }
            }
            if let Some(more) = &overflow {
                widgets::chip(ui, more);
            }
        });
    }

    if def.effect_id == DESIGNER_PIXMAP_EFFECT_ID {
        ui.add_space(theme::SPACE_5);
        if widgets::button(
            ui,
            &t!("canvas.edit_in_designer"),
            widgets::ButtonKind::Ghost,
            egui::vec2(ui.available_width(), 30.0),
        )
        .clicked()
        {
            *designer_ui =
                crate::ui::screens::effect_designer::DesignerUi::edit_for_canvas_instance(
                    def,
                    id.to_string(),
                    instance_name(&state.lighting.canvas.effects, id).to_string(),
                );
            *page = crate::domain::state::Page::EffectDesigner;
        }
    }

    ui.add_space(theme::SPACE_5);
    if widgets::button(
        ui,
        &t!("canvas.delete_instance"),
        widgets::ButtonKind::Ghost,
        egui::vec2(ui.available_width(), 26.0),
    )
    .clicked()
    {
        canvas_ui.selected_instance = None;
        canvas_ui.param_edits.remove(id);
        if canvas_ui.rename_instance.as_deref() == Some(id) {
            canvas_ui.rename_instance = None;
        }
        if canvas_ui.zones_modal.as_deref() == Some(id) {
            canvas_ui.zones_modal = None;
        }
        crate::runtime::ipc::send(
            cmd,
            halod_shared::commands::DaemonCommand::CanvasRemoveEffect {
                instance_id: id.to_string(),
            },
        );
    }
}

// ── Settings card ─────────────────────────────────────────────────────────────
pub(super) fn toggle_switch(ui: &mut egui::Ui, on: bool, id: egui::Id) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(28.0, 15.0), Sense::click());
    let t = ui.ctx().animate_bool_with_time(id, on, 0.15);
    widgets::paint_toggle(ui.painter(), rect, t);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screens::canvas::test_fixtures::def_of;

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
        ui.pending.effect = Some(("dead".into(), DaemonCommand::CanvasStop, 0.0));

        prune_stale_instance_refs(&mut ui, &["live".to_string()]);

        assert_eq!(ui.rename_instance, None);
        assert_eq!(ui.zones_modal, None);
        assert_eq!(ui.selected_instance, Some("live".to_string()));
        assert!(!ui.param_edits.contains_key("dead"));
        assert!(ui.param_edits.contains_key("live"));
        assert!(ui.pending.effect.is_none());
    }

    #[test]
    fn prune_stale_instance_refs_keeps_pending_for_live_instance() {
        let mut ui = CanvasUi::default();
        ui.pending.effect = Some(("live".to_string(), DaemonCommand::CanvasStop, 0.0));
        prune_stale_instance_refs(&mut ui, &["live".to_string()]);
        assert!(ui.pending.effect.is_some());
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

    #[test]
    fn instance_color_for_matches_rack_index_and_grays_unknown() {
        let mut s = AppState::default();
        s.lighting
            .canvas
            .effects
            .insert("a".into(), def_of("static_color"));
        s.lighting
            .canvas
            .effects
            .insert("b".into(), def_of("rainbow"));
        // sorted ids: a=0, b=1 — same order the rack colours by.
        assert_eq!(instance_color_for(&s, Some("a")), instance_color(0));
        assert_eq!(instance_color_for(&s, Some("b")), instance_color(1));
        // unknown ref / nothing resolved → gray.
        assert_eq!(
            instance_color_for(&s, Some("missing")),
            theme::hex(0x39414f)
        );
        assert_eq!(instance_color_for(&s, None), theme::hex(0x39414f));
    }
}
