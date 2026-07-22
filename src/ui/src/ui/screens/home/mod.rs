// SPDX-License-Identifier: GPL-3.0-or-later
//! The Home screen main panel: greeting + summary, the configurable widget row,
//! and the device grid/list. All data comes from the live daemon state.

mod widget_config;
pub mod widget_row;
mod widget_view;

use crate::ui::components as widgets;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::domain::topic_store::TopicStore;
use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{
    ConflictDeviceSource, DeviceCapability, DeviceType, VisibilityState, WireDevice,
};

use crate::domain::models::device::{self as model, Metric};
use crate::domain::state::{Rename, Variant};
use crate::runtime::ipc::CommandTx;
use crate::ui::theme::{self, a};

const GAP: f32 = 16.0;

/// Everything the Home screen owns across frames.
#[derive(Default)]
pub struct HomeUi {
    pub show_hidden: bool,
    pub variant: Variant,
    /// Device-list filter text (matches name or vendor).
    pub search: String,
    pub rename: Option<Rename>,
    /// Pending confirmation to unlink a chained device.
    pub confirm_remove: Option<ConfirmRemove>,
    /// Open duplicate-device resolve dialog: every conflict group with the
    /// owner the user has picked to keep. `None` when the dialog is closed.
    pub conflict_resolve: Option<Vec<ConflictGroup>>,
    pub widgets: widget_row::EditState,
}

pub fn show(
    ui: &mut egui::Ui,
    state: &TopicStore,
    cmd: &CommandTx,
    home: &mut HomeUi,
    history: &HashMap<String, VecDeque<f32>>,
    page: &mut crate::domain::state::Page,
    time: f64,
    allow_modals: bool,
) {
    let sensors = crate::domain::models::sensors::sensors(state);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Frame::NONE
                .inner_margin(egui::Margin {
                    left: 36,
                    right: 36,
                    top: 30,
                    bottom: 30,
                })
                .show(ui, |ui| {
                    header(ui, state, home);
                    let conflicts = conflict_group_count(&state.devices);
                    if conflicts > 0 {
                        ui.add_space(theme::SPACE_9);
                        if attention_banner(ui, conflicts) {
                            home.conflict_resolve = Some(conflict_groups(&state.devices));
                        }
                    }
                    let has_row = home.widgets.customizing() || !state.gui.home_widgets.is_empty();
                    ui.add_space(if has_row { 24.0 } else { 14.0 });
                    widget_row::show(
                        ui,
                        &mut home.widgets,
                        widget_row::RowCtx {
                            state,
                            cmd,
                            history,
                            sensors: &sensors,
                            time,
                        },
                    );
                    if has_row {
                        ui.add_space(26.0);
                    }

                    let devices: Vec<&WireDevice> = state
                        .devices
                        .iter()
                        .filter(|d| {
                            model::listable(d)
                                && (home.show_hidden || !model::is_hidden(d))
                                && model::matches_query(d, &home.search)
                        })
                        .collect();

                    if devices.is_empty() {
                        empty(ui, !home.search.trim().is_empty());
                    } else {
                        match home.variant {
                            Variant::Grid => grid(
                                ui,
                                &devices,
                                &state.devices,
                                cmd,
                                &mut home.rename,
                                &mut home.confirm_remove,
                                &mut home.conflict_resolve,
                                page,
                            ),
                            Variant::List => list(
                                ui,
                                &devices,
                                &state.devices,
                                cmd,
                                &mut home.rename,
                                &mut home.confirm_remove,
                                &mut home.conflict_resolve,
                                page,
                            ),
                        }
                    }
                });
        });

    if allow_modals {
        remove_confirm_modal(ui.ctx(), cmd, &mut home.confirm_remove);
        conflict_modal(ui.ctx(), cmd, &mut home.conflict_resolve);
    }
}

/// A pending "remove chained device" confirmation: the child to unlink plus the
/// parent device and channel that host it.
pub struct ConfirmRemove {
    pub child_id: String,
    pub child_name: String,
    pub parent_id: String,
    pub channel_id: String,
}

/// One duplicate-device conflict group and the owner the user has picked to
/// keep. Captured from the snapshot when the resolve modal opens so the dialog
/// stays stable while the live list refreshes.
pub struct ConflictGroup {
    pub devices: Vec<ConflictChoiceDevice>,
    pub recommended_id: String,
    /// The device id currently selected to keep; defaults to `recommended_id`.
    pub pick: String,
}

pub struct ConflictChoiceDevice {
    pub id: String,
    pub name: String,
    pub device_type: DeviceType,
    pub source: ConflictDeviceSource,
}

fn conflict_group_for(d: &WireDevice, all_devices: &[WireDevice]) -> Option<ConflictGroup> {
    let conflict = d.conflict.as_ref()?;
    let ids = std::iter::once(&d.id).chain(conflict.peer_ids.iter());
    let devices = ids
        .filter_map(|id| {
            all_devices
                .iter()
                .find(|candidate| candidate.id == *id)
                .map(|candidate| ConflictChoiceDevice {
                    id: candidate.id.clone(),
                    name: candidate.name.clone(),
                    device_type: candidate.device_type,
                    source: conflict
                        .participants
                        .iter()
                        .find(|participant| participant.id == candidate.id)
                        .map(|participant| participant.source.clone())
                        .unwrap_or_default(),
                })
        })
        .collect::<Vec<_>>();
    (devices.len() > 1).then(|| ConflictGroup {
        devices,
        recommended_id: conflict.recommended_id.clone(),
        pick: conflict.recommended_id.clone(),
    })
}

fn conflict_key(d: &WireDevice) -> Option<String> {
    let conflict = d.conflict.as_ref()?;
    let mut ids = conflict.peer_ids.clone();
    ids.push(d.id.clone());
    ids.sort();
    Some(format!("{:?}:{}", conflict.confidence, ids.join("\u{1f}")))
}

/// Every distinct conflict group in the snapshot, deduped by participant set —
/// each contested physical device appears once, with its recommended owner
/// pre-selected. This is the set the resolve modal walks.
fn conflict_groups(all_devices: &[WireDevice]) -> Vec<ConflictGroup> {
    let mut seen = HashSet::new();
    let mut groups = Vec::new();
    for d in all_devices {
        let Some(key) = conflict_key(d) else {
            continue;
        };
        if seen.insert(key) {
            if let Some(group) = conflict_group_for(d, all_devices) {
                groups.push(group);
            }
        }
    }
    groups
}

/// Count of distinct conflict groups — the number shown in the attention
/// banner. Kept separate from [`conflict_groups`] so the banner needn't build
/// the full snapshot every frame.
fn conflict_group_count(all_devices: &[WireDevice]) -> usize {
    let mut seen = HashSet::new();
    all_devices
        .iter()
        .filter_map(conflict_key)
        .filter(|key| seen.insert(key.clone()))
        .count()
}

fn ids_to_disable(group: &ConflictGroup, kept_id: &str) -> Vec<String> {
    group
        .devices
        .iter()
        .filter(|device| device.id != kept_id)
        .map(|device| device.id.clone())
        .collect()
}

/// Every device id to disable when the whole resolution is applied: for each
/// group, all participants except the one the user kept.
fn disables_for(groups: &[ConflictGroup]) -> Vec<String> {
    groups
        .iter()
        .flat_map(|group| ids_to_disable(group, &group.pick))
        .collect()
}

