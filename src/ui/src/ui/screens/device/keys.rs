// SPDX-License-Identifier: GPL-3.0-or-later
//! Keys/Buttons tab — per-button action remapping: a button list on the left
//! and one editor card for the selected button and layer on the right.

use crate::ui::components as widgets;
use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::commands::{DaemonCommand, DeviceOp};
use halod_shared::keycodes;
use halod_shared::types::{
    ButtonAction, ButtonMapping, CycleDir, DeviceCapability, DeviceType, KeyRemapStatus,
    MediaAction, ModKey, MouseBtn, ScrollAxis,
};

use super::{macro_editor, DeviceUi, TabCtx};
use crate::domain::tour;
use crate::domain::tour::AnchorId;
use crate::runtime::ipc;
use crate::ui::theme::{self, a};
use halod_shared::ids::DeviceId;

const LIST_W: f32 = 398.0;
const ROW_H: f32 = 58.0;

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    tour::anchor(ui.ctx(), AnchorId::TabKeys, ui.max_rect());
    let Some(remap) = ctx.dev.capabilities.iter().find_map(|c| match c {
        DeviceCapability::KeyRemap(r) => Some(r.clone()),
        _ => None,
    }) else {
        return;
    };
    let id = ctx.dev.id.clone();

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

    if let Some(status) = ctx.dev.keyboard_layout() {
        if !status.keys.is_empty() {
            keyboard_overview(ui, st, status);
            ui.add_space(theme::SPACE_8);
        }
    }

    let tab_label = if matches!(ctx.dev.device_type, DeviceType::Mouse) {
        t!("device.keys_tab_buttons")
    } else {
        t!("device.keys_tab_keys")
    };
    let gap = theme::SPACE_8;
    let left_w = LIST_W.min((ui.available_width() - gap) * 0.4);
    widgets::split_columns(ui, left_w, gap, |left, right| {
        button_list_card(left, ctx, st, &remap, &id, &tab_label);
        editor_card(right, ctx, st, &remap, &id);
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

fn selectable_cid_at(
    keys: &[halod_shared::keyboard::VisualKey],
    rects: &[Rect],
    pos: egui::Pos2,
    unit: f32,
) -> Option<u16> {
    let idx = super::keyboard_visual::hit_key(keys, rects, pos, unit)?;
    keys[idx].remap_cid
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

    let (resp, inner) = kbv::panel(ui, 240.0, Sense::click());
    let keys = &status.keys;
    let rects = kbv::key_rects(keys, inner, 3.0);
    let unit = kbv::unit_for(keys, inner);

    let hovered = resp
        .hover_pos()
        .and_then(|pos| selectable_cid_at(keys, &rects, pos, unit));
    if hovered.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        if resp.clicked() {
            st.keys.keys_sel_cid = hovered;
        }
    }

    let selected = selected_key_index(keys, st.keys.keys_sel_cid);
    let cid_by_led: std::collections::HashMap<u32, u16> = keys
        .iter()
        .filter_map(|k| Some((k.led_id, k.remap_cid?)))
        .collect();
    let selected_cid = st.keys.keys_sel_cid;
    kbv::draw_keyboard(
        ui,
        keys,
        &rects,
        &|id| key_fill(cid_by_led.get(&id).copied(), selected_cid, hovered),
        selected,
        status.language,
        unit,
    );
}

fn key_fill(cid: Option<u16>, selected: Option<u16>, hovered: Option<u16>) -> egui::Color32 {
    match cid {
        Some(cid) if Some(cid) == selected => theme::CYAN,
        Some(cid) if Some(cid) == hovered => a(theme::CYAN, 0.55),
        Some(_) => theme::ACCENT_BORDER,
        None => theme::INNER_BG,
    }
}

// ── Left card: button list ───────────────────────────────────────────────────

fn button_list_card(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    remap: &KeyRemapStatus,
    id: &DeviceId,
    tab_label: &str,
) {
    widgets::card_with_margin(ui, egui::Margin::same(8), |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                egui::Sides::new().height(30.0).show(
                    ui,
                    |ui| {
                        ui.label(
                            egui::RichText::new(tab_label)
                                .font(theme::heading())
                                .color(theme::TEXT),
                        );
                    },
                    |ui| {
                        if widgets::button(
                            ui,
                            &t!("device.keys_reset_all"),
                            widgets::ButtonKind::Secondary,
                            Vec2::new(0.0, 30.0),
                        )
                        .clicked()
                        {
                            ipc::send(
                                ctx.cmd,
                                DaemonCommand::Device {
                                    device_id: id.clone(),
                                    op: DeviceOp::ResetAllButtonMappings,
                                },
                            );
                        }
                    },
                );
            });

        for btn in &remap.buttons {
            let action = remap
                .mappings
                .iter()
                .find(|m| m.cid == btn.cid)
                .map(|m| m.base.clone())
                .unwrap_or_default();
            let ActionDisplay {
                tag, label, tone, ..
            } = action_display(&action);
            let selected = st.keys.keys_sel_cid == Some(btn.cid);

            let (rect, resp) = widgets::hover_row(ui, ROW_H, selected);
            let p = ui.painter();
            let tag_rect =
                widgets::paint_tag(p, rect.right_center() - Vec2::new(12.0, 0.0), &tag, tone);
            let text = p.with_clip_rect(Rect::from_min_max(
                rect.min,
                Pos2::new(tag_rect.left() - 12.0, rect.max.y),
            ));
            text.text(
                Pos2::new(rect.left() + 12.0, rect.center().y - 8.0),
                Align2::LEFT_CENTER,
                &btn.label,
                theme::medium(14.0),
                theme::TEXT,
            );
            text.text(
                Pos2::new(rect.left() + 12.0, rect.center().y + 9.0),
                Align2::LEFT_CENTER,
                label,
                theme::body_sm(),
                theme::TEXT_DIM,
            );

            if resp.clicked() {
                st.keys.keys_sel_cid = Some(btn.cid);
            }
        }
    });
}

