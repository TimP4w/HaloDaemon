// SPDX-License-Identifier: GPL-3.0-or-later
//! Keys/Buttons tab — per-button action remapping.
//!
//! Layout: left card (button selector list) + right column (assign action card
//! + parameters card). Matches the Prism Control design: the left card lets
//! the user pick which button to configure; the right panel assigns the action
//! category and then edits the category-specific parameters.

use crate::ui::components as widgets;
use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::commands::DaemonCommand;
use halod_shared::types::{
    ButtonAction, ButtonMapping, CycleDir, DeviceCapability, DeviceType, KeyRemapStatus,
    MediaAction, MouseBtn, ScrollAxis,
};

use super::{macro_editor, DeviceUi, TabCtx};
use crate::ui::theme::{self, a};

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::TabKeys,
        ui.max_rect(),
    );
    let Some(remap) = ctx.dev.capabilities.iter().find_map(|c| match c {
        DeviceCapability::KeyRemap(r) => Some(r.clone()),
        _ => None,
    }) else {
        return;
    };
    let id = ctx.dev.id.clone();

    if remap.requires_host_mode && !remap.host_mode_active {
        widgets::card(ui, |ui| {
            ui.label(
                egui::RichText::new(t!("device.keys_host_mode_required"))
                    .font(theme::body_md())
                    .color(theme::STAT_AMBER),
            );
        });
        ui.add_space(theme::SPACE_7);
    }

    // Default to first button
    if st.keys.keys_sel_cid.is_none() && !remap.buttons.is_empty() {
        st.keys.keys_sel_cid = Some(remap.buttons[0].cid);
    }

    // Wind down macro recording/drag state owned by a no-longer-selected
    // button before the buffers it relies on get re-seeded.
    macro_editor::sync_selection(ctx, st, &id);

    if let Some(cid) = st.keys.keys_sel_cid {
        let mapping = remap.mappings.iter().find(|m| m.cid == cid);
        let base = mapping.map(|m| m.base.clone()).unwrap_or_default();
        let shifted = mapping.map(|m| m.shifted.clone()).unwrap_or_default();
        sync_mapping_editor(&mut st.keys, cid, base, shifted);
    }

    // The list below stays the primary control; the picture is a shortcut.
    if let Some(status) = ctx.dev.keyboard_layout() {
        if !status.keys.is_empty() {
            keyboard_overview(ui, st, status);
            ui.add_space(theme::SPACE_7);
        }
    }

    let tab_label = if matches!(ctx.dev.device_type, DeviceType::Mouse) {
        t!("device.keys_tab_buttons")
    } else {
        t!("device.keys_tab_keys")
    };
    // ~58/42 split (matching the design), not the even split `ui.columns` gives.
    let gap = ui.spacing().item_spacing.x;
    let left_w = (ui.available_width() - gap) * 1.4 / 2.4;
    widgets::split_columns(ui, left_w, gap, |left, right| {
        button_selector_card(left, ctx, st, &remap, &id, &tab_label);
        action_card(right, ctx, st, &remap, &id);
        params_card(right, ctx, st, &id);
    });
}

/// Three-way synchronization between the last bus projection, the current bus
/// projection, and the editor buffers. A layer follows external changes only
/// while it is still equal to the projection it was seeded from.
fn sync_mapping_editor(
    keys: &mut super::KeysTab,
    cid: u16,
    base: ButtonAction,
    shifted: ButtonAction,
) {
    match keys.observed_mapping.take() {
        Some((observed_cid, old_base, old_shifted)) if observed_cid == cid => {
            if keys
                .keys_action
                .as_ref()
                .is_none_or(|local| *local == old_base)
            {
                keys.keys_action = Some(base.clone());
            }
            if keys
                .keys_shifted_action
                .as_ref()
                .is_none_or(|local| *local == old_shifted)
            {
                keys.keys_shifted_action = Some(shifted.clone());
            }
        }
        _ => {
            keys.keys_action = Some(base.clone());
            keys.keys_shifted_action = Some(shifted.clone());
        }
    }
    keys.observed_mapping = Some((cid, base, shifted));
}

// ── Keyboard overview (clickable key picture) ────────────────────────────────

/// Reduce a click on key `idx` to a new selected cid. A mapped key (one with a
/// KeyRemap cid) selects it; an unmapped key is a no-op (keeps `current`).
fn key_click_selection(
    keys: &[halod_shared::keyboard::VisualKey],
    idx: usize,
    current: Option<u16>,
) -> Option<u16> {
    match keys.get(idx).and_then(|k| k.remap_cid) {
        Some(cid) => Some(cid),
        None => current,
    }
}

/// The index of the key currently highlighted for `cid`, if any.
fn selected_key_index(
    keys: &[halod_shared::keyboard::VisualKey],
    cid: Option<u16>,
) -> Option<usize> {
    let cid = cid?;
    keys.iter().position(|k| k.remap_cid == Some(cid))
}

fn keyboard_overview(
    ui: &mut egui::Ui,
    st: &mut DeviceUi,
    status: &halod_shared::keyboard::KeyboardLayoutStatus,
) {
    use super::keyboard_visual as kbv;
    use std::collections::HashSet;

    let (resp, inner) = kbv::panel(ui, 240.0, Sense::click());
    let keys = &status.keys;
    let rects = kbv::key_rects(keys, inner, 3.0);
    let unit = kbv::unit_for(keys, inner);

    if resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            if let Some(i) = kbv::hit_key(keys, &rects, pos, unit) {
                st.keys.keys_sel_cid = key_click_selection(keys, i, st.keys.keys_sel_cid);
            }
        }
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let selected = selected_key_index(keys, st.keys.keys_sel_cid);
    let remappable: HashSet<u32> = keys
        .iter()
        .filter(|k| k.remap_cid.is_some())
        .map(|k| k.led_id)
        .collect();
    let (mapped, unmapped) = (theme::hex(0x28324a), theme::hex(0x141a24));
    kbv::draw_keyboard(
        ui,
        keys,
        &rects,
        &|id| {
            if remappable.contains(&id) {
                mapped
            } else {
                unmapped
            }
        },
        selected,
        status.language,
        unit,
    );
}

// ── Left card: button selector ───────────────────────────────────────────────

