// SPDX-License-Identifier: GPL-3.0-or-later
//! The Home screen main panel: greeting + summary, live sensor sparklines, and
//! the device grid/list. All data comes from the live daemon state.

use crate::ui::components as widgets;
use std::collections::{HashMap, HashSet, VecDeque};

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{
    AppState, ConflictDeviceSource, DeviceCapability, DeviceType, VisibilityState, WireDevice,
};

use crate::domain::models::device::{self as model, Metric};
use crate::domain::state::{Rename, Variant};
use crate::runtime::ipc::CommandTx;
use crate::ui::theme::{self, a};

const GAP: f32 = 16.0;

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    cmd: &CommandTx,
    show_hidden: &mut bool,
    variant: &mut Variant,
    search: &mut String,
    rename: &mut Option<Rename>,
    confirm_remove: &mut Option<ConfirmRemove>,
    conflict_choice: &mut Option<ConflictChoice>,
    conflict_prompted: &mut HashSet<String>,
    history: &HashMap<String, VecDeque<f32>>,
    page: &mut crate::domain::state::Page,
) {
    open_new_conflict_prompt(state, conflict_choice, conflict_prompted);
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
                    header(ui, state, show_hidden, variant, search);
                    let has_sensors =
                        !crate::domain::models::sensors::sensors(state, *show_hidden).is_empty();
                    ui.add_space(if has_sensors { 24.0 } else { 14.0 });
                    sensors_row(ui, state, cmd, *show_hidden, history);
                    if has_sensors {
                        ui.add_space(26.0);
                    }

                    let devices: Vec<&WireDevice> = state
                        .devices
                        .iter()
                        .filter(|d| {
                            model::listable(d)
                                && (*show_hidden || !model::is_hidden(d))
                                && model::matches_query(d, search)
                        })
                        .collect();

                    if devices.is_empty() {
                        empty(ui, !search.trim().is_empty());
                    } else {
                        match variant {
                            Variant::Grid => grid(
                                ui,
                                &devices,
                                &state.devices,
                                cmd,
                                rename,
                                confirm_remove,
                                conflict_choice,
                                page,
                            ),
                            Variant::List => list(
                                ui,
                                &devices,
                                &state.devices,
                                cmd,
                                rename,
                                confirm_remove,
                                conflict_choice,
                                page,
                            ),
                        }
                    }
                });
        });

    remove_confirm_modal(ui.ctx(), cmd, confirm_remove);
    conflict_choice_modal(ui.ctx(), cmd, conflict_choice);
}

/// A pending "remove chained device" confirmation: the child to unlink plus the
/// parent device and channel that host it.
pub struct ConfirmRemove {
    pub child_id: String,
    pub child_name: String,
    pub parent_id: String,
    pub channel_id: String,
}

/// A pending choice of the one device that should remain enabled in a
/// conflict group. Names are captured from the snapshot so the dialog remains
/// stable even while the live list refreshes.
pub struct ConflictChoice {
    pub devices: Vec<ConflictChoiceDevice>,
    pub recommended_id: String,
    pub confidence: halod_shared::types::ConflictConfidence,
}

pub struct ConflictChoiceDevice {
    pub id: String,
    pub name: String,
    pub device_type: DeviceType,
    pub source: ConflictDeviceSource,
}

fn conflict_choice_for(d: &WireDevice, all_devices: &[WireDevice]) -> Option<ConflictChoice> {
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
    (devices.len() > 1).then(|| ConflictChoice {
        devices,
        recommended_id: conflict.recommended_id.clone(),
        confidence: conflict.confidence.clone(),
    })
}

fn conflict_key(d: &WireDevice) -> Option<String> {
    let conflict = d.conflict.as_ref()?;
    let mut ids = conflict.peer_ids.clone();
    ids.push(d.id.clone());
    ids.sort();
    Some(format!("{:?}:{}", conflict.confidence, ids.join("\u{1f}")))
}

/// Prompt once for each newly observed group. A dismissed dialog stays
/// dismissed for this GUI session; its card badge can still reopen it.
fn open_new_conflict_prompt(
    state: &AppState,
    conflict_choice: &mut Option<ConflictChoice>,
    conflict_prompted: &mut HashSet<String>,
) {
    if conflict_choice.is_some() {
        return;
    }
    for d in &state.devices {
        let Some(key) = conflict_key(d) else {
            continue;
        };
        if conflict_prompted.insert(key) {
            *conflict_choice = conflict_choice_for(d, &state.devices);
            break;
        }
    }
}