/// The amber Home banner summarising every unresolved conflict group. Returns
/// `true` when its "Resolve" button is clicked. Only drawn when `count > 0`.
fn attention_banner(ui: &mut egui::Ui, count: usize) -> bool {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 62.0), Sense::hover());
    {
        let p = ui.painter();
        p.rect_filled(rect, theme::RADIUS_LG, a(theme::STAT_AMBER, 0.09));
        p.rect_stroke(
            rect,
            theme::RADIUS_LG,
            Stroke::new(1.0, a(theme::STAT_AMBER, 0.34)),
            egui::StrokeKind::Middle,
        );
        let icon = Rect::from_center_size(
            Pos2::new(rect.left() + 35.0, rect.center().y),
            Vec2::splat(34.0),
        );
        p.rect_filled(icon, theme::RADIUS_MD, a(theme::STAT_AMBER, 0.12));
        p.rect_stroke(
            icon,
            theme::RADIUS_MD,
            Stroke::new(1.0, a(theme::STAT_AMBER, 0.35)),
            egui::StrokeKind::Middle,
        );
        warning_glyph(p, icon.center(), 18.0, theme::STAT_AMBER);

        let tx = icon.right() + 15.0;
        let title = if count == 1 {
            t!("home.conflict_banner_title_one")
        } else {
            t!("home.conflict_banner_title_many", count = count)
        };
        p.text(
            Pos2::new(tx, rect.center().y - 9.0),
            Align2::LEFT_CENTER,
            title,
            theme::heading(),
            theme::hex(0xf0dca0),
        );
        p.text(
            Pos2::new(tx, rect.center().y + 9.0),
            Align2::LEFT_CENTER,
            t!("home.conflict_banner_sub"),
            theme::body_sm(),
            theme::hex(0xb39f6a),
        );
    }
    let btn = Rect::from_min_size(
        Pos2::new(rect.right() - 16.0 - 96.0, rect.center().y - 17.0),
        Vec2::new(96.0, 34.0),
    );
    widgets::button_at(
        ui,
        btn,
        egui::Id::new("conflict_banner_resolve"),
        &t!("home.conflict_resolve"),
        widgets::ButtonKind::Warn,
    )
    .clicked()
}

/// A warning triangle with an exclamation, drawn to fit a `size`×`size` box
/// centered at `c`. egui's bundled fonts don't carry ⚠, so it's painted.
fn warning_glyph(p: &egui::Painter, c: Pos2, size: f32, color: Color32) {
    let h = size * 0.5;
    let stroke = Stroke::new(1.6, color);
    p.add(egui::Shape::closed_line(
        vec![
            Pos2::new(c.x, c.y - h * 0.86),
            Pos2::new(c.x + h, c.y + h * 0.72),
            Pos2::new(c.x - h, c.y + h * 0.72),
        ],
        stroke,
    ));
    p.line_segment(
        [
            Pos2::new(c.x, c.y - h * 0.22),
            Pos2::new(c.x, c.y + h * 0.16),
        ],
        stroke,
    );
    p.circle_filled(Pos2::new(c.x, c.y + h * 0.46), 1.1, color);
}

/// The revised resolve dialog: every conflict group at once, each with radio
/// owner options. Applying keeps the picked owner per group and disables the
/// rest in a single pass.
fn conflict_modal(
    ctx: &egui::Context,
    cmd: &CommandTx,
    conflict_resolve: &mut Option<Vec<ConflictGroup>>,
) {
    let Some(groups) = conflict_resolve.as_mut() else {
        return;
    };
    if groups.is_empty() {
        *conflict_resolve = None;
        return;
    }
    let count = groups.len();
    let mut apply = false;
    let mut cancel = false;
    let dismissed = widgets::dialog(
        ctx,
        "home_conflict_resolve",
        &t!("home.conflict_modal_title"),
        560.0,
        |ui| {
            ui.label(
                egui::RichText::new(t!("home.conflict_modal_intro"))
                    .font(theme::body_md())
                    .color(theme::TEXT_MUT),
            );
            ui.add_space(theme::SPACE_7);
            for group in groups.iter_mut() {
                conflict_group_card(ui, group);
                ui.add_space(theme::SPACE_6);
            }
            ui.label(
                egui::RichText::new(t!("home.conflict_modal_note"))
                    .font(theme::body_sm())
                    .color(theme::TEXT_FAINT),
            );
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("home.conflict_keep_selected", count = count),
                widgets::ButtonKind::Warn,
                egui::vec2(150.0, 34.0),
            )
            .clicked()
            {
                apply = true;
            }
            ui.add_space(theme::SPACE_4);
            if widgets::button(
                ui,
                &t!("home.conflict_decide_later"),
                widgets::ButtonKind::Ghost,
                egui::vec2(110.0, 34.0),
            )
            .clicked()
            {
                cancel = true;
            }
        },
    );
    if apply {
        let groups = conflict_resolve.take().expect("dialog groups exist");
        for id in disables_for(&groups) {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::SetDeviceVisibility {
                    device_id: id,
                    state: VisibilityState::Disabled,
                },
            );
        }
    } else if cancel || dismissed {
        *conflict_resolve = None;
    }
}

/// One conflict group inside the resolve dialog: the contested device, a
/// "N sources" tally, and the radio owner options. Updates `group.pick`.
fn conflict_group_card(ui: &mut egui::Ui, group: &mut ConflictGroup) {
    egui::Frame::NONE
        .fill(theme::INNER_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::RADIUS_LG)
        .inner_margin(egui::Margin {
            left: 15,
            right: 15,
            top: 13,
            bottom: 14,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let head = group
                .devices
                .iter()
                .find(|d| d.id == group.recommended_id)
                .unwrap_or(&group.devices[0]);
            let dev_name = head.name.clone();
            let dev_type = head.device_type;
            let sources = group.devices.len();
            ui.horizontal(|ui| {
                let (badge, _) = ui.allocate_exact_size(Vec2::new(40.0, 30.0), Sense::hover());
                widgets::device_badge(ui.painter(), badge, dev_type);
                ui.add_space(theme::SPACE_6);
                ui.vertical(|ui| {
                    ui.add_space(theme::SPACE_1);
                    ui.label(
                        egui::RichText::new(&dev_name)
                            .font(theme::heading())
                            .color(theme::TEXT),
                    );
                    ui.label(
                        egui::RichText::new(model::device_type_label(dev_type))
                            .font(theme::caption())
                            .color(theme::TEXT_MUT),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    sources_pill(ui, sources);
                });
            });
            ui.add_space(theme::SPACE_6);
            let gap = 9.0;
            let side = ((ui.available_width() - gap) / 2.0).max(120.0);
            let mut picked = None;
            for chunk in group.devices.chunks(2) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for dev in chunk {
                        if owner_option(
                            ui,
                            dev,
                            dev.id == group.pick,
                            dev.id == group.recommended_id,
                            side,
                        ) {
                            picked = Some(dev.id.clone());
                        }
                    }
                });
                ui.add_space(gap);
            }
            if let Some(id) = picked {
                group.pick = id;
            }
        });
}