fn button_selector_card(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    remap: &KeyRemapStatus,
    id: &str,
    tab_label: &str,
) {
    widgets::card_frameless(ui, |ui| {
        let hdr_h = 50.0;
        let (hdr, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), hdr_h), Sense::hover());
        let p = ui.painter();
        p.text(
            Pos2::new(hdr.left() + 18.0, hdr.center().y - 8.0),
            Align2::LEFT_CENTER,
            tab_label,
            theme::heading(),
            theme::TEXT,
        );
        p.text(
            Pos2::new(hdr.left() + 18.0, hdr.center().y + 8.0),
            Align2::LEFT_CENTER,
            t!("device.keys_select_button_hint"),
            theme::body_sm(),
            theme::TEXT_FAINT,
        );

        // "Reset all" ghost button — right-aligned in header
        let btn_sz = Vec2::new(82.0, 28.0);
        let btn_rect = Rect::from_min_size(
            Pos2::new(
                hdr.right() - btn_sz.x - 18.0,
                hdr.center().y - btn_sz.y / 2.0,
            ),
            btn_sz,
        );
        let reset = ui
            .scope_builder(egui::UiBuilder::new().max_rect(btn_rect), |ui| {
                widgets::button(
                    ui,
                    &t!("device.keys_reset_all"),
                    widgets::ButtonKind::Ghost,
                    btn_sz,
                )
            })
            .inner;
        if reset.clicked() {
            crate::runtime::ipc::send(
                ctx.cmd,
                halod_shared::commands::DaemonCommand::ResetAllButtonMappings {
                    id: id.to_string(),
                },
            );
        }

        // Header hairline
        let p = ui.painter();
        p.line_segment(
            [hdr.left_bottom(), hdr.right_bottom()],
            Stroke::new(1.0, theme::BORDER_SOFT),
        );

        // ── Button rows ───────────────────────────────────────────────
        let last_cid = remap.buttons.last().map(|b| b.cid);
        for btn in &remap.buttons {
            let action = remap
                .mappings
                .iter()
                .find(|m| m.cid == btn.cid)
                .map(|m| m.base.clone())
                .unwrap_or_default();
            let ActionDisplay {
                tag,
                label,
                color: type_color,
                ..
            } = action_display(&action);
            let selected = st.keys.keys_sel_cid == Some(btn.cid);

            let (rect, resp) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 48.0), Sense::click());
            let p = ui.painter();

            // Row background + left accent bar
            if selected {
                p.rect_filled(rect, 0.0, theme::ROW_ACTIVE);
                p.rect_filled(
                    Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height())),
                    0.0,
                    type_color,
                );
            } else if resp.hovered() {
                p.rect_filled(rect, 0.0, a(Color32::WHITE, 0.025));
            }

            // Key label chip — width grows to fit longer labels
            let label_g = p.layout_no_wrap(
                btn.label.clone(),
                theme::semibold(11.0),
                if selected {
                    Color32::WHITE
                } else {
                    theme::TEXT_BRIGHT
                },
            );
            let chip_w = chip_width(label_g.size().x);
            let chip = Rect::from_min_size(
                Pos2::new(rect.left() + 18.0, rect.center().y - 15.0),
                Vec2::new(chip_w, 30.0),
            );
            p.rect_filled(chip, theme::RADIUS_SM, theme::INNER_BG);
            p.rect_stroke(
                chip,
                theme::RADIUS_SM,
                Stroke::new(1.0, if selected { type_color } else { theme::BORDER }),
                egui::StrokeKind::Middle,
            );
            p.galley(
                Pos2::new(
                    chip.center().x - label_g.size().x / 2.0,
                    chip.center().y - label_g.size().y / 2.0,
                ),
                label_g,
                Color32::WHITE,
            );

            // Action name
            p.text(
                Pos2::new(chip.right() + 12.0, rect.center().y),
                Align2::LEFT_CENTER,
                label,
                theme::body_md(),
                if selected {
                    theme::TEXT
                } else {
                    theme::TEXT_DIM
                },
            );

            // Tag chip (right edge)
            let tag_g = p.layout_no_wrap(tag.to_string(), theme::caption(), type_color);
            let tag_w = tag_g.size().x + 18.0;
            let tag_rect = Rect::from_min_size(
                Pos2::new(rect.right() - tag_w - 18.0, rect.center().y - 11.0),
                Vec2::new(tag_w, 22.0),
            );
            p.rect_stroke(
                tag_rect,
                theme::RADIUS_SM,
                Stroke::new(1.0, theme::BORDER),
                egui::StrokeKind::Middle,
            );
            p.galley(
                Pos2::new(
                    tag_rect.center().x - tag_g.size().x / 2.0,
                    tag_rect.center().y - tag_g.size().y / 2.0,
                ),
                tag_g,
                type_color,
            );

            // Hairline separator (skip on last row)
            if Some(btn.cid) != last_cid {
                p.line_segment(
                    [rect.left_bottom(), rect.right_bottom()],
                    Stroke::new(1.0, theme::BORDER_SOFT),
                );
            }

            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() {
                st.keys.keys_sel_cid = Some(btn.cid);
                // Don't update last_edit: allow the re-seed logic to immediately
                // pick up the new button's daemon action on the next frame.
            }
        }
    });
}

// ── Which layer a section edits ──────────────────────────────────────────────

/// A button mapping has two independent slots: the plain press (`Base`) and
/// what it does while the layer-shift key is held (`Shifted`). Both are edited
/// side by side so the user can see the whole mapping at once.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Layer {
    Base,
    Shifted,
}