// ── Which layer the editor edits ─────────────────────────────────────────────

/// A button mapping has two independent slots: the plain press (`Base`) and
/// what it does while the layer-shift key is held (`Shifted`).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(super) enum Layer {
    #[default]
    Base,
    Shifted,
}

impl Layer {
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

// ── Right card: editor for the selected button ───────────────────────────────

fn editor_card(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    remap: &KeyRemapStatus,
    id: &DeviceId,
) {
    widgets::card(ui, |ui| {
        ui.set_width(ui.available_width());
        let Some(cid) = st.keys.keys_sel_cid else {
            ui.label(
                egui::RichText::new(t!("device.keys_select_button_prompt"))
                    .font(theme::body_md())
                    .color(theme::TEXT_FAINT),
            );
            return;
        };
        let name = remap
            .buttons
            .iter()
            .find(|b| b.cid == cid)
            .map(|b| b.label.as_str())
            .unwrap_or("-");

        egui::Sides::new().show(
            ui,
            |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = theme::SPACE_2;
                    ui.label(
                        egui::RichText::new(name)
                            .font(theme::semibold(18.0))
                            .color(theme::TEXT),
                    );
                    ui.label(
                        egui::RichText::new(t!("device.keys_editor_hint"))
                            .font(theme::body_md())
                            .color(theme::TEXT_DIM),
                    );
                });
            },
            |ui| {
                let labels = [
                    t!("device.keys_layer_normal"),
                    t!("device.keys_layer_shift"),
                ];
                let labels: Vec<&str> = labels.iter().map(|l| l.as_ref()).collect();
                let mut idx = st.keys.layer as usize;
                let rect = ui
                    .scope(|ui| widgets::segmented(ui, &labels, &mut idx, false))
                    .response
                    .rect;
                tour::anchor(ui.ctx(), AnchorId::KeysLayerShift, rect);
                st.keys.layer = Layer::from_shifted(idx == 1);
            },
        );
        let layer = st.keys.layer;
        let action = layer.get(st).unwrap_or_default();

        ui.add_space(theme::SPACE_9);
        widgets::section_label(ui, &t!("device.keys_action"));
        ui.add_space(theme::SPACE_4);
        action_grid(ui, ctx, st, id, cid, layer, &action);

        if has_params(&action) {
            ui.add_space(theme::SPACE_9);
            params_editor(ui, ctx, st, id, action, layer);
        }