fn ids_to_disable(choice: &ConflictChoice, kept_id: &str) -> Vec<String> {
    choice
        .devices
        .iter()
        .filter(|device| device.id != kept_id)
        .map(|device| device.id.clone())
        .collect()
}

fn conflict_choice_modal(
    ctx: &egui::Context,
    cmd: &CommandTx,
    conflict_choice: &mut Option<ConflictChoice>,
) {
    let Some(choice) = conflict_choice.as_ref() else {
        return;
    };
    let mut keep = None;
    let mut cancel = false;
    let body = if choice.confidence == halod_shared::types::ConflictConfidence::Confirmed {
        t!("home.conflict_dialog_confirmed")
    } else {
        t!("home.conflict_dialog_possible")
    };
    let dismissed = widgets::dialog(
        ctx,
        "home_conflict_choice",
        &t!("home.conflict_dialog_title"),
        440.0,
        |ui| {
            ui.label(
                egui::RichText::new(body)
                    .font(theme::body(12.5))
                    .color(theme::TEXT_MUT),
            );
            ui.add_space(12.0);
            let option_side = ((ui.available_width() - 10.0) / 2.0).clamp(130.0, 180.0);
            for row in choice.devices.chunks(2) {
                ui.horizontal(|ui| {
                    let gap = 10.0;
                    let row_width =
                        option_side * row.len() as f32 + gap * row.len().saturating_sub(1) as f32;
                    // Center the whole pair (or a final unpaired tile), not
                    // merely the contents of each individual tile.
                    ui.add_space(((ui.available_width() - row_width) / 2.0).max(0.0));
                    ui.spacing_mut().item_spacing.x = 10.0;
                    for device in row {
                        if conflict_device_option(
                            ui,
                            device,
                            device.id == choice.recommended_id,
                            option_side,
                        ) {
                            keep = Some(device.id.clone());
                        }
                    }
                });
                ui.add_space(10.0);
            }
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("home.cancel"),
                widgets::ButtonKind::Ghost,
                egui::vec2(96.0, 32.0),
            )
            .clicked()
            {
                cancel = true;
            }
        },
    );
    if let Some(kept_id) = keep {
        let choice = conflict_choice.take().expect("dialog choice exists");
        for id in ids_to_disable(&choice, &kept_id) {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::SetDeviceVisibility {
                    device_id: id,
                    state: VisibilityState::Disabled,
                },
            );
        }
    } else if cancel || dismissed {
        *conflict_choice = None;
    }
}

fn conflict_device_option(
    ui: &mut egui::Ui,
    device: &ConflictChoiceDevice,
    recommended: bool,
    side: f32,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(side, side), Sense::click());
    let accent = if recommended {
        theme::CYAN
    } else {
        theme::BORDER
    };
    let fill = if response.hovered() {
        a(theme::CYAN, 0.10)
    } else {
        theme::INNER_BG
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 12.0, fill);
    painter.rect_stroke(
        rect,
        12.0,
        Stroke::new(if recommended { 1.5 } else { 1.0 }, accent),
        egui::StrokeKind::Middle,
    );
    let badge = Rect::from_center_size(
        Pos2::new(rect.center().x, rect.top() + side * 0.34),
        Vec2::splat(side * 0.29),
    );
    widgets::device_badge(painter, badge, device.device_type);
    let name_font = theme::semibold(12.0);
    let source_font = theme::body(10.0);
    let max_text_width = side - 18.0;
    let name = ellipsize(painter, &device.name, &name_font, max_text_width);
    let full_source = conflict_source_label(&device.source);
    let source = ellipsize(painter, &full_source, &source_font, max_text_width);
    painter.text(
        Pos2::new(rect.center().x, rect.top() + side * 0.59),
        Align2::CENTER_TOP,
        name,
        name_font,
        theme::TEXT,
    );
    painter.text(
        Pos2::new(rect.center().x, rect.top() + side * 0.76),
        Align2::CENTER_TOP,
        source,
        source_font,
        theme::TEXT_MUT,
    );
    if recommended {
        painter.text(
            Pos2::new(rect.center().x, rect.bottom() - 10.0),
            Align2::CENTER_BOTTOM,
            t!("home.conflict_recommended"),
            theme::body(9.0),
            theme::CYAN,
        );
    }
    response
        .on_hover_text(format!(
            "{}\n{}\n{}",
            device.name,
            full_source,
            t!("home.conflict_choose_owner")
        ))
        .clicked()
}