/// The amber "N sources" tally chip (dot + mono count) in a group header.
fn sources_pill(ui: &mut egui::Ui, sources: usize) {
    let text = t!("home.conflict_sources", count = sources);
    let font = theme::mono(10.0);
    let text_w = ui
        .painter()
        .layout_no_wrap(text.to_string(), font.clone(), theme::STAT_AMBER)
        .rect
        .width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(text_w + 21.0, 18.0), Sense::hover());
    let p = ui.painter();
    p.circle_filled(
        Pos2::new(rect.left() + 5.0, rect.center().y),
        3.0,
        theme::STAT_AMBER,
    );
    p.text(
        Pos2::new(rect.left() + 13.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        font,
        theme::STAT_AMBER,
    );
}

/// A single radio owner option inside a conflict group card. Returns `true`
/// when clicked (the caller then makes this device the group's pick).
fn owner_option(
    ui: &mut egui::Ui,
    device: &ConflictChoiceDevice,
    selected: bool,
    recommended: bool,
    width: f32,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 58.0), Sense::click());
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    let fill = if selected {
        a(theme::CYAN, 0.10)
    } else if response.hovered() {
        a(Color32::WHITE, 0.03)
    } else {
        theme::CARD_BG
    };
    p.rect_filled(rect, theme::RADIUS_MD, fill);
    p.rect_stroke(
        rect,
        theme::RADIUS_MD,
        Stroke::new(
            if selected { 1.5 } else { 1.0 },
            if selected { theme::CYAN } else { theme::BORDER },
        ),
        egui::StrokeKind::Middle,
    );
    let radio = Pos2::new(rect.left() + 18.0, rect.center().y);
    p.circle_stroke(
        radio,
        6.5,
        Stroke::new(
            1.5,
            if selected {
                theme::CYAN
            } else {
                theme::TEXT_FAINT
            },
        ),
    );
    if selected {
        p.circle_filled(radio, 3.3, theme::CYAN);
    }
    let tx = rect.left() + 34.0;
    let max_w = (rect.right() - 12.0 - tx).max(20.0);
    let name_font = theme::semibold(12.5);
    let tag_w = if recommended { 74.0 } else { 0.0 };
    let name = ellipsize(p, &device.name, &name_font, (max_w - tag_w).max(20.0));
    let nr = p.text(
        Pos2::new(tx, rect.center().y - 8.0),
        Align2::LEFT_CENTER,
        name,
        name_font,
        theme::TEXT,
    );
    if recommended {
        tag_pill(
            p,
            Pos2::new(nr.right() + 7.0, rect.center().y - 8.0),
            &t!("home.conflict_tag_recommended"),
            theme::CYAN,
        );
    }
    let source_font = theme::caption();
    let source = ellipsize(
        p,
        &conflict_source_label(&device.source),
        &source_font,
        max_w,
    );
    p.text(
        Pos2::new(tx, rect.center().y + 9.0),
        Align2::LEFT_CENTER,
        source,
        source_font,
        theme::TEXT_MUT,
    );
    response.clicked()
}

/// A small filled tag pill (label on a tinted rounded rect) centered vertically
/// at `left_center`.
fn tag_pill(p: &egui::Painter, left_center: Pos2, text: &str, color: Color32) {
    let font = theme::body(9.5);
    let text_w = p
        .layout_no_wrap(text.to_owned(), font.clone(), color)
        .rect
        .width();
    let rect = Rect::from_min_size(
        Pos2::new(left_center.x, left_center.y - 7.5),
        Vec2::new(text_w + 12.0, 15.0),
    );
    p.rect_filled(rect, 5.0, a(color, 0.14));
    p.text(rect.center(), Align2::CENTER_CENTER, text, font, color);
}

pub(super) fn ellipsize(
    painter: &egui::Painter,
    text: &str,
    font: &egui::FontId,
    max_width: f32,
) -> String {
    widgets::truncate_to_width(text, max_width, |s| {
        painter
            .layout_no_wrap(s.to_owned(), font.clone(), theme::TEXT)
            .rect
            .width()
    })
}

fn conflict_source_label(source: &ConflictDeviceSource) -> String {
    match source {
        ConflictDeviceSource::Builtin => t!("home.conflict_source_builtin").into_owned(),
        ConflictDeviceSource::Plugin(id) => {
            t!("home.conflict_source_plugin", name = id).into_owned()
        }
        ConflictDeviceSource::Integration(id) => {
            t!("home.conflict_source_integration", name = id).into_owned()
        }
    }
}

/// If `child_id` is a user-added (unlocked) link in some device's RGB chain,
/// returns that parent device id and the hosting channel id. Hardware-detected
/// (`locked`) links are controlled by the hardware and can't be removed, so
/// they yield `None`.
fn chain_parent(devices: &[WireDevice], child_id: &str) -> Option<(String, String)> {
    devices.iter().find_map(|p| {
        p.capabilities.iter().find_map(|c| match c {
            DeviceCapability::Lighting(r) => r.descriptor.channels.iter().find_map(|channel| {
                let halod_shared::types::LightingDivision::Divisible { segments, .. } =
                    &channel.division
                else {
                    return None;
                };
                segments
                    .iter()
                    .any(|segment| segment.device_id == child_id && !segment.locked)
                    .then(|| (p.id.clone(), channel.id.clone()))
            }),
            _ => None,
        })
    })
}

/// Confirm dialog shown before a chained device is unlinked. Rendered every
/// frame; a no-op until a removal is pending.
fn remove_confirm_modal(
    ctx: &egui::Context,
    cmd: &CommandTx,
    confirm_remove: &mut Option<ConfirmRemove>,
) {
    let Some(target) = confirm_remove.as_ref() else {
        return;
    };
    if let Some(target) = widgets::confirm_delete_dialog(
        ctx,
        "home_remove_chain",
        &t!("home.remove_device_title"),
        &t!("home.remove_device_confirm", name = target.child_name),
        &t!("home.remove"),
        confirm_remove,
    ) {
        crate::runtime::ipc::send(
            cmd,
            halod_shared::commands::DaemonCommand::LightingRemoveSegment {
                id: target.parent_id,
                channel_id: target.channel_id,
                device_id: target.child_id,
            },
        );
    }
}