        ui.add_space(theme::SPACE_9);
        let (rule, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
        ui.painter().line_segment(
            [rule.left_center(), rule.right_center()],
            Stroke::new(1.0, theme::BORDER_SOFT),
        );
        ui.add_space(theme::SPACE_4);
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), 36.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                if widgets::button(
                    ui,
                    &t!("device.keys_restore_default"),
                    widgets::ButtonKind::Secondary,
                    Vec2::new(0.0, 36.0),
                )
                .clicked()
                {
                    st.keys.keys_action = Some(ButtonAction::Native);
                    st.keys.keys_shifted_action = Some(ButtonAction::Native);
                    st.keys.chord_capture = None;
                    st.last_edit = ctx.time;
                    ipc::send(
                        ctx.cmd,
                        DaemonCommand::Device {
                            device_id: id.clone(),
                            op: DeviceOp::ResetButtonMapping { cid },
                        },
                    );
                }
            },
        );
    });
}

/// Whether an action has a sub-option section below the action grid.
fn has_params(action: &ButtonAction) -> bool {
    !matches!(
        action,
        ButtonAction::Native | ButtonAction::Disable | ButtonAction::LayerShift
    )
}

fn action_grid(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &DeviceId,
    cid: u16,
    layer: Layer,
    current_action: &ButtonAction,
) {
    let protos: Vec<&ButtonAction> = action_categories()
        .iter()
        .filter(|p| layer.allows(p))
        .collect();
    let names: Vec<_> = protos.iter().map(|p| action_display(p).type_name).collect();
    let labels: Vec<&str> = names.iter().map(|n| n.as_ref()).collect();
    let selected = protos.iter().position(|p| same_category(current_action, p));
    let tiles = widgets::option_grid(ui, &labels, selected, 4);
    if let Some(grid) = tiles.iter().map(|t| t.rect).reduce(|a, b| a.union(b)) {
        tour::anchor(ui.ctx(), AnchorId::KeysActionCategory, grid);
    }
    for (proto, tile) in protos.into_iter().zip(&tiles) {
        if matches!(proto, ButtonAction::Macro { .. }) {
            anchor_macro_tour(ui.ctx(), tile.rect);
        }
        if tile.clicked() {
            let new_action = adapt_to_category(proto.clone(), current_action);
            let (base, shifted) = layer.pair(st, new_action.clone());
            layer.set(st, new_action);
            st.last_edit = ctx.time;
            if mapping_is_complete(&base, &shifted) {
                ipc::send(
                    ctx.cmd,
                    DaemonCommand::Device {
                        device_id: id.clone(),
                        op: DeviceOp::SetButtonMapping {
                            mapping: ButtonMapping { cid, base, shifted },
                        },
                    },
                );
            }
        }
    }
}

/// A captured key-chord press: the key's native code plus held modifiers.
/// Modifier keys alone are not a chord, so they are skipped.
fn chord_from_event(ev: &egui::Event) -> Option<(u32, Vec<ModKey>)> {
    let ctrl = |name: &str| keycodes::native_code(name).map(|k| (k, vec![ModKey::Ctrl]));
    match ev {
        egui::Event::Key {
            key,
            physical_key,
            pressed: true,
            repeat: false,
            modifiers,
        } => {
            let code = macro_editor::key_code(*key, *physical_key)?;
            let name = keycodes::label(code);
            if ["Shift", "Control", "Alt", "Super"]
                .iter()
                .any(|m| name.starts_with(m))
            {
                return None;
            }
            let mods = [
                (modifiers.ctrl, ModKey::Ctrl),
                (modifiers.shift, ModKey::Shift),
                (modifiers.alt, ModKey::Alt),
                (modifiers.mac_cmd, ModKey::Super),
            ]
            .into_iter()
            .filter_map(|(on, m)| on.then_some(m))
            .collect();
            Some((code, mods))
        }
        egui::Event::Copy => ctrl("C"),
        egui::Event::Cut => ctrl("X"),
        egui::Event::Paste(_) => ctrl("V"),
        _ => None,
    }
}

fn chord_label(key: u32, modifiers: &[ModKey]) -> String {
    modifiers
        .iter()
        .map(|m| format!("{m:?}"))
        .chain(std::iter::once(keycodes::label(key)))
        .collect::<Vec<_>>()
        .join(" + ")
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
    widgets::section_label(ui, &t!("device.keys_direction"));
    ui.add_space(theme::SPACE_4);
    let dirs = [
        (t!("device.keys_dir_up"), CycleDir::Up),
        (t!("device.keys_dir_down"), CycleDir::Down),
    ];
    if let Some(dir) = widgets::option_grid_value(ui, &dirs, &current, 2) {
        let a = make_action(dir);
        layer.set(st, a.clone());
        st.last_edit = ctx.time;
        ipc::send(ctx.cmd, make_cmd(a));
    }
}