fn ellipsize(painter: &egui::Painter, text: &str, font: &egui::FontId, max_width: f32) -> String {
    if painter
        .layout_no_wrap(text.to_owned(), font.clone(), theme::TEXT)
        .rect
        .width()
        <= max_width
    {
        return text.to_owned();
    }
    let mut clipped = text.chars().collect::<Vec<_>>();
    while !clipped.is_empty() {
        let candidate = format!("{}…", clipped.iter().collect::<String>());
        if painter
            .layout_no_wrap(candidate.clone(), font.clone(), theme::TEXT)
            .rect
            .width()
            <= max_width
        {
            return candidate;
        }
        clipped.pop();
    }
    "…".into()
}

fn conflict_source_label(source: &ConflictDeviceSource) -> String {
    match source {
        ConflictDeviceSource::Native => t!("home.conflict_source_native").into_owned(),
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
            DeviceCapability::Rgb(r) => r.chainable_channels.iter().find_map(|ch| {
                ch.links
                    .iter()
                    .any(|l| l.child_device_id == child_id && !l.locked)
                    .then(|| (p.id.clone(), ch.channel_id.clone()))
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
    let (mut confirm, mut cancel) = (false, false);
    let dismissed = widgets::dialog(
        ctx,
        "home_remove_chain",
        &t!("home.remove_device_title"),
        380.0,
        |ui| {
            ui.label(
                egui::RichText::new(t!("home.remove_device_confirm", name = target.child_name))
                    .font(theme::body(12.5))
                    .color(theme::TEXT_MUT),
            );
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("home.remove"),
                widgets::ButtonKind::Danger,
                egui::vec2(96.0, 32.0),
            )
            .clicked()
            {
                confirm = true;
            }
            ui.add_space(8.0);
            if widgets::button(
                ui,
                &t!("home.cancel"),
                widgets::ButtonKind::Ghost,
                egui::vec2(96.0, 32.0),
            )
            .clicked()
            {
                cancel = true;
            }
        },
    );
    if let Some(target) =
        widgets::resolve_delete_confirm(confirm_remove, confirm, cancel || dismissed)
    {
        crate::runtime::ipc::send(
            cmd,
            halod_shared::commands::DaemonCommand::RgbChainRemoveLink {
                id: target.parent_id,
                channel_id: target.channel_id,
                child_device_id: target.child_id,
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
        ui.add_space(7.0);
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

fn header(
    ui: &mut egui::Ui,
    state: &AppState,
    show_hidden: &mut bool,
    variant: &mut Variant,
    search: &mut String,
) {
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
        .count()
        + crate::domain::models::sensors::hidden_count(state);

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
                    .font(theme::body(13.0))
                    .color(theme::TEXT_MUT),
            );
        });

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            segmented(ui, variant);
            ui.add_space(10.0);
            if hidden > 0 {
                let label = if *show_hidden {
                    t!("home.hide_hidden")
                } else {
                    t!("home.show_hidden")
                };
                let clicked = widgets::pill(ui, &label, *show_hidden);
                crate::domain::tour::anchor(
                    ui.ctx(),
                    crate::domain::tour::AnchorId::HomeShowHidden,
                    ui.min_rect(),
                );
                if clicked {
                    *show_hidden = !*show_hidden;
                }
                ui.add_space(10.0);
            }
            search_box(ui, search);
        });
    });
}

/// A rounded search field that filters the device list by name or vendor.
fn search_box(ui: &mut egui::Ui, search: &mut String) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(190.0, 33.0), Sense::hover());
    crate::domain::tour::anchor(ui.ctx(), crate::domain::tour::AnchorId::HomeSearch, rect);
    ui.painter().rect_filled(rect, 10.0, theme::CARD_BG);
    ui.painter().rect_stroke(
        rect,
        10.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Middle,
    );
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
            .font(theme::body(12.5))
            .hint_text(t!("home.search_devices")),
    );
}