impl Layer {
    fn title(self) -> std::borrow::Cow<'static, str> {
        match self {
            Layer::Base => t!("device.keys_layer_normal"),
            Layer::Shifted => t!("device.keys_layer_shift"),
        }
    }

    pub(super) fn from_shifted(shifted: bool) -> Self {
        if shifted {
            Layer::Shifted
        } else {
            Layer::Base
        }
    }

    pub(super) fn get(self, st: &DeviceUi) -> Option<ButtonAction> {
        match self {
            Layer::Base => st.keys.keys_action.clone(),
            Layer::Shifted => st.keys.keys_shifted_action.clone(),
        }
    }

    pub(super) fn set(self, st: &mut DeviceUi, action: ButtonAction) {
        match self {
            Layer::Base => st.keys.keys_action = Some(action),
            Layer::Shifted => st.keys.keys_shifted_action = Some(action),
        }
    }

    /// Build the `(base, shifted)` pair for a `SetButtonMapping` command:
    /// `action` goes in this layer's slot, the other layer keeps its current
    /// buffered value.
    pub(super) fn pair(self, st: &DeviceUi, action: ButtonAction) -> (ButtonAction, ButtonAction) {
        match self {
            Layer::Base => (
                action,
                st.keys.keys_shifted_action.clone().unwrap_or_default(),
            ),
            Layer::Shifted => (st.keys.keys_action.clone().unwrap_or_default(), action),
        }
    }

    /// Disambiguates scratch/debounce/widget-id keys so base and shifted
    /// edits for the same button don't collide.
    pub(super) fn tag(self) -> &'static str {
        match self {
            Layer::Base => "base",
            Layer::Shifted => "shifted",
        }
    }

    /// Whether a category prototype is assignable to this layer. A shifted
    /// mapping can't itself be another layer-shift key.
    fn allows(self, proto: &ButtonAction) -> bool {
        !(self == Layer::Shifted && matches!(proto, ButtonAction::LayerShift))
    }
}

// ── Right card 1: Assign action ──────────────────────────────────────────────

fn action_card(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    remap: &KeyRemapStatus,
    id: &str,
) {
    let sel_cid = st.keys.keys_sel_cid;
    let sel_label = sel_cid
        .and_then(|cid| remap.buttons.iter().find(|b| b.cid == cid))
        .map(|b| b.label.as_str())
        .unwrap_or("-");

    widgets::card(ui, |ui| {
        // ── Card header ───────────────────────────────────────────────────
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.label(
                    egui::RichText::new(t!("device.keys_assign_action"))
                        .font(theme::heading())
                        .color(theme::TEXT),
                );
            },
            |ui| {
                // Selected button chip
                let g = ui.painter().layout_no_wrap(
                    sel_label.to_string(),
                    theme::semibold(11.0),
                    Color32::WHITE,
                );
                let (chip, _) =
                    ui.allocate_exact_size(Vec2::new(g.size().x + 18.0, 26.0), Sense::hover());
                ui.painter()
                    .rect_filled(chip, theme::RADIUS_SM, theme::INNER_BG);
                ui.painter().rect_stroke(
                    chip,
                    theme::RADIUS_SM,
                    Stroke::new(1.0, theme::hex(0x2a3446)),
                    egui::StrokeKind::Middle,
                );
                ui.painter().galley(
                    Pos2::new(
                        chip.center().x - g.size().x / 2.0,
                        chip.center().y - g.size().y / 2.0,
                    ),
                    g,
                    Color32::WHITE,
                );
                ui.add_space(theme::SPACE_3);
                ui.label(
                    egui::RichText::new(t!("device.keys_editing"))
                        .font(theme::micro())
                        .color(theme::TEXT_FAINT2),
                );
            },
        );
        ui.add_space(theme::SPACE_3);
        ui.label(
            egui::RichText::new(t!("device.keys_assign_hint", button = sel_label))
                .font(theme::body_sm())
                .color(theme::TEXT_MUT),
        );
        ui.add_space(theme::SPACE_7);

        action_section(ui, ctx, st, id, Layer::Base);
        ui.add_space(theme::SPACE_8);
        action_section(ui, ctx, st, id, Layer::Shifted);
    });
}

/// One layer's category-chip row within the "Assign action" card.
fn action_section(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, id: &str, layer: Layer) {
    let Some(cid) = st.keys.keys_sel_cid else {
        return;
    };
    let current_action = layer.get(st).unwrap_or_default();

    widgets::caps_label(ui, &layer.title());
    ui.add_space(theme::SPACE_4);
    let cat_rect = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), 30.0));
    match layer {
        Layer::Base => {
            crate::domain::tour::anchor(
                ui.ctx(),
                crate::domain::tour::AnchorId::KeysActionCategory,
                cat_rect,
            );
        }
        Layer::Shifted => {
            crate::domain::tour::anchor(
                ui.ctx(),
                crate::domain::tour::AnchorId::KeysLayerShift,
                cat_rect,
            );
        }
    }
    widgets::pill_strip(ui, |ui| {
        for proto in action_categories() {
            if !layer.allows(proto) {
                continue;
            }
            let active = same_category(&current_action, proto);
            let ActionDisplay {
                type_name, color, ..
            } = action_display(proto);
            if action_chip(ui, &type_name, active, color) {
                let new_action = adapt_to_category(proto.clone(), &current_action);
                let (base, shifted) = layer.pair(st, new_action.clone());
                layer.set(st, new_action);
                st.last_edit = ctx.time;
                if mapping_is_complete(&base, &shifted) {
                    crate::runtime::ipc::send(
                        ctx.cmd,
                        DaemonCommand::SetButtonMapping {
                            id: id.to_string(),
                            mapping: ButtonMapping { cid, base, shifted },
                        },
                    );
                }
            }
        }
    });
}

// ── Right card 2: Parameters ─────────────────────────────────────────────────

fn params_card(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, id: &str) {
    widgets::card(ui, |ui| {
        if st.keys.keys_sel_cid.is_none() {
            ui.label(
                egui::RichText::new(t!("device.keys_select_button_prompt"))
                    .font(theme::body_md())
                    .color(theme::TEXT_FAINT),
            );
            return;
        }

        params_section(ui, ctx, st, id, Layer::Base);
        ui.add_space(theme::SPACE_9);
        params_section(ui, ctx, st, id, Layer::Shifted);
    });
}