fn params_editor(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &DeviceId,
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
        DaemonCommand::Device {
            device_id: id.clone(),
            op: DeviceOp::SetButtonMapping {
                mapping: ButtonMapping { cid, base, shifted },
            },
        }
    };
    let tag = layer.tag();
    // Apply an edited action: seed the layer buffer, then send now (`apply`)
    // or debounce under a per-field key (`queue`).
    let apply = |st: &mut DeviceUi, a: ButtonAction| {
        layer.set(st, a.clone());
        st.last_edit = ctx.time;
        let cmd = make_cmd(a);
        if let DaemonCommand::Device {
            op: DeviceOp::SetButtonMapping { mapping, .. },
            ..
        } = &cmd
        {
            if mapping_is_complete(&mapping.base, &mapping.shifted) {
                ipc::send(ctx.cmd, cmd);
            }
        }
    };
    let queue = |st: &mut DeviceUi, field: &str, a: ButtonAction| {
        layer.set(st, a.clone());
        let cmd = make_cmd(a);
        if let DaemonCommand::Device {
            op: DeviceOp::SetButtonMapping { mapping, .. },
            ..
        } = &cmd
        {
            if mapping_is_complete(&mapping.base, &mapping.shifted) {
                st.queue(&format!("btn:{field}:{cid}:{tag}"), cmd, ctx.time);
            }
        }
    };

    match action {
        ButtonAction::MouseButton { btn } => {
            widgets::section_label(ui, &t!("device.keys_mouse_button"));
            ui.add_space(theme::SPACE_4);
            let options = [
                (t!("device.keys_mouse_left"), MouseBtn::Left),
                (t!("device.keys_mouse_right"), MouseBtn::Right),
                (t!("device.keys_mouse_middle"), MouseBtn::Middle),
                (t!("device.keys_mouse_back"), MouseBtn::Back),
                (t!("device.keys_mouse_forward"), MouseBtn::Forward),
            ];
            if let Some(btn) = widgets::option_grid_value(ui, &options, &btn, 3) {
                apply(st, ButtonAction::MouseButton { btn });
            }
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
            widgets::section_label(ui, &t!("device.keys_dpi_value"));
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
            widgets::section_label(ui, &t!("device.keys_presets"));
            ui.add_space(theme::SPACE_4);
            let presets = [400u16, 800, 1600, 3200, 6400].map(|v| (v.to_string(), v));
            if let Some(dpi) = widgets::option_grid_value(ui, &presets, &dpi, 5) {
                apply(st, ButtonAction::MomentaryDpi { dpi });
            }
        }
        ButtonAction::MediaKey { key } => {
            widgets::section_label(ui, &t!("device.keys_media_key"));
            ui.add_space(theme::SPACE_4);
            let options = [
                (t!("device.keys_media_play"), MediaAction::Play),
                (t!("device.keys_media_next"), MediaAction::Next),
                (t!("device.keys_media_prev"), MediaAction::Prev),
                (t!("device.keys_media_mute"), MediaAction::Mute),
                (t!("device.keys_media_volume_up"), MediaAction::VolumeUp),
                (t!("device.keys_media_volume_down"), MediaAction::VolumeDown),
            ];
            if let Some(key) = widgets::option_grid_value(ui, &options, &key, 3) {
                apply(st, ButtonAction::MediaKey { key });
            }
        }
        ButtonAction::Scroll { axis, clicks } => {
            widgets::section_label(ui, &t!("device.keys_axis"));
            ui.add_space(theme::SPACE_4);
            let options = [
                (t!("device.keys_axis_vertical"), ScrollAxis::Vertical),
                (t!("device.keys_axis_horizontal"), ScrollAxis::Horizontal),
            ];
            if let Some(axis) = widgets::option_grid_value(ui, &options, &axis, 2) {
                apply(st, ButtonAction::Scroll { axis, clicks });
            }
            ui.add_space(theme::SPACE_6);
            widgets::section_label(ui, &t!("device.keys_clicks_per_event"));
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
            widgets::section_label(ui, &t!("device.keys_application"));
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
                    ui.available_width() - 96.0,
                    &mut buf,
                    &t!("device.keys_app_hint"),
                    ("openapp", cid, tag),
                );
                if resp.changed() {
                    queue(st, "app", ButtonAction::OpenApp { path: buf.clone() });
                }
                if widgets::button(
                    ui,
                    &t!("device.keys_browse"),
                    widgets::ButtonKind::Secondary,
                    Vec2::new(88.0, 34.0),
                )
                .clicked()
                {
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
            widgets::section_label(ui, &t!("device.keys_command"));
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
            tour::anchor(ui.ctx(), AnchorId::KeysMacro, macro_rect);
            macro_editor::show(ui, ctx, st, id, cid, layer, steps);
        }
        ButtonAction::KeyChord { key, modifiers } => {
            widgets::section_label(ui, &t!("device.keys_type_key_chord"));
            ui.add_space(theme::SPACE_4);
            let armed = st.keys.chord_capture == Some((cid, layer));
            let label = if armed {
                t!("device.keys_chord_press").to_string()
            } else if key == 0 {
                t!("device.keys_chord_record").to_string()
            } else {
                chord_label(key, &modifiers)
            };
            let tile = widgets::option_grid(ui, &[&label], armed.then_some(0), 1);
            if tile[0].clicked() {
                st.keys.chord_capture = (!armed).then_some((cid, layer));
            }
            if armed {
                st.last_edit = ctx.time;
                ui.memory_mut(|m| m.request_focus(egui::Id::unique(("chord", cid, tag))));
                if let Some((key, modifiers)) =
                    ui.input(|i| i.events.iter().find_map(chord_from_event))
                {
                    st.keys.chord_capture = None;
                    apply(st, ButtonAction::KeyChord { key, modifiers });
                }
            }
        }
        _ => {}
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
            .id(egui::Id::unique(id)),
    )
}