/// Grid/List segmented control, drawn on a single allocated rect so it never
/// wraps regardless of the surrounding layout.
fn segmented(ui: &mut egui::Ui, variant: &mut Variant) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(108.0, 33.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 10.0, theme::CARD_BG);
    p.rect_stroke(
        rect,
        10.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Middle,
    );
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
            ui.painter().rect_filled(chip, 7.0, theme::CYAN);
        } else {
            let t =
                ui.ctx()
                    .animate_bool_with_time(ui.id().with(("seg_h", i)), resp.hovered(), 0.12);
            if t > 0.001 {
                ui.painter()
                    .rect_filled(chip, 7.0, a(Color32::WHITE, 0.05 * t));
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
                theme::semibold(12.0)
            } else {
                theme::body(12.0)
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

/// All visible sensors (plus hidden ones when `show_hidden`), each
/// with a live sparkline drawn in the card background. Wraps into rows of four.
fn sensors_row(
    ui: &mut egui::Ui,
    state: &AppState,
    cmd: &CommandTx,
    show_hidden: bool,
    history: &HashMap<String, VecDeque<f32>>,
) {
    sensors_grid(ui, state, cmd, show_hidden, history, 4);
}

// Sensor cards with sparklines, shared by Home and Cooling pages.
pub(crate) fn sensors_grid(
    ui: &mut egui::Ui,
    state: &AppState,
    cmd: &CommandTx,
    show_hidden: bool,
    history: &HashMap<String, VecDeque<f32>>,
    cols: usize,
) {
    let sensors = crate::domain::models::sensors::sensors(state, show_hidden);
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
                sensor_card(ui, s, color, w, cmd, history);
            }
        });
    }
}

fn sensor_card(
    ui: &mut egui::Ui,
    s: &crate::domain::models::sensors::SensorView,
    color: Color32,
    w: f32,
    cmd: &CommandTx,
    history: &HashMap<String, VecDeque<f32>>,
) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 82.0), Sense::click());
    let hovered = resp.hovered() && !s.hidden;
    let t = ui
        .ctx()
        .animate_bool_with_time(egui::Id::new(("sensor_h", &s.id)), hovered, 0.15);
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    p.rect_filled(rect, 12.0, theme::CARD_BG);

    // Background sparkline from the rolling history.
    if let Some(samples) = history.get(&s.id) {
        let samples: Vec<f32> = samples.iter().copied().collect();
        sparkline(&p.with_clip_rect(rect), rect, &samples, color);
    }
    // The sparkline fill bleeds into the rounded corners; mask them back.
    theme::round_corners(p, rect, 12.0, theme::MAIN_BG);
    let border_color = if s.hidden {
        theme::BORDER
    } else {
        theme::lerp_color(theme::BORDER, a(color, 0.5), t)
    };
    p.rect_stroke(
        rect,
        12.0,
        Stroke::new(1.0, border_color),
        egui::StrokeKind::Middle,
    );

    // Label row: colored dot + name.
    let dot = Pos2::new(rect.left() + 18.0 + 2.5, rect.top() + 18.0);
    p.circle_filled(dot, 2.5, color);
    p.text(
        Pos2::new(rect.left() + 28.0, rect.top() + 18.0),
        Align2::LEFT_CENTER,
        &s.label,
        theme::body(11.0),
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
        theme::body(12.0),
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
    // Greyed when hidden; right-click to toggle visibility.
    if s.hidden {
        p.rect_filled(rect, 12.0, a(theme::MAIN_BG, 0.45));
        theme::round_corners(p, rect, 12.0, theme::MAIN_BG);
        p.rect_stroke(
            rect,
            12.0,
            Stroke::new(1.0, theme::BORDER),
            egui::StrokeKind::Middle,
        );
    }
    resp.context_menu(|ui| {
        ui.set_width(150.0);
        let (label, state) = if s.hidden {
            (t!("home.show_sensor"), VisibilityState::Visible)
        } else {
            (t!("home.hide_sensor"), VisibilityState::Hidden)
        };
        if widgets::context_menu_item(ui, &label, theme::TEXT).clicked() {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::SetSensorVisibility {
                    sensor_id: s.id.clone(),
                    state,
                },
            );
            ui.close();
        }
    });
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
    conflict_choice: &mut Option<ConflictChoice>,
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
                    device_card(ui, rect, d, all_devices, conflict_choice, resp.hovered());
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

    p.rect_filled(rect, 14.0, theme::CARD_BG);
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
    conflict_choice: &mut Option<ConflictChoice>,
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
        theme::body(11.0),
        theme::TEXT_MUT,
    );
    p.text(
        Pos2::new(rect.left() + 16.0, type_y - 6.0 - 16.0),
        Align2::LEFT_BOTTOM,
        &d.name,
        theme::semibold(13.5),
        theme::TEXT,
    );

    // Grey out hidden/disabled/offline devices with a uniform scrim.
    if dimmed {
        p.rect_filled(rect, 14.0, a(theme::MAIN_BG, 0.5));
    }

    // Restore the rounded silhouette (the clipped glow filled the corners),
    // then stroke the border — accent on hover, animated.
    theme::round_corners(p, rect, 14.0, theme::MAIN_BG);
    let border = theme::lerp_color(theme::BORDER, a(color, 0.55), t);
    p.rect_stroke(
        rect,
        14.0,
        Stroke::new(1.0, border),
        egui::StrokeKind::Middle,
    );
    conflict_control(ui, rect, d, all_devices, conflict_choice)
}