/// One layer's header + parameter editor within the "Parameters" card.
fn params_section(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, id: &str, layer: Layer) {
    let action = layer.get(st).unwrap_or_default();
    let ActionDisplay {
        type_name,
        color: type_color,
        ..
    } = action_display(&action);

    ui.horizontal(|ui| {
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
        ui.painter().circle_filled(dot.center(), 3.5, type_color);
        ui.label(
            egui::RichText::new(layer.title())
                .font(theme::heading())
                .color(theme::TEXT),
        );
        ui.add_space(theme::SPACE_1);
        ui.label(
            egui::RichText::new(format!("· {type_name}"))
                .font(theme::body_sm())
                .color(theme::TEXT_FAINT),
        );
    });
    ui.add_space(theme::SPACE_7);

    let no_params = match &action {
        ButtonAction::Native => Some(t!("device.keys_no_params_native")),
        ButtonAction::Disable => Some(t!("device.keys_no_params_disable")),
        ButtonAction::LayerShift => Some(t!("device.keys_no_params_layer_shift")),
        ButtonAction::KeyChord { .. } => Some(t!("device.keys_no_params")),
        _ => None,
    };

    if let Some(msg) = no_params {
        ui.label(
            egui::RichText::new(msg)
                .font(theme::body_md())
                .color(theme::TEXT_MUT),
        );
        return;
    }

    params_editor(ui, ctx, st, id, action, layer);
}

fn direction_pills(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    layer: Layer,
    current: CycleDir,
    make_cmd: &impl Fn(ButtonAction) -> DaemonCommand,
    make_action: impl Fn(CycleDir) -> ButtonAction,
) {
    widgets::caps_label(ui, &t!("device.keys_direction"));
    ui.add_space(theme::SPACE_4);
    widgets::pill_strip(ui, |ui| {
        for (label, dir) in [
            (t!("device.keys_dir_up"), CycleDir::Up),
            (t!("device.keys_dir_down"), CycleDir::Down),
        ] {
            let active = current == dir;
            if widgets::pill(ui, &label, active) && !active {
                let a = make_action(dir);
                layer.set(st, a.clone());
                st.last_edit = ctx.time;
                crate::runtime::ipc::send(ctx.cmd, make_cmd(a));
            }
        }
    });
}

fn params_editor(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    action: ButtonAction,
    layer: Layer,
) {
    let Some(cid) = st.keys.keys_sel_cid else {
        return;
    };
    let other = match layer {
        Layer::Base => st.keys.keys_shifted_action.clone().unwrap_or_default(),
        Layer::Shifted => st.keys.keys_action.clone().unwrap_or_default(),
    };
    let make_cmd = |act: ButtonAction| {
        let (base, shifted) = match layer {
            Layer::Base => (act, other.clone()),
            Layer::Shifted => (other.clone(), act),
        };
        DaemonCommand::SetButtonMapping {
            id: id.to_string(),
            mapping: ButtonMapping { cid, base, shifted },
        }
    };
    let tag = layer.tag();
    // Apply an edited action: seed the layer buffer, then send now (`apply`)
    // or debounce under a per-field key (`queue`).
    let apply = |st: &mut DeviceUi, a: ButtonAction| {
        layer.set(st, a.clone());
        st.last_edit = ctx.time;
        let cmd = make_cmd(a);
        if let DaemonCommand::SetButtonMapping { mapping, .. } = &cmd {
            if mapping_is_complete(&mapping.base, &mapping.shifted) {
                crate::runtime::ipc::send(ctx.cmd, cmd);
            }
        }
    };
    let queue = |st: &mut DeviceUi, field: &str, a: ButtonAction| {
        layer.set(st, a.clone());
        let cmd = make_cmd(a);
        if let DaemonCommand::SetButtonMapping { mapping, .. } = &cmd {
            if mapping_is_complete(&mapping.base, &mapping.shifted) {
                st.queue(&format!("btn:{field}:{cid}:{tag}"), cmd, ctx.time);
            }
        }
    };

    match action {
        ButtonAction::MouseButton { btn } => {
            widgets::caps_label(ui, &t!("device.keys_mouse_button"));
            ui.add_space(theme::SPACE_4);
            widgets::pill_strip(ui, |ui| {
                for (label, mbtn) in [
                    (t!("device.keys_mouse_left"), MouseBtn::Left),
                    (t!("device.keys_mouse_right"), MouseBtn::Right),
                    (t!("device.keys_mouse_middle"), MouseBtn::Middle),
                    (t!("device.keys_mouse_back"), MouseBtn::Back),
                    (t!("device.keys_mouse_forward"), MouseBtn::Forward),
                ] {
                    let active = btn == mbtn;
                    if widgets::pill(ui, &label, active) && !active {
                        apply(st, ButtonAction::MouseButton { btn: mbtn });
                    }
                }
            });
        }
        ButtonAction::DpiCycle { direction } => {
            direction_pills(ui, ctx, st, layer, direction, &make_cmd, |dir| {
                ButtonAction::DpiCycle { direction: dir }
            });
        }
        ButtonAction::ProfileCycle { direction } => {
            direction_pills(ui, ctx, st, layer, direction, &make_cmd, |dir| {
                ButtonAction::ProfileCycle { direction: dir }
            });
        }
        ButtonAction::MomentaryDpi { dpi } => {
            widgets::caps_label(ui, &t!("device.keys_dpi_value"));
            ui.add_space(theme::SPACE_4);
            let mut dpi_f = dpi as f32;
            let mut new_dpi: Option<u16> = None;
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.label(
                        egui::RichText::new(t!("device.keys_sniper_dpi"))
                            .font(theme::body_md())
                            .color(theme::TEXT_DIM),
                    );
                },
                |ui| {
                    let mut v = dpi as i32;
                    if ui
                        .add(egui::DragValue::new(&mut v).speed(50.0).range(100..=26000))
                        .changed()
                    {
                        new_dpi = Some(v.clamp(100, 26000) as u16);
                    }
                },
            );
            ui.add_space(theme::SPACE_4);
            if widgets::slider(ui, &mut dpi_f, 100.0..=26000.0) {
                new_dpi = Some((dpi_f as u16).clamp(100, 26000));
            }
            if let Some(new_dpi) = new_dpi {
                queue(st, "dpi", ButtonAction::MomentaryDpi { dpi: new_dpi });
            }
            ui.add_space(theme::SPACE_6);
            widgets::caps_label(ui, &t!("device.keys_presets"));
            ui.add_space(theme::SPACE_4);
            widgets::pill_strip(ui, |ui| {
                for preset in [400u16, 800, 1600, 3200, 6400] {
                    let active = dpi == preset;
                    if widgets::pill(ui, &preset.to_string(), active) && !active {
                        apply(st, ButtonAction::MomentaryDpi { dpi: preset });
                    }
                }
            });
        }
        ButtonAction::MediaKey { key } => {
            widgets::caps_label(ui, &t!("device.keys_media_key"));
            ui.add_space(theme::SPACE_4);
            widgets::pill_strip(ui, |ui| {
                for (label, mkey) in [
                    (t!("device.keys_media_play"), MediaAction::Play),
                    (t!("device.keys_media_next"), MediaAction::Next),
                    (t!("device.keys_media_prev"), MediaAction::Prev),
                    (t!("device.keys_media_mute"), MediaAction::Mute),
                ] {
                    let active = key == mkey;
                    if widgets::pill(ui, &label, active) && !active {
                        apply(st, ButtonAction::MediaKey { key: mkey });
                    }
                }
            });
        }
        ButtonAction::Scroll { axis, clicks } => {
            widgets::caps_label(ui, &t!("device.keys_axis"));
            ui.add_space(theme::SPACE_4);
            widgets::pill_strip(ui, |ui| {
                for (label, saxis) in [
                    (t!("device.keys_axis_vertical"), ScrollAxis::Vertical),
                    (t!("device.keys_axis_horizontal"), ScrollAxis::Horizontal),
                ] {
                    let active = axis == saxis;
                    if widgets::pill(ui, &label, active) && !active {
                        apply(
                            st,
                            ButtonAction::Scroll {
                                axis: saxis,
                                clicks,
                            },
                        );
                    }
                }
            });
            ui.add_space(theme::SPACE_6);
            widgets::caps_label(ui, &t!("device.keys_clicks_per_event"));
            ui.add_space(theme::SPACE_4);
            let mut v = clicks;
            if ui
                .add(egui::DragValue::new(&mut v).speed(1).range(-10..=10))
                .changed()
            {
                queue(st, "scroll", ButtonAction::Scroll { axis, clicks: v });
            }
        }
        ButtonAction::OpenApp { ref path } => {
            widgets::caps_label(ui, &t!("device.keys_application"));
            ui.add_space(theme::SPACE_4);
            let mut buf = path.clone();
            let shifted = matches!(layer, Layer::Shifted);

            // A finished native pick for this button/layer replaces the path.
            let recv = st
                .keys
                .app_picker
                .as_ref()
                .filter(|(c, s, _)| *c == cid && *s == shifted)
                .and_then(|(_, _, rx)| rx.try_recv().ok());
            if let Some(picked) = recv {
                st.keys.app_picker = None;
                if let Some(a) = open_app_from_pick(picked) {
                    if let ButtonAction::OpenApp { path } = &a {
                        buf = path.clone();
                    }
                    queue(st, "app", a);
                }
            }

            ui.horizontal(|ui| {
                let resp = action_text_field(
                    ui,
                    ui.available_width() - 92.0,
                    &mut buf,
                    &t!("device.keys_app_hint"),
                    ("openapp", cid, tag),
                );
                if resp.changed() {
                    queue(st, "app", ButtonAction::OpenApp { path: buf.clone() });
                }
                if widgets::pill(ui, &t!("device.keys_browse"), false) {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let egui_ctx = ui.ctx().clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(rfd::FileDialog::new().pick_file());
                        egui_ctx.request_repaint();
                    });
                    st.keys.app_picker = Some((cid, shifted, rx));
                }
            });
        }
        ButtonAction::Command { ref cmd, ref args } => {
            widgets::caps_label(ui, &t!("device.keys_command"));
            ui.add_space(theme::SPACE_4);
            let mut buf = cmd.clone();
            let resp = action_text_field(
                ui,
                ui.available_width(),
                &mut buf,
                &t!("device.keys_command_hint"),
                ("cmd", cid, tag),
            );
            if resp.changed() {
                queue(
                    st,
                    "cmd",
                    ButtonAction::Command {
                        cmd: buf,
                        args: args.clone(),
                    },
                );
            }
        }
        ButtonAction::Macro { steps } => {
            let macro_rect =
                Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), 28.0));
            crate::domain::tour::anchor(
                ui.ctx(),
                crate::domain::tour::AnchorId::KeysMacro,
                macro_rect,
            );
            macro_editor::show(ui, ctx, st, id, cid, layer, steps);
        }
        _ => {
            ui.label(
                egui::RichText::new(t!("device.keys_no_params"))
                    .font(theme::body_md())
                    .color(theme::TEXT_MUT),
            );
        }
    }
}