/// Keep the macro tour step reachable before Macro is selected. Once it is
/// selected, the later parameters-card anchor replaces this fallback with the
/// full macro editor rect in the same frame.
fn anchor_macro_tour(ctx: &egui::Context, rect: Rect) {
    tour::anchor(ctx, AnchorId::KeysMacro, rect);
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
        ButtonAction::Scroll {
            axis: ScrollAxis::Vertical,
            clicks: 1,
        },
        ButtonAction::KeyChord {
            key: 0,
            modifiers: Vec::new(),
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
        (ButtonAction::KeyChord { .. }, ButtonAction::KeyChord { key, modifiers }) => {
            ButtonAction::KeyChord {
                key: *key,
                modifiers: modifiers.clone(),
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
            ButtonAction::KeyChord { key, .. } => *key != 0,
            ButtonAction::OpenApp { path } => !path.is_empty(),
            ButtonAction::Command { cmd, .. } => !cmd.is_empty(),
            _ => true,
        }
    }
    complete(base) && complete(shifted)
}

/// Everything the UI shows for one action: the left-card row's tag and tone, the Parameters card header's type name, and the row's full
/// label (type name plus any sub-value, e.g. "DPI cycle: Up").
struct ActionDisplay {
    tag: std::borrow::Cow<'static, str>,
    type_name: std::borrow::Cow<'static, str>,
    label: String,
    tone: widgets::TagTone,
}

fn action_display(a: &ButtonAction) -> ActionDisplay {
    use widgets::TagTone as T;
    let (tag, type_name, tone) = match a {
        ButtonAction::Native => (
            t!("device.keys_tag_default"),
            t!("device.keys_type_default"),
            T::Neutral,
        ),
        ButtonAction::Disable => (
            t!("device.keys_tag_disabled"),
            t!("device.keys_type_disabled"),
            T::Danger,
        ),
        ButtonAction::MouseButton { .. } => (
            t!("device.keys_tag_mouse_btn"),
            t!("device.keys_type_mouse_button"),
            T::Accent,
        ),
        ButtonAction::Scroll { .. } => (
            t!("device.keys_tag_scroll"),
            t!("device.keys_type_scroll"),
            T::Accent,
        ),
        ButtonAction::KeyChord { .. } => (
            t!("device.keys_tag_key_chord"),
            t!("device.keys_type_key_chord"),
            T::Accent,
        ),
        ButtonAction::MediaKey { .. } => (
            t!("device.keys_tag_media"),
            t!("device.keys_type_media_key"),
            T::Accent,
        ),
        ButtonAction::DpiCycle { .. } => (
            t!("device.keys_tag_dpi_cycle"),
            t!("device.keys_type_dpi_cycle"),
            T::Warn,
        ),
        ButtonAction::ProfileCycle { .. } => (
            t!("device.keys_tag_profile"),
            t!("device.keys_type_profile_cycle"),
            T::Ok,
        ),
        ButtonAction::MomentaryDpi { .. } => (
            t!("device.keys_tag_dpi_snap"),
            t!("device.keys_type_momentary_dpi"),
            T::Warn,
        ),
        ButtonAction::LayerShift => (
            t!("device.keys_tag_layer"),
            t!("device.keys_type_layer_shift"),
            T::Ok,
        ),
        ButtonAction::Macro { .. } => (
            t!("device.keys_tag_macro"),
            t!("device.keys_type_macro"),
            T::Ok,
        ),
        ButtonAction::OpenApp { .. } => (
            t!("device.keys_tag_launch"),
            t!("device.keys_type_open_app"),
            T::Ok,
        ),
        ButtonAction::Command { .. } => (
            t!("device.keys_tag_command"),
            t!("device.keys_type_command"),
            T::Warn,
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
        tone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::keyboard::{KeyCell, KeyId, VisualKey};

    #[test]
    fn macro_category_registers_the_second_tour_step_before_selection() {
        let ctx = egui::Context::default();
        let rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(80.0, 31.0));

        anchor_macro_tour(&ctx, rect);

        assert_eq!(
            crate::domain::tour::take_anchor(&ctx, crate::domain::tour::AnchorId::KeysMacro),
            Some(rect)
        );
    }

    fn vkey(led_id: u32, remap_cid: Option<u16>) -> VisualKey {
        VisualKey {
            led_id,
            remap_cid,
            cell: KeyCell::new(KeyId::A, 0.0, 0.0),
        }
    }

    #[test]
    fn selectable_hovered_and_selected_keys_each_get_their_own_fill() {
        let plain = key_fill(None, Some(1), Some(1));
        let selectable = key_fill(Some(2), Some(1), None);
        let hovered = key_fill(Some(2), Some(1), Some(2));
        let selected = key_fill(Some(1), Some(1), Some(1));
        let fills = [plain, selectable, hovered, selected];
        for (i, a) in fills.iter().enumerate() {
            for b in &fills[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn only_keys_tied_to_a_remap_button_are_selectable() {
        let mut keys = vec![vkey(0, Some(0x50)), vkey(1, None)];
        keys[1].cell.col = 1.0;
        let avail = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 100.0));
        let rects = super::super::keyboard_visual::key_rects(&keys, avail, 3.0);
        let unit = super::super::keyboard_visual::unit_for(&keys, avail);
        let at = |i: usize| selectable_cid_at(&keys, &rects, rects[i].center(), unit);
        assert_eq!(at(0), Some(0x50));
        assert_eq!(at(1), None);
        assert_eq!(
            selectable_cid_at(&keys, &rects, egui::Pos2::new(-5.0, -5.0), unit),
            None
        );
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

    fn key_press(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn chord_capture_takes_key_with_held_modifiers() {
        let a = keycodes::native_code("A").unwrap();
        let ev = key_press(egui::Key::A, egui::Modifiers::CTRL | egui::Modifiers::SHIFT);
        assert_eq!(
            chord_from_event(&ev),
            Some((a, vec![ModKey::Ctrl, ModKey::Shift]))
        );
    }

    #[test]
    fn chord_capture_ignores_lone_modifiers_and_releases() {
        let ev = key_press(egui::Key::ControlLeft, egui::Modifiers::CTRL);
        assert_eq!(chord_from_event(&ev), None);
        let release = egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        assert_eq!(chord_from_event(&release), None);
    }

    #[test]
    fn chord_capture_recovers_clipboard_shortcuts() {
        let c = keycodes::native_code("C").unwrap();
        assert_eq!(
            chord_from_event(&egui::Event::Copy),
            Some((c, vec![ModKey::Ctrl]))
        );
    }

    #[test]
    fn unrecorded_key_chord_is_a_draft() {
        let empty = ButtonAction::KeyChord {
            key: 0,
            modifiers: vec![],
        };
        assert!(!mapping_is_complete(&empty, &ButtonAction::Native));
        let recorded = ButtonAction::KeyChord {
            key: keycodes::native_code("A").unwrap(),
            modifiers: vec![ModKey::Ctrl],
        };
        assert!(mapping_is_complete(&recorded, &ButtonAction::Native));
    }
}