fn conflict_control(
    ui: &mut egui::Ui,
    rect: Rect,
    d: &WireDevice,
    all_devices: &[WireDevice],
    conflict_choice: &mut Option<ConflictChoice>,
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
        theme::body(9.0),
        color,
    );
    let clicked = response.clicked();
    response.on_hover_text(if conflict.is_recommended {
        t!("home.conflict_owner", name = conflict.recommended_name)
    } else {
        t!("home.conflict_with", name = conflict.peer_names.join(", "))
    });
    if clicked {
        *conflict_choice = conflict_choice_for(d, all_devices);
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
        theme::body(10.5),
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
    conflict_choice: &mut Option<ConflictChoice>,
    page: &mut crate::domain::state::Page,
) {
    egui::Frame::NONE
        .fill(theme::CARD_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(14.0)
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
                    conflict_choice,
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
    conflict_choice: &mut Option<ConflictChoice>,
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
        theme::semibold(13.0),
        theme::TEXT,
    );
    p.text(
        Pos2::new(chip.right() + 14.0, rect.center().y + 9.0),
        Align2::LEFT_CENTER,
        model::type_label(d),
        theme::body(11.0),
        theme::TEXT_MUT,
    );

    // Metrics, mid.
    let mut mx = chip.right() + 220.0;
    for m in model::metrics(d) {
        p.text(
            Pos2::new(mx, rect.center().y - 8.0),
            Align2::LEFT_CENTER,
            &m.label,
            theme::body(10.0),
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
        conflict_choice,
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

    fn hub_with_links(links: Vec<halod_shared::types::ChainLinkInfo>) -> WireDevice {
        use halod_shared::types::*;
        WireDevice {
            id: "hub1".into(),
            name: "Hub".into(),
            vendor: "v".into(),
            model: "m".into(),
            device_type: DeviceType::Other,
            connected: true,
            capabilities: vec![DeviceCapability::Rgb(RgbStatus {
                descriptor: RgbDescriptor {
                    zones: vec![],
                    native_effects: vec![],
                },
                state: None,
                zone_transforms: Default::default(),
                chainable_channels: vec![ChainableChannelInfo {
                    channel_id: "h1".into(),
                    name: "Header 1".into(),
                    max_leds: 120,
                    links,
                }],
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

    fn link(child: &str, locked: bool) -> halod_shared::types::ChainLinkInfo {
        use halod_shared::types::*;
        ChainLinkInfo {
            child_device_id: child.into(),
            name: "Strip".into(),
            topology: ZoneTopology::Linear,
            led_count: 30,
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

    #[test]
    fn keeping_one_conflict_owner_disables_every_other_participant() {
        let choice = ConflictChoice {
            devices: vec![
                ConflictChoiceDevice {
                    id: "native".into(),
                    name: "Native driver".into(),
                    device_type: DeviceType::Mouse,
                    source: ConflictDeviceSource::Native,
                },
                ConflictChoiceDevice {
                    id: "openrgb".into(),
                    name: "OpenRGB".into(),
                    device_type: DeviceType::Mouse,
                    source: ConflictDeviceSource::Integration("openrgb".into()),
                },
            ],
            recommended_id: "native".into(),
            confidence: halod_shared::types::ConflictConfidence::Confirmed,
        };
        assert_eq!(ids_to_disable(&choice, "native"), vec!["openrgb"]);
        assert_eq!(ids_to_disable(&choice, "openrgb"), vec!["native"]);
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