/// The 34 px free-text field both text-y actions (OpenApp, Command) share; a
/// stable id keeps focus across frames while the buffer is re-seeded.
fn action_text_field(
    ui: &mut egui::Ui,
    width: f32,
    buf: &mut String,
    hint: &str,
    id: impl std::hash::Hash + std::fmt::Debug,
) -> egui::Response {
    ui.add_sized(
        Vec2::new(width, 34.0),
        egui::TextEdit::singleline(buf)
            .hint_text(hint.to_string())
            .margin(egui::vec2(10.0, 8.0))
            .id(egui::Id::new(id)),
    )
}

// ── Action chip widget (colored active state) ────────────────────────────────

fn action_chip(ui: &mut egui::Ui, label: &str, active: bool, color: Color32) -> bool {
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        theme::body_sm(),
        if active {
            Color32::WHITE
        } else {
            theme::TEXT_DIM
        },
    );
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(galley.size().x + 24.0, 31.0), Sense::click());
    let p = ui.painter();
    if active {
        p.rect_filled(rect, 8.0, a(color, 0.16));
        p.rect_stroke(rect, 8.0, Stroke::new(1.0, color), egui::StrokeKind::Middle);
    } else {
        p.rect_stroke(
            rect,
            8.0,
            Stroke::new(1.0, theme::hex(0x222b3a)),
            egui::StrokeKind::Middle,
        );
    }
    p.galley(
        Pos2::new(
            rect.center().x - galley.size().x / 2.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        if active {
            Color32::WHITE
        } else {
            theme::TEXT_DIM
        },
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

// ── Data helpers ─────────────────────────────────────────────────────────────

/// Turn a native file-picker result into an Open-app action. A cancelled
/// dialog (`None`) leaves the existing mapping untouched.
fn open_app_from_pick(picked: Option<std::path::PathBuf>) -> Option<ButtonAction> {
    picked.map(|p| ButtonAction::OpenApp {
        path: p.to_string_lossy().into_owned(),
    })
}

/// One prototype `ButtonAction` per category chip in the action picker; the
/// params card handles sub-options. Display text comes from
/// `action_display()`, keyed off the variant, so it's always translated.
fn action_categories() -> &'static [ButtonAction] {
    static CATS: &[ButtonAction] = &[
        ButtonAction::Native,
        ButtonAction::Disable,
        ButtonAction::MouseButton {
            btn: MouseBtn::Left,
        },
        ButtonAction::DpiCycle {
            direction: CycleDir::Up,
        },
        ButtonAction::MomentaryDpi { dpi: 800 },
        ButtonAction::ProfileCycle {
            direction: CycleDir::Up,
        },
        ButtonAction::MediaKey {
            key: MediaAction::Play,
        },
        ButtonAction::LayerShift,
        ButtonAction::Scroll {
            axis: ScrollAxis::Vertical,
            clicks: 1,
        },
        ButtonAction::Macro { steps: Vec::new() },
        ButtonAction::OpenApp {
            path: String::new(),
        },
        ButtonAction::Command {
            cmd: String::new(),
            args: Vec::new(),
        },
    ];
    CATS
}

/// Whether `current` is the same action variant (category) as `proto`, ignoring
/// sub-values. Used to highlight the active chip in the action card.
fn same_category(current: &ButtonAction, proto: &ButtonAction) -> bool {
    std::mem::discriminant(current) == std::mem::discriminant(proto)
}

/// Build the new action for a category chip click, preserving any existing
/// sub-values when the category is unchanged.
fn adapt_to_category(proto: ButtonAction, current: &ButtonAction) -> ButtonAction {
    match (&proto, current) {
        (ButtonAction::MouseButton { .. }, ButtonAction::MouseButton { btn }) => {
            ButtonAction::MouseButton { btn: btn.clone() }
        }
        (ButtonAction::DpiCycle { .. }, ButtonAction::DpiCycle { direction }) => {
            ButtonAction::DpiCycle {
                direction: direction.clone(),
            }
        }
        (ButtonAction::ProfileCycle { .. }, ButtonAction::ProfileCycle { direction }) => {
            ButtonAction::ProfileCycle {
                direction: direction.clone(),
            }
        }
        (ButtonAction::MediaKey { .. }, ButtonAction::MediaKey { key }) => {
            ButtonAction::MediaKey { key: key.clone() }
        }
        (ButtonAction::MomentaryDpi { .. }, ButtonAction::MomentaryDpi { dpi }) => {
            ButtonAction::MomentaryDpi { dpi: *dpi }
        }
        (ButtonAction::Scroll { .. }, ButtonAction::Scroll { axis, clicks }) => {
            ButtonAction::Scroll {
                axis: axis.clone(),
                clicks: *clicks,
            }
        }
        (ButtonAction::Macro { .. }, ButtonAction::Macro { steps }) => ButtonAction::Macro {
            steps: steps.clone(),
        },
        (ButtonAction::OpenApp { .. }, ButtonAction::OpenApp { path }) => {
            ButtonAction::OpenApp { path: path.clone() }
        }
        (ButtonAction::Command { .. }, ButtonAction::Command { cmd, args }) => {
            ButtonAction::Command {
                cmd: cmd.clone(),
                args: args.clone(),
            }
        }
        _ => proto,
    }
}

pub(super) fn mapping_is_complete(base: &ButtonAction, shifted: &ButtonAction) -> bool {
    fn complete(action: &ButtonAction) -> bool {
        match action {
            ButtonAction::Macro { steps } => !steps.is_empty(),
            ButtonAction::OpenApp { path } => !path.is_empty(),
            ButtonAction::Command { cmd, .. } => !cmd.is_empty(),
            _ => true,
        }
    }
    complete(base) && complete(shifted)
}

/// Width of the key-label chip: grows to fit the label with side padding,
/// but never below the base width so short labels keep a uniform pill.
fn chip_width(label_w: f32) -> f32 {
    (label_w + 20.0).max(62.0)
}

/// Everything the UI shows for one action: the left-card row's short tag +
/// accent color, the Parameters card header's type name, and the row's full
/// label (type name plus any sub-value, e.g. "DPI cycle: Up").
struct ActionDisplay {
    tag: std::borrow::Cow<'static, str>,
    type_name: std::borrow::Cow<'static, str>,
    label: String,
    color: Color32,
}

fn action_display(a: &ButtonAction) -> ActionDisplay {
    let (tag, type_name, color) = match a {
        ButtonAction::Native => (
            t!("device.keys_tag_default"),
            t!("device.keys_type_default"),
            theme::TEXT_FAINT,
        ),
        ButtonAction::Disable => (
            t!("device.keys_tag_disabled"),
            t!("device.keys_type_disabled"),
            theme::STAT_AMBER,
        ),
        ButtonAction::MouseButton { .. } => (
            t!("device.keys_tag_mouse_btn"),
            t!("device.keys_type_mouse_button"),
            theme::CYAN,
        ),
        ButtonAction::Scroll { .. } => (
            t!("device.keys_tag_scroll"),
            t!("device.keys_type_scroll"),
            theme::hex(0x38bdf8),
        ),
        ButtonAction::KeyChord { .. } => (
            t!("device.keys_tag_key_chord"),
            t!("device.keys_type_key_chord"),
            theme::STAT_PURPLE,
        ),
        ButtonAction::MediaKey { .. } => (
            t!("device.keys_tag_media"),
            t!("device.keys_type_media_key"),
            theme::STAT_AMBER,
        ),
        ButtonAction::DpiCycle { .. } => (
            t!("device.keys_tag_dpi_cycle"),
            t!("device.keys_type_dpi_cycle"),
            theme::hex(0xf472b6),
        ),
        ButtonAction::ProfileCycle { .. } => (
            t!("device.keys_tag_profile"),
            t!("device.keys_type_profile_cycle"),
            theme::STAT_GREEN,
        ),
        ButtonAction::MomentaryDpi { .. } => (
            t!("device.keys_tag_dpi_snap"),
            t!("device.keys_type_momentary_dpi"),
            theme::hex(0xf472b6),
        ),
        ButtonAction::LayerShift => (
            t!("device.keys_tag_layer"),
            t!("device.keys_type_layer_shift"),
            theme::STAT_PURPLE,
        ),
        ButtonAction::Macro { .. } => (
            t!("device.keys_tag_macro"),
            t!("device.keys_type_macro"),
            theme::STAT_GREEN,
        ),
        ButtonAction::OpenApp { .. } => (
            t!("device.keys_tag_launch"),
            t!("device.keys_type_open_app"),
            theme::STAT_GREEN,
        ),
        ButtonAction::Command { .. } => (
            t!("device.keys_tag_command"),
            t!("device.keys_type_command"),
            theme::STAT_AMBER,
        ),
    };

    let label = match a {
        ButtonAction::MouseButton { btn } => {
            t!("device.keys_label_mouse_click", btn = format!("{btn:?}")).to_string()
        }
        ButtonAction::MediaKey { key } => match key {
            MediaAction::Play => t!("device.keys_media_play"),
            MediaAction::Next => t!("device.keys_media_next"),
            MediaAction::Prev => t!("device.keys_media_prev"),
            MediaAction::Mute => t!("device.keys_media_mute"),
            MediaAction::VolumeUp => t!("device.keys_media_volume_up"),
            MediaAction::VolumeDown => t!("device.keys_media_volume_down"),
        }
        .to_string(),
        ButtonAction::DpiCycle { direction } => t!(
            "device.keys_label_dpi_dir",
            direction = format!("{direction:?}")
        )
        .to_string(),
        ButtonAction::ProfileCycle { direction } => t!(
            "device.keys_label_profile_dir",
            direction = format!("{direction:?}")
        )
        .to_string(),
        ButtonAction::MomentaryDpi { dpi } => t!("device.keys_label_dpi", dpi = dpi).to_string(),
        _ => type_name.clone().into_owned(),
    };

    ActionDisplay {
        tag,
        type_name,
        label,
        color,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::keyboard::{KeyCell, KeyId, VisualKey};

    fn vkey(led_id: u32, remap_cid: Option<u16>) -> VisualKey {
        VisualKey {
            led_id,
            remap_cid,
            cell: KeyCell::new(KeyId::A, 0.0, 0.0),
        }
    }

    #[test]
    fn key_click_selects_mapped_key_and_ignores_unmapped() {
        let keys = vec![vkey(0, Some(0x50)), vkey(1, None), vkey(2, Some(0x51))];
        // Clicking a mapped key selects its cid.
        assert_eq!(key_click_selection(&keys, 0, None), Some(0x50));
        assert_eq!(key_click_selection(&keys, 2, Some(0x50)), Some(0x51));
        // Clicking an unmapped key keeps the current selection (no-op).
        assert_eq!(key_click_selection(&keys, 1, Some(0x50)), Some(0x50));
        assert_eq!(key_click_selection(&keys, 1, None), None);
        // Out-of-range index is a no-op.
        assert_eq!(key_click_selection(&keys, 9, Some(0x50)), Some(0x50));
    }

    #[test]
    fn selected_key_index_looks_up_by_cid() {
        let keys = vec![vkey(0, Some(0x50)), vkey(1, None), vkey(2, Some(0x51))];
        assert_eq!(selected_key_index(&keys, Some(0x51)), Some(2));
        assert_eq!(selected_key_index(&keys, Some(0x50)), Some(0));
        // No selection, or a cid not on the grid, highlights nothing.
        assert_eq!(selected_key_index(&keys, None), None);
        assert_eq!(selected_key_index(&keys, Some(0x99)), None);
    }

    #[test]
    fn chip_width_clamps_and_grows() {
        // Short labels keep the uniform base width.
        assert_eq!(chip_width(10.0), 62.0);
        assert_eq!(chip_width(42.0), 62.0);
        // Long labels grow to fit with side padding, so text never overflows.
        assert_eq!(chip_width(90.0), 110.0);
        assert!(chip_width(200.0) > chip_width(90.0));
    }

    #[test]
    fn same_category_matches_variant_not_value() {
        let left = ButtonAction::MouseButton {
            btn: MouseBtn::Left,
        };
        let right = ButtonAction::MouseButton {
            btn: MouseBtn::Right,
        };
        assert!(same_category(&left, &right));
        assert!(!same_category(
            &left,
            &ButtonAction::DpiCycle {
                direction: CycleDir::Up
            }
        ));
    }

    #[test]
    fn open_app_pick_maps_path_and_ignores_cancel() {
        assert_eq!(open_app_from_pick(None), None);
        assert_eq!(
            open_app_from_pick(Some(std::path::PathBuf::from("/usr/bin/foo"))),
            Some(ButtonAction::OpenApp {
                path: "/usr/bin/foo".to_string()
            })
        );
    }

    #[test]
    fn adapt_to_category_preserves_sub_values() {
        let current = ButtonAction::MomentaryDpi { dpi: 3200 };
        let proto = ButtonAction::MomentaryDpi { dpi: 800 };
        let result = adapt_to_category(proto, &current);
        assert_eq!(result, ButtonAction::MomentaryDpi { dpi: 3200 });
    }

    #[test]
    fn adapt_to_category_uses_proto_for_type_change() {
        let current = ButtonAction::Native;
        let proto = ButtonAction::MomentaryDpi { dpi: 800 };
        let result = adapt_to_category(proto, &current);
        assert_eq!(result, ButtonAction::MomentaryDpi { dpi: 800 });
    }

    /// Build one instance of every `ButtonAction` variant so the three parallel
    /// label/tag/type matches are all exercised — a new variant that forgets an
    /// arm fails to compile here.
    fn all_actions() -> Vec<ButtonAction> {
        vec![
            ButtonAction::Native,
            ButtonAction::Disable,
            ButtonAction::MouseButton {
                btn: MouseBtn::Left,
            },
            ButtonAction::Scroll {
                axis: ScrollAxis::Vertical,
                clicks: 1,
            },
            ButtonAction::KeyChord {
                key: 0,
                modifiers: vec![],
            },
            ButtonAction::MediaKey {
                key: MediaAction::Play,
            },
            ButtonAction::DpiCycle {
                direction: CycleDir::Up,
            },
            ButtonAction::ProfileCycle {
                direction: CycleDir::Up,
            },
            ButtonAction::MomentaryDpi { dpi: 800 },
            ButtonAction::LayerShift,
            ButtonAction::Macro { steps: vec![] },
            ButtonAction::OpenApp {
                path: String::new(),
            },
            ButtonAction::Command {
                cmd: String::new(),
                args: vec![],
            },
        ]
    }

    #[test]
    fn label_tag_type_cover_all_variants() {
        for a in all_actions() {
            // Each field is non-empty for every variant; the exhaustive
            // `match` arms guarantee no variant is missed.
            let d = action_display(&a);
            assert!(!d.label.is_empty());
            assert!(!d.tag.is_empty());
            assert!(!d.type_name.is_empty());
        }
    }

    #[test]
    fn label_formats_embed_sub_values() {
        // Sub-value formats (numbers, `{:?}` enum names) aren't translated
        // copy, so pinning the exact text is meaningful here, unlike the
        // translated-copy checks below.
        assert_eq!(
            action_display(&ButtonAction::MomentaryDpi { dpi: 1600 }).label,
            "DPI 1600"
        );
        assert_eq!(
            action_display(&ButtonAction::DpiCycle {
                direction: CycleDir::Down
            })
            .label,
            "DPI Down"
        );
    }

    #[test]
    fn type_name_and_tag_are_translated() {
        // Translated copy can be reworded any time — assert a translation
        // exists (differs from the raw i18n key) rather than pinning text.
        let native = action_display(&ButtonAction::Native);
        assert_ne!(native.type_name, "device.keys_type_default");
        let disabled = action_display(&ButtonAction::Disable);
        assert_ne!(disabled.tag, "device.keys_tag_disabled");
    }

    #[test]
    fn action_categories_covers_all_expected_variants() {
        let cats = action_categories();
        assert!(cats.iter().any(|a| matches!(a, ButtonAction::Native)));
        assert!(cats
            .iter()
            .any(|a| matches!(a, ButtonAction::MediaKey { .. })));
        assert!(cats
            .iter()
            .any(|a| matches!(a, ButtonAction::MomentaryDpi { .. })));
        assert!(cats.iter().any(|a| matches!(a, ButtonAction::Macro { .. })));
    }

    #[test]
    fn adapt_to_category_preserves_macro_steps() {
        let steps = vec![halod_shared::types::MacroStep {
            kind: halod_shared::types::MacroAtom::KeyDown { key: 30 },
            delay_after_ms: 0,
        }];
        let current = ButtonAction::Macro {
            steps: steps.clone(),
        };
        let proto = ButtonAction::Macro { steps: vec![] };
        assert_eq!(
            adapt_to_category(proto, &current),
            ButtonAction::Macro { steps }
        );
    }

    #[test]
    fn mapping_sync_preserves_incomplete_local_macro_draft() {
        let mut keys = crate::ui::screens::device::KeysTab::default();
        sync_mapping_editor(&mut keys, 7, ButtonAction::Native, ButtonAction::Native);
        keys.keys_action = Some(ButtonAction::Macro { steps: vec![] });

        sync_mapping_editor(&mut keys, 7, ButtonAction::Native, ButtonAction::Native);

        assert_eq!(
            keys.keys_action,
            Some(ButtonAction::Macro { steps: vec![] })
        );
    }

    #[test]
    fn mapping_sync_applies_external_change_to_clean_layer() {
        let mut keys = crate::ui::screens::device::KeysTab::default();
        sync_mapping_editor(&mut keys, 7, ButtonAction::Native, ButtonAction::Native);

        sync_mapping_editor(&mut keys, 7, ButtonAction::Disable, ButtonAction::Native);

        assert_eq!(keys.keys_action, Some(ButtonAction::Disable));
    }

    #[test]
    fn incomplete_parameterized_actions_are_editor_drafts() {
        assert!(!mapping_is_complete(
            &ButtonAction::Macro { steps: vec![] },
            &ButtonAction::Native
        ));
        assert!(!mapping_is_complete(
            &ButtonAction::OpenApp {
                path: String::new()
            },
            &ButtonAction::Native
        ));
        assert!(!mapping_is_complete(
            &ButtonAction::Command {
                cmd: String::new(),
                args: vec![]
            },
            &ButtonAction::Native
        ));
        assert!(mapping_is_complete(
            &ButtonAction::Macro {
                steps: vec![halod_shared::types::MacroStep {
                    kind: halod_shared::types::MacroAtom::KeyDown { key: 30 },
                    delay_after_ms: 0,
                }]
            },
            &ButtonAction::Native
        ));
    }

    #[test]
    fn layer_get_set_target_independent_buffers() {
        let mut st = DeviceUi::default();
        Layer::Base.set(&mut st, ButtonAction::Native);
        Layer::Shifted.set(&mut st, ButtonAction::Disable);
        assert_eq!(Layer::Base.get(&st), Some(ButtonAction::Native));
        assert_eq!(Layer::Shifted.get(&st), Some(ButtonAction::Disable));
    }

    #[test]
    fn layer_pair_preserves_the_other_layers_buffer() {
        let mut st = DeviceUi::default();
        st.keys.keys_action = Some(ButtonAction::Native);
        st.keys.keys_shifted_action = Some(ButtonAction::Disable);

        let new_base = ButtonAction::MouseButton {
            btn: MouseBtn::Left,
        };
        assert_eq!(
            Layer::Base.pair(&st, new_base.clone()),
            (new_base, ButtonAction::Disable)
        );

        let new_shifted = ButtonAction::MediaKey {
            key: MediaAction::Play,
        };
        assert_eq!(
            Layer::Shifted.pair(&st, new_shifted.clone()),
            (ButtonAction::Native, new_shifted)
        );
    }

    #[test]
    fn shifted_layer_excludes_layer_shift_category() {
        assert!(!Layer::Shifted.allows(&ButtonAction::LayerShift));
        assert!(Layer::Base.allows(&ButtonAction::LayerShift));
        assert!(Layer::Shifted.allows(&ButtonAction::Native));
    }
}