/// Right-click menu for a device card/row: rename + visibility, all dispatched
/// to the daemon. The rename editor lives inline in the popover (per the
/// design), not in a separate modal.
fn card_menu(
    ui: &mut egui::Ui,
    d: &WireDevice,
    all_devices: &[WireDevice],
    cmd: &CommandTx,
    rename: &mut Option<Rename>,
    confirm_remove: &mut Option<ConfirmRemove>,
) {
    // Fix (not min) the width — a min-width menu here would grow every frame.
    ui.set_width(186.0);
    let inner_w = ui.available_width();

    if let Some(r) = rename.as_mut().filter(|r| r.id == d.id) {
        let edit = ui.add(
            egui::TextEdit::singleline(&mut r.buf)
                .hint_text(t!("home.device_name"))
                .desired_width(inner_w)
                .margin(egui::vec2(9.0, 8.0)),
        );
        edit.request_focus();
        let mut save = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let mut cancel = false;
        ui.add_space(theme::SPACE_4);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let bw = egui::vec2((inner_w - 6.0) / 2.0, 28.0);
            save |=
                widgets::button(ui, &t!("home.save"), widgets::ButtonKind::Primary, bw).clicked();
            cancel =
                widgets::button(ui, &t!("home.cancel"), widgets::ButtonKind::Ghost, bw).clicked();
        });
        let trimmed = r.buf.trim().to_string();
        if save && !trimmed.is_empty() {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::SetDeviceName {
                    device_id: d.id.clone(),
                    name: trimmed,
                },
            );
            *rename = None;
            ui.close();
        } else if cancel {
            *rename = None;
            ui.close();
        }
        return;
    }

    widgets::context_menu_title(ui, &d.name);
    if widgets::context_menu_item(ui, &t!("home.rename_device"), theme::TEXT).clicked() {
        *rename = Some(Rename {
            id: d.id.clone(),
            buf: d.name.clone(),
        });
    }
    match d.active_state {
        VisibilityState::Visible => {
            if widgets::context_menu_item(ui, &t!("home.hide_device"), theme::TEXT).clicked() {
                set_vis(ui, cmd, &d.id, VisibilityState::Hidden);
            }
            if widgets::context_menu_item(ui, &t!("home.disable_device"), theme::OFFLINE_TEXT)
                .clicked()
            {
                set_vis(ui, cmd, &d.id, VisibilityState::Disabled);
            }
        }
        VisibilityState::Hidden => {
            if widgets::context_menu_item(ui, &t!("home.show_device"), theme::TEXT).clicked() {
                set_vis(ui, cmd, &d.id, VisibilityState::Visible);
            }
            if widgets::context_menu_item(ui, &t!("home.disable_device"), theme::OFFLINE_TEXT)
                .clicked()
            {
                set_vis(ui, cmd, &d.id, VisibilityState::Disabled);
            }
        }
        VisibilityState::Disabled => {
            if widgets::context_menu_item(ui, &t!("home.enable_device"), theme::TEXT).clicked() {
                set_vis(ui, cmd, &d.id, VisibilityState::Visible);
            }
        }
    }
    if let Some((parent_id, channel_id)) = chain_parent(all_devices, &d.id) {
        if widgets::context_menu_item(ui, &t!("home.remove_device"), theme::OFFLINE_TEXT).clicked()
        {
            *confirm_remove = Some(ConfirmRemove {
                child_id: d.id.clone(),
                child_name: d.name.clone(),
                parent_id,
                channel_id,
            });
            ui.close();
        }
    }
}

fn set_vis(ui: &mut egui::Ui, cmd: &CommandTx, id: &str, state: VisibilityState) {
    crate::runtime::ipc::send(
        cmd,
        halod_shared::commands::DaemonCommand::SetDeviceVisibility {
            device_id: id.to_string(),
            state,
        },
    );
    ui.close();
}

fn header(ui: &mut egui::Ui, state: &TopicStore, home: &mut HomeUi) {
    let total = state.devices.iter().filter(|d| model::listable(d)).count();
    let online = state
        .devices
        .iter()
        .filter(|d| model::listable(d) && d.connected)
        .count();
    let attention = total - online;
    let hidden = state
        .devices
        .iter()
        .filter(|d| model::listable(d) && model::is_hidden(d))
        .count();

    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            let sub = if attention > 0 {
                t!(
                    "home.devices_connected_attention",
                    online = online,
                    total = total,
                    attention = attention
                )
            } else {
                t!("home.devices_connected", online = online, total = total)
            };
            ui.label(
                egui::RichText::new(sub)
                    .font(theme::body_lg())
                    .color(theme::TEXT_MUT),
            );
        });

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            segmented(ui, &mut home.variant);
            ui.add_space(theme::SPACE_5);
            if hidden > 0 {
                let label = if home.show_hidden {
                    t!("home.hide_hidden")
                } else {
                    t!("home.show_hidden")
                };
                let clicked = widgets::pill(ui, &label, home.show_hidden);
                crate::domain::tour::anchor(
                    ui.ctx(),
                    crate::domain::tour::AnchorId::HomeShowHidden,
                    ui.min_rect(),
                );
                if clicked {
                    home.show_hidden = !home.show_hidden;
                }
                ui.add_space(theme::SPACE_5);
            }
            let customizing = home.widgets.customizing();
            let label = if customizing {
                t!("home.customize_done")
            } else {
                t!("home.customize")
            };
            if widgets::pill(ui, &label, customizing) {
                home.widgets.toggle(state);
            }
            ui.add_space(theme::SPACE_5);
            search_box(ui, &mut home.search);
        });
    });
}

/// A rounded search field that filters the device list by name or vendor.
fn search_box(ui: &mut egui::Ui, search: &mut String) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(190.0, 33.0), Sense::hover());
    crate::domain::tour::anchor(ui.ctx(), crate::domain::tour::AnchorId::HomeSearch, rect);
    theme::paint_card_rect(ui.painter(), rect, theme::RADIUS_MD);
    // Magnifier glyph.
    let icon = Pos2::new(rect.left() + 13.0, rect.center().y);
    ui.painter()
        .circle_stroke(icon, 4.0, Stroke::new(1.3, theme::TEXT_FAINT));
    ui.painter().line_segment(
        [
            Pos2::new(icon.x + 3.0, icon.y + 3.0),
            Pos2::new(icon.x + 6.5, icon.y + 6.5),
        ],
        Stroke::new(1.3, theme::TEXT_FAINT),
    );
    let field = Rect::from_min_max(
        Pos2::new(rect.left() + 24.0, rect.top()),
        Pos2::new(rect.right() - 8.0, rect.bottom()),
    );
    let mut field_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(field)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    field_ui.add(
        egui::TextEdit::singleline(search)
            .frame(egui::Frame::NONE)
            .desired_width(f32::INFINITY)
            .font(theme::body_md())
            .hint_text(t!("home.search_devices")),
    );
}

/// Grid/List segmented control, drawn on a single allocated rect so it never
/// wraps regardless of the surrounding layout.
fn segmented(ui: &mut egui::Ui, variant: &mut Variant) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(108.0, 33.0), Sense::hover());
    let p = ui.painter();
    theme::paint_card_rect(p, rect, theme::RADIUS_MD);
    for (i, (v, label)) in [
        (Variant::Grid, t!("home.grid")),
        (Variant::List, t!("home.list")),
    ]
    .into_iter()
    .enumerate()
    {
        let chip = Rect::from_min_size(
            Pos2::new(rect.left() + 4.0 + i as f32 * 50.0, rect.top() + 4.0),
            Vec2::new(48.0, 25.0),
        );
        let active = *variant == v;
        let resp = ui.interact(chip, ui.id().with(("seg", i)), Sense::click());
        if active {
            ui.painter()
                .rect_filled(chip, theme::RADIUS_SM, theme::CYAN);
        } else {
            let t =
                ui.ctx()
                    .animate_bool_with_time(ui.id().with(("seg_h", i)), resp.hovered(), 0.12);
            if t > 0.001 {
                ui.painter()
                    .rect_filled(chip, theme::RADIUS_SM, a(Color32::WHITE, 0.05 * t));
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
        ui.painter().text(
            chip.center(),
            Align2::CENTER_CENTER,
            label,
            if active {
                theme::subhead()
            } else {
                theme::body_md()
            },
            if active {
                theme::hex(0x0a0d13)
            } else {
                theme::TEXT_DIM
            },
        );
        if resp.clicked() {
            *variant = v;
        }
    }
}

// Sensor cards with sparklines, drawn on the Cooling page.
pub(crate) fn sensors_grid(
    ui: &mut egui::Ui,
    state: &TopicStore,
    history: &HashMap<String, VecDeque<f32>>,
    cols: usize,
) {
    let sensors = crate::domain::models::sensors::sensors(state);
    if sensors.is_empty() {
        return;
    }
    let w = (ui.available_width() - GAP * (cols as f32 - 1.0)) / cols as f32;
    for (row, chunk) in sensors.chunks(cols).enumerate() {
        if row > 0 {
            ui.add_space(GAP);
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for (col, s) in chunk.iter().enumerate() {
                let color = theme::sensor_hue(row * cols + col);
                sensor_card(ui, s, color, w, history);
            }
        });
    }
}

fn sensor_card(
    ui: &mut egui::Ui,
    s: &crate::domain::models::sensors::SensorView,
    color: Color32,
    w: f32,
    history: &HashMap<String, VecDeque<f32>>,
) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 82.0), Sense::hover());
    let t =
        ui.ctx()
            .animate_bool_with_time(egui::Id::new(("sensor_h", &s.id)), resp.hovered(), 0.15);
    let p = ui.painter();
    if t > 0.0 {
        theme::gentle_glow_ellipse(p, rect.center(), w * 0.68, 68.0, color, 0.10 * t);
    }
    p.rect_filled(rect, theme::RADIUS_LG, theme::CARD_BG);

    // Background sparkline from the rolling history.
    if let Some(samples) = history.get(&s.id) {
        let samples: Vec<f32> = samples.iter().copied().collect();
        sparkline(&p.with_clip_rect(rect), rect, &samples, color);
    }
    // The sparkline fill bleeds into the rounded corners; mask them back.
    theme::round_corners(p, rect, 12.0, theme::MAIN_BG);
    p.rect_stroke(
        rect,
        theme::RADIUS_LG,
        Stroke::new(1.0, theme::lerp_color(theme::BORDER, a(color, 0.5), t)),
        egui::StrokeKind::Middle,
    );

    // Label row: colored dot + name.
    let dot = Pos2::new(rect.left() + 18.0 + 2.5, rect.top() + 18.0);
    p.circle_filled(dot, 2.5, color);
    p.text(
        Pos2::new(rect.left() + 28.0, rect.top() + 18.0),
        Align2::LEFT_CENTER,
        &s.label,
        theme::body_sm(),
        theme::TEXT_MUT,
    );
    // Value + unit, with trend at the right.
    let vrect = p.text(
        Pos2::new(rect.left() + 18.0, rect.top() + 34.0),
        Align2::LEFT_TOP,
        format!("{:.0}", s.value),
        theme::mono_bold(24.0),
        color,
    );
    p.text(
        Pos2::new(vrect.right() + 6.0, vrect.bottom() - 4.0),
        Align2::LEFT_BOTTOM,
        s.unit,
        theme::body_md(),
        theme::TEXT_FAINT,
    );
    if let Some(trend) = history
        .get(&s.id)
        .and_then(|h| trend_label(&h.iter().copied().collect::<Vec<_>>(), s.unit))
    {
        p.text(
            Pos2::new(rect.right() - 14.0, vrect.bottom() - 4.0),
            Align2::RIGHT_BOTTOM,
            trend,
            theme::mono(10.0),
            theme::TEXT_FAINT,
        );
    }
}

/// Signed change across the history window, suffixed with the sensor's unit —
/// e.g. `"+3°C"`, `"-1%"`, `"+150MHz"`.
fn trend_label(samples: &[f32], unit: &str) -> Option<String> {
    if samples.len() < 2 {
        return None;
    }
    let delta = (samples[samples.len() - 1] - samples[0]).round() as i32;
    Some(match delta {
        0 => format!("0{unit}"),
        d if d > 0 => format!("+{d}{unit}"),
        d => format!("{d}{unit}"),
    })
}

/// Draw a filled sparkline across the bottom band of `card`.
pub(crate) fn sparkline(p: &egui::Painter, card: Rect, samples: &[f32], color: Color32) {
    let pts = sparkline_points(card, samples);
    if pts.len() < 2 {
        return;
    }
    widgets::fill_under_line(p, &pts, card.bottom(), a(color, 0.13));
    p.add(egui::Shape::line(pts, Stroke::new(1.5, color)));
}

/// Min/max of `samples`, the auto-scale range the sparkline band maps onto.
fn sparkline_scale(samples: &[f32]) -> (f32, f32) {
    samples
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)))
}

/// Map `samples` to polyline points across the bottom band of `card`. Returns
/// empty when there are too few samples to draw a line.
pub(crate) fn sparkline_points(card: Rect, samples: &[f32]) -> Vec<Pos2> {
    if samples.len() < 2 {
        return Vec::new();
    }
    let (mn, mx) = sparkline_scale(samples);
    let band = 50.0;
    let top = card.bottom() - band;
    let n = samples.len();
    samples
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = card.left() + (i as f32 / (n - 1) as f32) * card.width();
            Pos2::new(x, sparkline_value_y(top, band, v, mn, mx))
        })
        .collect()
}

/// The y coordinate `value` maps to under `samples`' auto-scale, using the
/// same band mapping as [`sparkline_points`]. Lets a caller draw a reference
/// line (e.g. a declared ceiling) consistent with the plotted series.
/// Clamped to the band so a reference far outside the current sample range
/// (e.g. a ceiling well above recent usage) still draws at the band's edge
/// instead of off-screen.
pub(crate) fn sparkline_reference_y(card: Rect, value: f32, samples: &[f32]) -> f32 {
    let (mn, mx) = sparkline_scale(samples);
    let band = 50.0;
    let top = card.bottom() - band;
    sparkline_value_y(top, band, value, mn, mx).clamp(top, card.bottom())
}

fn sparkline_value_y(top: f32, band: f32, value: f32, mn: f32, mx: f32) -> f32 {
    let rng = (mx - mn).max(1.0);
    top + (band - 6.0) - ((value - mn) / rng) * (band - 14.0)
}

fn grid(
    ui: &mut egui::Ui,
    devices: &[&WireDevice],
    all_devices: &[WireDevice],
    cmd: &CommandTx,
    rename: &mut Option<Rename>,
    confirm_remove: &mut Option<ConfirmRemove>,
    conflict_resolve: &mut Option<Vec<ConflictGroup>>,
    page: &mut crate::domain::state::Page,
) {
    let avail = ui.available_width();
    let cols = (((avail + GAP) / (236.0 + GAP)).floor() as usize).clamp(1, 4);
    let w = (avail - GAP * (cols - 1) as f32) / cols as f32;
    let h = 178.0;

    let mut anchored_a_card = false;
    for row in devices.chunks(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for d in row {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
                if !anchored_a_card {
                    crate::domain::tour::anchor(
                        ui.ctx(),
                        crate::domain::tour::AnchorId::HomeDeviceCard,
                        rect,
                    );
                    anchored_a_card = true;
                }
                let conflict_clicked =
                    device_card(ui, rect, d, all_devices, conflict_resolve, resp.hovered());
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if resp.clicked()
                    && !conflict_clicked
                    && d.active_state != VisibilityState::Disabled
                {
                    *page = crate::domain::state::Page::Device(d.id.clone());
                }
                resp.context_menu(|ui| card_menu(ui, d, all_devices, cmd, rename, confirm_remove));
            }
        });
        ui.add_space(GAP);
    }
}

fn draw_card_glow(ui: &egui::Ui, rect: Rect, d: &WireDevice, color: Color32, t: f32) {
    let p = ui.painter();
    let glow_rx = egui::lerp(rect.width() * 0.32..=rect.width() * 0.52, t);
    let glow_ry = egui::lerp(44.0..=80.0, t);
    let glow_s = egui::lerp(0.088..=0.22, t);
    let badge_r = egui::lerp(16.0..=30.0, t);
    let badge_s = egui::lerp(0.154..=0.36, t);

    p.rect_filled(rect, theme::RADIUS_XL, theme::CARD_BG);
    let anchor = Pos2::new(rect.right(), rect.top());
    let animating = t > 0.002;
    let time = ui.input(|i| i.time) as f32;
    {
        let clip = p.with_clip_rect(rect);
        theme::glow_ellipse(&clip, anchor, glow_rx, glow_ry, color, glow_s);
        // On hover the glow comes alive: hue-shifted blobs drift around the
        // top-right corner like an aurora.
        if animating {
            theme::aurora(
                &clip,
                anchor,
                glow_rx * 0.72,
                glow_ry * 0.92,
                rect.width(),
                color,
                t,
                time,
            );
        }
    }

    let chip = Rect::from_min_size(
        Pos2::new(rect.left() + 16.0, rect.top() + 16.0),
        Vec2::new(44.0, 32.0),
    );
    theme::glow(p, chip.center(), badge_r, color, badge_s);
    if animating {
        let clip = p.with_clip_rect(chip.expand(badge_r));
        theme::aurora(
            &clip,
            chip.center(),
            badge_r * 0.9,
            badge_r * 0.9,
            badge_r * 1.6,
            color,
            t,
            time,
        );
    }
    widgets::device_badge(p, chip, d.device_type);

    if animating {
        ui.ctx().request_repaint();
    }
}

fn device_card(
    ui: &mut egui::Ui,
    rect: Rect,
    d: &WireDevice,
    all_devices: &[WireDevice],
    conflict_resolve: &mut Option<Vec<ConflictGroup>>,
    hovered: bool,
) -> bool {
    let color = theme::device_color(d);
    // Smoothly animate hover state in [0,1] for halo growth + lift.
    let t = ui
        .ctx()
        .animate_bool_with_time(egui::Id::new(("card", &d.id)), hovered, 0.18);

    // Hover lifts the card by up to 2px (design: translateY(-2px)).
    let rect = rect.translate(Vec2::new(0.0, -2.0 * t));
    // Offline, hidden and disabled devices are greyed out (via a scrim below).
    let dimmed = !d.connected || d.active_state != VisibilityState::Visible;

    draw_card_glow(ui, rect, d, color, t);
    let p = ui.painter();

    // Top-right: battery hint when present, otherwise a status dot + label.
    let chip_cy = rect.top() + 22.0;
    if d.connected {
        if let Some((level, charging)) = model::battery(d) {
            battery_hint(p, rect.right() - 16.0, chip_cy, level, charging);
        } else {
            // Non-battery devices show their transport (HID / SMBus / …).
            status_chip(
                p,
                rect.right() - 16.0,
                chip_cy,
                theme::ONLINE,
                &model::transport_label(d),
                theme::TEXT,
            );
        }
    } else {
        status_chip(
            p,
            rect.right() - 16.0,
            chip_cy,
            theme::OFFLINE,
            &t!("home.offline"),
            theme::OFFLINE_TEXT,
        );
    }

    // Metrics (bottom), then type + name above them.
    let metrics = model::metrics(d);
    let mini_h = 50.0;
    let metrics_top = rect.bottom() - 16.0 - mini_h;
    if !metrics.is_empty() {
        metric_row(p, rect, metrics_top, mini_h, &metrics);
    }
    let type_y = if metrics.is_empty() {
        rect.bottom() - 16.0 - 13.0
    } else {
        metrics_top - 13.0 - 5.0
    };
    p.text(
        Pos2::new(rect.left() + 16.0, type_y),
        Align2::LEFT_TOP,
        model::type_label(d),
        theme::body_sm(),
        theme::TEXT_MUT,
    );
    let name_font = theme::heading();
    let display_name = ellipsize(p, &d.name, &name_font, rect.width() - 32.0);
    p.text(
        Pos2::new(rect.left() + 16.0, type_y - 6.0 - 16.0),
        Align2::LEFT_BOTTOM,
        display_name,
        name_font,
        theme::TEXT,
    );

    // Grey out hidden/disabled/offline devices with a uniform scrim.
    if dimmed {
        p.rect_filled(rect, theme::RADIUS_XL, a(theme::MAIN_BG, 0.5));
    }

    // Restore the rounded silhouette (the clipped glow filled the corners),
    // then stroke the border — accent on hover, animated.
    theme::round_corners(p, rect, 14.0, theme::MAIN_BG);
    let border = theme::lerp_color(theme::BORDER, a(color, 0.55), t);
    p.rect_stroke(
        rect,
        theme::RADIUS_XL,
        Stroke::new(1.0, border),
        egui::StrokeKind::Middle,
    );
    conflict_control(ui, rect, d, all_devices, conflict_resolve)
}

fn conflict_control(
    ui: &mut egui::Ui,
    rect: Rect,
    d: &WireDevice,
    all_devices: &[WireDevice],
    conflict_resolve: &mut Option<Vec<ConflictGroup>>,
) -> bool {
    let Some(conflict) = model::conflict_presentation(d, all_devices) else {
        return false;
    };
    let label = if conflict.is_recommended {
        t!("home.conflict_recommended").into_owned()
    } else if conflict.confidence == halod_shared::types::ConflictConfidence::Confirmed {
        t!("home.conflict_confirmed").into_owned()
    } else {
        t!("home.conflict_possible").into_owned()
    };
    let y = rect.top() + 43.0;
    let chip = Rect::from_min_size(
        Pos2::new(rect.right() - 106.0, y - 9.0),
        Vec2::new(96.0, 18.0),
    );
    let response = ui.interact(chip, egui::Id::new(("conflict", &d.id)), Sense::click());
    let color = if conflict.confidence == halod_shared::types::ConflictConfidence::Confirmed {
        theme::OFFLINE_TEXT
    } else {
        theme::TEXT_MUT
    };
    ui.painter().text(
        chip.right_top(),
        Align2::RIGHT_TOP,
        &label,
        theme::micro(),
        color,
    );
    let clicked = response.clicked();
    response.on_hover_text(if conflict.is_recommended {
        t!("home.conflict_owner", name = conflict.recommended_name)
    } else {
        t!("home.conflict_with", name = conflict.peer_names.join(", "))
    });
    if clicked {
        *conflict_resolve = Some(conflict_groups(all_devices));
    }
    clicked
}

fn metric_row(p: &egui::Painter, rect: Rect, top: f32, h: f32, metrics: &[Metric]) {
    let n = metrics.len();
    let inner = rect.width() - 32.0;
    let gap = 8.0;
    let w = (inner - gap * (n as f32 - 1.0)) / n as f32;
    for (i, m) in metrics.iter().enumerate() {
        let x = rect.left() + 16.0 + (w + gap) * i as f32;
        let mr = Rect::from_min_size(Pos2::new(x, top), Vec2::new(w, h));
        p.rect_filled(mr, 8.0, theme::INNER_BG);
        p.rect_stroke(
            mr,
            8.0,
            Stroke::new(1.0, theme::BORDER_INNER),
            egui::StrokeKind::Middle,
        );
        p.text(
            Pos2::new(mr.left() + 9.0, mr.top() + 9.0),
            Align2::LEFT_TOP,
            &m.label,
            theme::body(9.5),
            theme::TEXT_MUT,
        );
        p.text(
            Pos2::new(mr.left() + 9.0, mr.top() + 23.0),
            Align2::LEFT_TOP,
            &m.value,
            theme::mono_semibold(12.5),
            theme::TEXT_BRIGHT,
        );
    }
}

/// Status dot + label, right-aligned with its right edge at `right`.
fn status_chip(p: &egui::Painter, right: f32, cy: f32, dot: Color32, text: &str, color: Color32) {
    let st = p.text(
        Pos2::new(right, cy),
        Align2::RIGHT_CENTER,
        text,
        theme::caption(),
        color,
    );
    p.circle_filled(Pos2::new(st.left() - 8.0, cy), 3.0, dot);
}

/// A small battery glyph (outlined cell + fill + terminal nub) followed by the
/// percentage, right-aligned with its right edge at `right`.
fn battery_hint(p: &egui::Painter, right: f32, cy: f32, level: u8, charging: bool) {
    let color = theme::battery_color(level, charging);
    let pct = format!("{level}%");
    let txt = p.text(
        Pos2::new(right, cy),
        Align2::RIGHT_CENTER,
        &pct,
        theme::mono_semibold(11.0),
        theme::hex(0xcfd6e4),
    );

    let body = Rect::from_min_size(
        Pos2::new(txt.left() - 6.0 - 2.0 - 20.0, cy - 5.0),
        Vec2::new(20.0, 10.0),
    );
    widgets::battery_glyph(p, body, level, color);
}

fn list(
    ui: &mut egui::Ui,
    devices: &[&WireDevice],
    all_devices: &[WireDevice],
    cmd: &CommandTx,
    rename: &mut Option<Rename>,
    confirm_remove: &mut Option<ConfirmRemove>,
    conflict_resolve: &mut Option<Vec<ConflictGroup>>,
    page: &mut crate::domain::state::Page,
) {
    egui::Frame::NONE
        .fill(theme::CARD_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::RADIUS_XL)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (i, d) in devices.iter().enumerate() {
                let (rect, resp) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 58.0), Sense::click());
                if i == 0 {
                    crate::domain::tour::anchor(
                        ui.ctx(),
                        crate::domain::tour::AnchorId::HomeDeviceCard,
                        rect,
                    );
                }
                let conflict_clicked = list_row(
                    ui,
                    rect,
                    d,
                    all_devices,
                    conflict_resolve,
                    i + 1 < devices.len(),
                    resp.hovered(),
                );
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if resp.clicked()
                    && !conflict_clicked
                    && d.active_state != VisibilityState::Disabled
                {
                    *page = crate::domain::state::Page::Device(d.id.clone());
                }
                resp.context_menu(|ui| card_menu(ui, d, all_devices, cmd, rename, confirm_remove));
            }
        });
}

fn list_row(
    ui: &mut egui::Ui,
    rect: Rect,
    d: &WireDevice,
    all_devices: &[WireDevice],
    conflict_resolve: &mut Option<Vec<ConflictGroup>>,
    divider: bool,
    hovered: bool,
) -> bool {
    let p = ui.painter();
    if hovered {
        p.rect_filled(rect, 0.0, a(Color32::WHITE, 0.025));
    }
    if divider {
        p.line_segment(
            [
                Pos2::new(rect.left() + 18.0, rect.bottom()),
                Pos2::new(rect.right() - 18.0, rect.bottom()),
            ],
            Stroke::new(1.0, theme::BORDER_SOFT),
        );
    }
    let chip = Rect::from_min_size(
        Pos2::new(rect.left() + 18.0, rect.center().y - 16.0),
        Vec2::new(46.0, 32.0),
    );
    widgets::device_badge(p, chip, d.device_type);

    p.text(
        Pos2::new(chip.right() + 14.0, rect.center().y - 8.0),
        Align2::LEFT_CENTER,
        &d.name,
        theme::heading(),
        theme::TEXT,
    );
    p.text(
        Pos2::new(chip.right() + 14.0, rect.center().y + 9.0),
        Align2::LEFT_CENTER,
        model::type_label(d),
        theme::body_sm(),
        theme::TEXT_MUT,
    );

    // Metrics, mid.
    let mut mx = chip.right() + 220.0;
    for m in model::metrics(d) {
        p.text(
            Pos2::new(mx, rect.center().y - 8.0),
            Align2::LEFT_CENTER,
            &m.label,
            theme::caption(),
            theme::TEXT_MUT,
        );
        p.text(
            Pos2::new(mx, rect.center().y + 9.0),
            Align2::LEFT_CENTER,
            &m.value,
            theme::mono_semibold(12.5),
            theme::TEXT_BRIGHT,
        );
        mx += 110.0;
    }

    if !d.connected {
        p.rect_filled(rect, 0.0, a(theme::MAIN_BG, 0.35));
    }
    conflict_control(
        ui,
        rect.translate(Vec2::new(0.0, 16.0)),
        d,
        all_devices,
        conflict_resolve,
    )
}

fn empty(ui: &mut egui::Ui, filtered: bool) {
    let (title, sub) = if filtered {
        (t!("home.no_matching"), t!("home.no_matching_sub"))
    } else {
        (t!("home.no_devices"), t!("home.no_devices_sub"))
    };
    widgets::empty_state(ui, &title, Some(sub.as_ref()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub_with_links(links: Vec<halod_shared::types::LightingSegmentInfo>) -> WireDevice {
        use halod_shared::types::*;
        WireDevice {
            id: "hub1".into(),
            name: "Hub".into(),
            vendor: "v".into(),
            model: "m".into(),
            device_type: DeviceType::Other,
            connected: true,
            capabilities: vec![DeviceCapability::Lighting(LightingStatus {
                descriptor: LightingDescriptor {
                    channels: vec![LightingChannel {
                        id: "h1".into(),
                        name: "Header 1".into(),
                        topology: ZoneTopology::Linear,
                        leds: vec![],
                        color_order: Default::default(),
                        division: LightingDivision::Divisible {
                            max_leds: 120,
                            segments: links,
                        },
                        visibility: Default::default(),
                    }],
                    native_effects: vec![],
                },
                state: None,
                channel_transforms: Default::default(),
            })],
            active_state: Default::default(),
            connection_type: None,
            serial_number: None,
            transport: None,
            write_rate: Default::default(),
            control_layout: Vec::new(),
            integration_id: None,
            conflict: None,
        }
    }

    fn link(child: &str, locked: bool) -> halod_shared::types::LightingSegmentInfo {
        use halod_shared::types::*;
        LightingSegmentInfo {
            device_id: child.into(),
            channel_id: "lighting".into(),
            name: "Strip".into(),
            topology: ZoneTopology::Linear,
            led_count: 30,
            color_order: None,
            locked,
        }
    }

    #[test]
    fn chain_parent_finds_unlocked_link_parent_and_channel() {
        let devices = vec![hub_with_links(vec![link("child1", false)])];
        assert_eq!(
            chain_parent(&devices, "child1"),
            Some(("hub1".into(), "h1".into()))
        );
    }

    #[test]
    fn chain_parent_ignores_locked_and_unknown_children() {
        let devices = vec![hub_with_links(vec![link("child1", true)])];
        // Hardware-detected (locked) links are not removable.
        assert_eq!(chain_parent(&devices, "child1"), None);
        // A device that is nobody's child yields nothing.
        assert_eq!(chain_parent(&devices, "stranger"), None);
    }

    fn group(ids: &[(&str, &str)], recommended: &str) -> ConflictGroup {
        ConflictGroup {
            devices: ids
                .iter()
                .map(|(id, name)| ConflictChoiceDevice {
                    id: (*id).into(),
                    name: (*name).into(),
                    device_type: DeviceType::Mouse,
                    source: ConflictDeviceSource::Builtin,
                })
                .collect(),
            recommended_id: recommended.into(),
            pick: recommended.into(),
        }
    }

    /// A minimal device carrying a conflict with `peers`, for group building.
    fn dev_with_conflict(id: &str, peers: &[&str], recommended: &str) -> WireDevice {
        use halod_shared::types::*;
        let mut d = hub_with_links(vec![]);
        d.id = id.into();
        d.name = id.to_uppercase();
        d.conflict = Some(DeviceConflictSummary {
            peer_ids: peers.iter().map(|p| (*p).to_string()).collect(),
            recommended_id: recommended.into(),
            confidence: ConflictConfidence::Confirmed,
            participants: vec![],
        });
        d
    }

    #[test]
    fn keeping_one_conflict_owner_disables_every_other_participant() {
        let group = group(
            &[("builtin", "Built-in host device"), ("openrgb", "OpenRGB")],
            "builtin",
        );
        assert_eq!(ids_to_disable(&group, "builtin"), vec!["openrgb"]);
        assert_eq!(ids_to_disable(&group, "openrgb"), vec!["builtin"]);
    }

    #[test]
    fn conflict_groups_dedupe_participants_into_one_group_each() {
        // Two devices describing the *same* pair collapse to one group; a third
        // unrelated pair is its own group.
        let devices = vec![
            dev_with_conflict("a", &["b"], "a"),
            dev_with_conflict("b", &["a"], "a"),
            dev_with_conflict("c", &["d"], "c"),
            dev_with_conflict("d", &["c"], "c"),
        ];
        assert_eq!(conflict_group_count(&devices), 2);
        let groups = conflict_groups(&devices);
        assert_eq!(groups.len(), 2);
        // Each group carries both of its participants and defaults its pick to
        // the recommended owner.
        assert_eq!(groups[0].devices.len(), 2);
        assert_eq!(groups[0].pick, groups[0].recommended_id);
    }

    #[test]
    fn disables_for_keeps_each_groups_pick_and_disables_the_rest() {
        let groups = vec![group(&[("a", "A"), ("b", "B")], "a"), {
            let mut g = group(&[("c", "C"), ("d", "D")], "c");
            g.pick = "d".into(); // user overrode the recommendation here
            g
        }];
        // Group 1 keeps its recommended 'a'; group 2 keeps the overridden 'd'.
        assert_eq!(disables_for(&groups), vec!["b", "c"]);
    }

    #[test]
    fn trend_label_needs_two_samples() {
        assert_eq!(trend_label(&[], "°C"), None);
        assert_eq!(trend_label(&[42.0], "°C"), None);
    }

    #[test]
    fn trend_label_signs_and_rounding() {
        // Uses first..last delta, rounded to the nearest integer, suffixed with unit.
        assert_eq!(trend_label(&[20.0, 23.4], "°C").as_deref(), Some("+3°C"));
        assert_eq!(trend_label(&[20.0, 23.6], "°C").as_deref(), Some("+4°C"));
        assert_eq!(trend_label(&[25.0, 24.0], "°C").as_deref(), Some("-1°C"));
        assert_eq!(trend_label(&[25.0, 25.2], "°C").as_deref(), Some("0°C"));
        assert_eq!(trend_label(&[25.0, 25.0], "°C").as_deref(), Some("0°C"));
        // Only endpoints matter, not the spread between them.
        assert_eq!(
            trend_label(&[20.0, 99.0, 22.0], "°C").as_deref(),
            Some("+2°C")
        );
        // Non-temperature units carry their own suffix.
        assert_eq!(trend_label(&[10.0, 15.0], "%").as_deref(), Some("+5%"));
    }

    #[test]
    fn sparkline_points_empty_when_too_few() {
        let card = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 82.0));
        assert!(sparkline_points(card, &[]).is_empty());
        assert!(sparkline_points(card, &[1.0]).is_empty());
    }

    #[test]
    fn sparkline_points_span_card_width_and_stay_in_band() {
        let card = Rect::from_min_size(Pos2::new(10.0, 5.0), Vec2::new(120.0, 82.0));
        let pts = sparkline_points(card, &[0.0, 10.0, 5.0, 20.0]);
        assert_eq!(pts.len(), 4);
        // First x at the left edge, last x at the right edge.
        assert!((pts.first().unwrap().x - card.left()).abs() < 1e-3);
        assert!((pts.last().unwrap().x - card.right()).abs() < 1e-3);
        // All y values sit within the bottom 50px band of the card.
        for p in &pts {
            assert!(p.y >= card.bottom() - 50.0 && p.y <= card.bottom());
        }
    }

    #[test]
    fn sparkline_points_flat_series_does_not_divide_by_zero() {
        let card = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 82.0));
        let pts = sparkline_points(card, &[7.0, 7.0, 7.0]);
        assert_eq!(pts.len(), 3);
        assert!(pts.iter().all(|p| p.y.is_finite()));
    }

    #[test]
    fn sparkline_reference_y_matches_plotted_point_at_same_value() {
        let card = Rect::from_min_size(Pos2::new(10.0, 5.0), Vec2::new(120.0, 82.0));
        let samples = [0.0, 10.0, 5.0, 20.0];
        let pts = sparkline_points(card, &samples);
        // The reference y for a value equal to an existing sample must land
        // on that sample's own plotted y (same scale, same mapping).
        let y = sparkline_reference_y(card, samples[1], &samples);
        assert!((y - pts[1].y).abs() < 1e-3);
    }

    #[test]
    fn sparkline_reference_y_above_max_stays_within_the_band() {
        let card = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 82.0));
        let samples = [1.0, 2.0, 3.0];
        let y = sparkline_reference_y(card, 100.0, &samples);
        assert!(y.is_finite());
        assert!(y >= card.bottom() - 50.0 && y <= card.bottom());
    }
}
