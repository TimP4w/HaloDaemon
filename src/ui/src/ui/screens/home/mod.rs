// SPDX-License-Identifier: GPL-3.0-or-later
//! The Home screen main panel: greeting + summary, the configurable widget row,
//! and the device grid/list. All data comes from the live daemon state.

mod widget_config;
pub mod widget_row;
mod widget_view;

use crate::ui::components as widgets;
use halod_shared::commands::DeviceOp;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::domain::topic_store::TopicStore;
use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{ConflictDeviceSource, DeviceCapability, VisibilityState, WireDevice};

use crate::domain::models::device::{self as model};
use crate::domain::state::Page;
use crate::domain::state::Rename;
use crate::domain::tour;
use crate::domain::tour::AnchorId;
use crate::runtime::ipc::CommandTx;
use crate::ui::screens::startup;
use crate::ui::theme::{self, a};
use halod_shared::ids::DeviceId;

const GAP: f32 = 12.0;
const SECTION_GAP: f32 = 20.0;
const CONFLICT_STRIP_H: f32 = 30.0;
const CONFLICT_STRIP_GAP: f32 = 12.0;

/// Everything the Home screen owns across frames.
#[derive(Default)]
pub struct HomeUi {
    pub show_hidden: bool,
    /// Device-list filter text (matches name or vendor).
    pub search: String,
    pub rename: Option<Rename>,
    /// Pending confirmation to unlink a chained device.
    pub confirm_remove: Option<ConfirmRemove>,
    /// Open duplicate-device resolve dialog: every conflict group with the
    /// owner the user has picked to keep. `None` when the dialog is closed.
    pub conflict_resolve: Option<Vec<ConflictGroup>>,
    pub pending_disable: Option<(DeviceId, String)>,
    pub widgets: widget_row::EditState,
}

impl HomeUi {
    /// Open the resolve dialog on every current conflict group, if any.
    #[cfg(all(test, feature = "screenshots"))]
    pub fn open_conflicts(&mut self, devices: &[WireDevice]) {
        let groups = conflict_groups(devices);
        self.conflict_resolve = (!groups.is_empty()).then_some(groups);
    }
}

pub fn show(
    ui: &mut egui::Ui,
    state: &TopicStore,
    cmd: &CommandTx,
    home: &mut HomeUi,
    history: &HashMap<String, VecDeque<f32>>,
    page: &mut Page,
    time: f64,
    allow_modals: bool,
) {
    let sensors = crate::domain::models::sensors::sensors(state);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            widgets::page_frame(ui, |ui| {
                header(ui, state, home);
                startup::discovery_status(ui, state);
                let conflicts = conflict_group_count(&state.devices);
                if conflicts > 0 {
                    ui.add_space(theme::SPACE_9);
                    if attention_banner(ui, conflicts) {
                        home.conflict_resolve = Some(conflict_groups(&state.devices));
                    }
                }
                let has_row = home.widgets.customizing() || !state.gui.home_widgets.is_empty();
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
                    ui.add_space(SECTION_GAP - theme::SPACE_4);
                }

                let discovering = matches!(
                    state.discovery.phase,
                    halod_shared::types::DiscoveryPhase::Discovering
                );
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
                    grid(
                        ui,
                        &devices,
                        &state.devices,
                        cmd,
                        &mut home.rename,
                        &mut home.confirm_remove,
                        &mut home.pending_disable,
                        &mut home.conflict_resolve,
                        page,
                        discovering,
                    );
                }
            });
        });

    if allow_modals {
        remove_confirm_modal(ui.ctx(), cmd, &mut home.confirm_remove);
        if let Some((id, _)) =
            crate::ui::screens::device::info::disable_confirm(ui.ctx(), &mut home.pending_disable)
        {
            crate::runtime::ipc::send(cmd, crate::ui::screens::device::info::disable_command(id));
        }
        conflict_modal(ui.ctx(), cmd, &mut home.conflict_resolve);
    }
}

/// A pending "remove chained device" confirmation: the child to unlink plus the
/// parent device and channel that host it.
pub struct ConfirmRemove {
    pub child_id: DeviceId,
    pub child_name: String,
    pub parent_id: DeviceId,
    pub channel_id: String,
}

/// One duplicate-device conflict group and the owner the user has picked to
/// keep. Captured from the snapshot when the resolve modal opens so the dialog
/// stays stable while the live list refreshes.
pub struct ConflictGroup {
    pub devices: Vec<ConflictChoiceDevice>,
    pub recommended_id: DeviceId,
    /// The device id currently selected to keep; defaults to `recommended_id`.
    pub pick: DeviceId,
}

pub struct ConflictChoiceDevice {
    pub id: DeviceId,
    pub name: String,
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
    let ids: Vec<&str> = ids.iter().map(|id| id.as_str()).collect();
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

fn ids_to_disable(group: &ConflictGroup, kept_id: &DeviceId) -> Vec<DeviceId> {
    group
        .devices
        .iter()
        .filter(|device| device.id != *kept_id)
        .map(|device| device.id.clone())
        .collect()
}

/// Every device id to disable when the whole resolution is applied: for each
/// group, all participants except the one the user kept.
fn disables_for(groups: &[ConflictGroup]) -> Vec<DeviceId> {
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
            theme::STAT_AMBER,
        );
        p.text(
            Pos2::new(tx, rect.center().y + 9.0),
            Align2::LEFT_CENTER,
            t!("home.conflict_banner_sub"),
            theme::body_sm(),
            theme::a(theme::STAT_AMBER, 0.75),
        );
    }
    let btn = Rect::from_min_size(
        Pos2::new(rect.right() - 16.0 - 96.0, rect.center().y - 17.0),
        Vec2::new(96.0, 34.0),
    );
    widgets::button_at(
        ui,
        btn,
        egui::Id::unique("conflict_banner_resolve"),
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

/// The resolve dialog: every conflict group at once, each with radio owner
/// options. Applying keeps the picked owner per group and disables the rest in
/// a single pass.
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
    let dismissed = widgets::dialog_with_subtitle(
        ctx,
        "home_conflict_resolve",
        &t!("home.conflict_modal_title"),
        &t!("home.conflict_modal_intro"),
        560.0,
        |ui| {
            ui.add_space(theme::SPACE_5);
            for (i, group) in groups.iter_mut().enumerate() {
                if i > 0 {
                    ui.add_space(theme::SPACE_7);
                }
                conflict_group_card(ui, group);
            }
        },
        |ui| {
            if widgets::button(
                ui,
                &if count == 1 {
                    t!("home.conflict_keep_selected_one")
                } else {
                    t!("home.conflict_keep_selected", count = count)
                },
                widgets::ButtonKind::Primary,
                egui::vec2(0.0, 36.0),
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
                egui::vec2(0.0, 36.0),
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
                halod_shared::commands::DaemonCommand::Device {
                    device_id: id,
                    op: DeviceOp::SetDeviceVisibility {
                        state: VisibilityState::Disabled,
                    },
                },
            );
        }
    } else if cancel || dismissed {
        *conflict_resolve = None;
    }
}

/// One conflict group: the contested device's name over its radio owner
/// options. Updates `group.pick`.
fn conflict_group_card(ui: &mut egui::Ui, group: &mut ConflictGroup) {
    let head = group
        .devices
        .iter()
        .find(|d| d.id == group.recommended_id)
        .unwrap_or(&group.devices[0])
        .name
        .clone();
    egui::Frame::NONE
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::RADIUS_MD)
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = theme::SPACE_4;
            ui.label(
                egui::RichText::new(head)
                    .font(theme::semibold(14.0))
                    .color(theme::TEXT),
            );
            let mut picked = None;
            for dev in &group.devices {
                if owner_option(
                    ui,
                    dev,
                    dev.id == group.pick,
                    dev.id == group.recommended_id,
                ) {
                    picked = Some(dev.id.clone());
                }
            }
            if let Some(id) = picked {
                group.pick = id;
            }
        });
}

/// A single radio owner option. Returns `true` when clicked.
fn owner_option(
    ui: &mut egui::Ui,
    device: &ConflictChoiceDevice,
    selected: bool,
    recommended: bool,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 56.0), Sense::click());
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    let (fill, border) = if selected {
        (theme::CHECKED_BG, theme::SELECTED_BORDER)
    } else if response.hovered() {
        (theme::INNER_BG, theme::BORDER_INNER)
    } else {
        (theme::INNER_BG, theme::BORDER)
    };
    theme::paint_surface(p, rect, theme::RADIUS_SM + 1.0, fill, border);
    let radio = Pos2::new(rect.left() + 24.0, rect.center().y);
    if selected {
        p.circle_filled(radio, 6.5, theme::CYAN);
        p.circle_filled(radio, 2.8, theme::TEXT_BRIGHT);
    } else {
        p.circle_filled(radio, 6.5, theme::TEXT);
    }
    let tx = rect.left() + 46.0;
    let note = if recommended {
        p.text(
            Pos2::new(rect.right() - 12.0, rect.center().y),
            Align2::RIGHT_CENTER,
            t!("home.conflict_tag_recommended"),
            theme::body_sm(),
            theme::ACCENT_TEXT,
        )
        .left()
    } else {
        rect.right()
    };
    let max_w = (note - 12.0 - tx).max(20.0);
    let name_font = theme::medium(14.0);
    p.text(
        Pos2::new(tx, rect.center().y - 8.0),
        Align2::LEFT_CENTER,
        ellipsize(p, &conflict_source_label(&device.source), &name_font, max_w),
        name_font,
        theme::TEXT,
    );
    p.text(
        Pos2::new(tx, rect.center().y + 9.0),
        Align2::LEFT_CENTER,
        ellipsize(p, &device.name, &theme::body_sm(), max_w),
        theme::body_sm(),
        theme::TEXT_MUT,
    );
    response.clicked()
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
fn chain_parent(devices: &[WireDevice], child_id: &DeviceId) -> Option<(DeviceId, String)> {
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
            halod_shared::commands::DaemonCommand::Device {
                device_id: target.parent_id,
                op: DeviceOp::LightingRemoveSegment {
                    channel_id: target.channel_id,
                    segment_id: target.child_id.clone(),
                },
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
    pending_disable: &mut Option<(DeviceId, String)>,
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
            cancel = widgets::button(ui, &t!("home.cancel"), widgets::ButtonKind::Secondary, bw)
                .clicked();
        });
        let trimmed = r.buf.trim().to_string();
        if save && !trimmed.is_empty() {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::Device {
                    device_id: d.id.clone(),
                    op: DeviceOp::SetDeviceName { name: trimmed },
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
                *pending_disable = Some((d.id.clone(), d.name.clone()));
                ui.close();
            }
        }
        VisibilityState::Hidden => {
            if widgets::context_menu_item(ui, &t!("home.show_device"), theme::TEXT).clicked() {
                set_vis(ui, cmd, &d.id, VisibilityState::Visible);
            }
            if widgets::context_menu_item(ui, &t!("home.disable_device"), theme::OFFLINE_TEXT)
                .clicked()
            {
                *pending_disable = Some((d.id.clone(), d.name.clone()));
                ui.close();
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

fn set_vis(ui: &mut egui::Ui, cmd: &CommandTx, id: &DeviceId, state: VisibilityState) {
    crate::runtime::ipc::send(
        cmd,
        halod_shared::commands::DaemonCommand::Device {
            device_id: id.clone(),
            op: DeviceOp::SetDeviceVisibility { state },
        },
    );
    ui.close();
}

fn device_counts(devices: &[WireDevice]) -> (usize, usize) {
    let listed = devices.iter().filter(|d| model::listable(d));
    let total = listed.clone().count();
    let attention = listed
        .filter(|d| !model::is_hidden(d) && !d.connected)
        .count();
    (total, attention)
}

fn header(ui: &mut egui::Ui, state: &TopicStore, home: &mut HomeUi) {
    let (total, attention) = device_counts(&state.devices);

    let sub = if matches!(
        state.discovery.phase,
        halod_shared::types::DiscoveryPhase::Discovering
    ) {
        t!("home.devices_found_so_far", total = total)
    } else if attention > 0 {
        t!(
            "home.devices_count_attention",
            total = total,
            attention = attention
        )
    } else {
        t!("home.devices_count", total = total)
    };
    widgets::page_title(ui, &t!("home.title"), &sub, |ui| {
        let label = if home.widgets.customizing() {
            t!("home.customize_done")
        } else {
            t!("home.customize")
        };
        if widgets::button(
            ui,
            &label,
            widgets::ButtonKind::Secondary,
            Vec2::new(0.0, 36.0),
        )
        .clicked()
        {
            home.widgets.toggle(state);
        }
        let resp = widgets::search_field(ui, &mut home.search, &t!("home.search_devices"), 280.0);
        tour::anchor(ui.ctx(), AnchorId::HomeSearch, resp.rect);
    });
}

/// The change across the history window in words, e.g. `"Up 4 °C"`.
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
    pending_disable: &mut Option<(halod_shared::ids::DeviceId, String)>,
    conflict_resolve: &mut Option<Vec<ConflictGroup>>,
    page: &mut Page,
    discovering: bool,
) {
    let avail = ui.available_width();
    let cols = (((avail + GAP) / (300.0 + GAP)).floor() as usize).clamp(1, 3);
    let w = (avail - GAP * (cols - 1) as f32) / cols as f32;
    let row_h = |row: &[&WireDevice], partial: bool| {
        let base = if partial || row.iter().any(|d| !model::metrics(d).is_empty()) {
            138.0
        } else {
            68.0
        };
        if row.iter().any(|d| d.conflict.is_some()) {
            base + CONFLICT_STRIP_GAP + CONFLICT_STRIP_H
        } else {
            base
        }
    };

    let mut anchored_a_card = false;
    ui.spacing_mut().item_spacing.y = 0.0;
    let placeholders = if discovering {
        cols - devices.len() % cols
    } else {
        0
    };
    let rows = (devices.len() + placeholders).div_ceil(cols);
    for r in 0..rows {
        let row = &devices[(r * cols).min(devices.len())..((r + 1) * cols).min(devices.len())];
        let h = row_h(row, row.len() < cols);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for d in row {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
                if !anchored_a_card {
                    tour::anchor(ui.ctx(), AnchorId::HomeDeviceCard, rect);
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
                    *page = Page::Device(d.id.clone());
                }
                resp.context_menu(|ui| {
                    card_menu(
                        ui,
                        d,
                        all_devices,
                        cmd,
                        rename,
                        confirm_remove,
                        pending_disable,
                    )
                });
            }
            for _ in row.len()..cols.min(devices.len() + placeholders - r * cols) {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
                skeleton_card(ui.painter(), rect);
            }
        });
        ui.add_space(GAP);
    }
}

/// A device card still loading during discovery.
fn skeleton_card(p: &egui::Painter, rect: Rect) {
    theme::paint_surface(p, rect, theme::RADIUS_MD, theme::CARD_BG, theme::BORDER);
    let inner = rect.shrink(16.0);
    p.rect_filled(
        Rect::from_min_size(inner.min, Vec2::splat(36.0)),
        theme::RADIUS_SM + 1.0,
        theme::TILE_BG,
    );
    let x = inner.left() + 48.0;
    let text_w = inner.right() - x;
    p.rect_filled(
        Rect::from_min_size(
            Pos2::new(x, inner.top() + 6.0),
            Vec2::new(text_w * 0.6, 12.0),
        ),
        3.0,
        theme::ROW_ACTIVE,
    );
    p.rect_filled(
        Rect::from_min_size(
            Pos2::new(x, inner.top() + 24.0),
            Vec2::new(text_w * 0.35, 10.0),
        ),
        3.0,
        theme::RAISED_BG,
    );
    p.rect_filled(
        Rect::from_min_max(
            Pos2::new(inner.left(), inner.bottom() - widgets::STAT_STRIP_H),
            inner.max,
        ),
        theme::RADIUS_SM,
        theme::INNER_BG,
    );
}

fn device_status_tag(d: &WireDevice) -> (String, widgets::TagTone) {
    if d.active_state == VisibilityState::Disabled {
        (t!("home.disabled").into_owned(), widgets::TagTone::Neutral)
    } else if d.connected {
        (model::transport_label(d), widgets::TagTone::Neutral)
    } else {
        (t!("home.offline").into_owned(), widgets::TagTone::Danger)
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
    let t = ui
        .ctx()
        .animate_bool_with_time(egui::Id::unique(("card", &d.id)), hovered, 0.12);
    let p = ui.painter();
    p.rect_filled(rect, theme::RADIUS_MD, theme::CARD_BG);
    p.rect_stroke(
        rect,
        theme::RADIUS_MD,
        Stroke::new(
            1.0,
            theme::lerp_color(theme::BORDER, theme::BORDER_INNER, t),
        ),
        egui::StrokeKind::Inside,
    );

    let inner = rect.shrink(16.0);
    let tile = Rect::from_min_size(inner.min, Vec2::splat(36.0));
    p.rect_filled(tile, theme::RADIUS_SM + 1.0, theme::TILE_BG);
    widgets::device_badge(p, tile, d.device_type);

    let (status, tone) = device_status_tag(d);
    let tag_rect = widgets::paint_tag(p, Pos2::new(inner.right(), tile.center().y), &status, tone);

    let text_left = tile.right() + 12.0;
    let text_w = (tag_rect.left() - 8.0 - text_left).max(20.0);
    let name_font = theme::medium(14.0);
    p.text(
        Pos2::new(text_left, tile.top() + 1.0),
        Align2::LEFT_TOP,
        ellipsize(p, &d.name, &name_font, text_w),
        name_font,
        theme::TEXT,
    );
    p.text(
        Pos2::new(text_left, tile.bottom() - 1.0),
        Align2::LEFT_BOTTOM,
        model::type_label(d),
        theme::body_sm(),
        theme::TEXT_DIM,
    );

    let metrics = model::metrics(d);
    if !metrics.is_empty() {
        let strip = Rect::from_min_size(
            Pos2::new(inner.left(), tile.bottom() + 14.0),
            Vec2::new(inner.width(), widgets::STAT_STRIP_H),
        );
        let items: Vec<(&str, &str)> = metrics
            .iter()
            .map(|m| (m.label.as_str(), m.value.as_str()))
            .collect();
        widgets::paint_stat_strip(p, strip, &items);
    }

    if !d.connected || d.active_state != VisibilityState::Visible {
        p.rect_filled(rect, theme::RADIUS_MD, a(theme::MAIN_BG, 0.5));
    }
    conflict_control(ui, inner, d, all_devices, conflict_resolve)
}

fn conflict_control(
    ui: &mut egui::Ui,
    inner: Rect,
    d: &WireDevice,
    all_devices: &[WireDevice],
    conflict_resolve: &mut Option<Vec<ConflictGroup>>,
) -> bool {
    use halod_shared::types::ConflictConfidence;
    let Some(conflict) = model::conflict_presentation(d, all_devices) else {
        return false;
    };
    let peers = conflict.peer_names.join(", ");
    let (label, action, fill, border, ink, action_ink) = if conflict.is_recommended {
        (
            t!("home.conflict_recommended"),
            t!("home.conflict_review"),
            theme::OK_FILL,
            theme::OK_BORDER,
            theme::ONLINE_TEXT,
            theme::ONLINE_TEXT,
        )
    } else if conflict.confidence == ConflictConfidence::Confirmed {
        (
            t!("home.conflict_duplicate_of", name = peers),
            t!("home.conflict_resolve"),
            a(theme::STAT_AMBER, 0.09),
            a(theme::STAT_AMBER, 0.34),
            theme::WARN_TEXT,
            theme::STAT_AMBER,
        )
    } else {
        (
            t!("home.conflict_possibly", name = peers),
            t!("home.conflict_check"),
            theme::INNER_BG,
            theme::BORDER_STRONG,
            theme::TEXT_SOFT,
            theme::LINK,
        )
    };

    let strip = Rect::from_min_max(
        Pos2::new(inner.left(), inner.bottom() - CONFLICT_STRIP_H),
        inner.max,
    );
    let response = ui.interact(strip, egui::Id::unique(("conflict", &d.id)), Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id, response.hovered(), 0.12);
    let p = ui.painter();
    p.rect_filled(strip, theme::RADIUS_SM, fill);
    p.rect_stroke(
        strip,
        theme::RADIUS_SM,
        Stroke::new(1.0, theme::lerp_color(border, action_ink, hover * 0.5)),
        egui::StrokeKind::Inside,
    );

    let icon = Rect::from_center_size(
        Pos2::new(strip.left() + 17.0, strip.center().y),
        Vec2::splat(13.0),
    );
    if conflict.is_recommended {
        crate::ui::icons::draw(ui, icon, crate::ui::icons::Icon::Check, ink);
    } else if conflict.confidence == ConflictConfidence::Confirmed {
        warning_glyph(p, icon.center(), 13.0, ink);
    } else {
        p.circle_stroke(icon.center(), 6.0, Stroke::new(1.3, ink));
        p.text(
            icon.center(),
            Align2::CENTER_CENTER,
            "?",
            theme::micro(),
            ink,
        );
    }

    let action_galley = p.layout_no_wrap(action.into_owned(), theme::medium(12.0), action_ink);
    let action_pos = Pos2::new(
        strip.right() - 10.0 - action_galley.size().x,
        strip.center().y - action_galley.size().y / 2.0,
    );
    let text_left = icon.right() + 8.0;
    let text_w = (action_pos.x - 10.0 - text_left).max(20.0);
    let font = theme::body_sm();
    p.text(
        Pos2::new(text_left, strip.center().y),
        Align2::LEFT_CENTER,
        ellipsize(p, &label, &font, text_w),
        font,
        ink,
    );
    p.galley(action_pos, action_galley, action_ink);

    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let clicked = response.clicked();
    response.on_hover_text(if conflict.is_recommended {
        t!("home.conflict_owner", name = conflict.recommended_name)
    } else {
        t!("home.conflict_with", name = peers)
    });
    if clicked {
        *conflict_resolve = Some(conflict_groups(all_devices));
    }
    clicked
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
            id: "hub1".parse().unwrap(),
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
            device_id: child.parse().unwrap(),
            channel_id: "lighting".into(),
            name: "Strip".into(),
            topology: ZoneTopology::Linear,
            led_count: 30,
            color_order: None,
            locked,
        }
    }

    #[test]
    fn a_disabled_device_is_tagged_disabled_not_offline() {
        let mut d = WireDevice {
            connected: false,
            active_state: VisibilityState::Disabled,
            ..Default::default()
        };
        assert_eq!(device_status_tag(&d).0, t!("home.disabled"));
        d.active_state = VisibilityState::Visible;
        assert_eq!(device_status_tag(&d).0, t!("home.offline"));
        assert_eq!(device_status_tag(&d).1, widgets::TagTone::Danger);
    }

    #[test]
    fn chain_parent_finds_unlocked_link_parent_and_channel() {
        let devices = vec![hub_with_links(vec![link("child1", false)])];
        assert_eq!(
            chain_parent(&devices, &"child1".parse().unwrap()),
            Some(("hub1".parse().unwrap(), "h1".into()))
        );
    }

    #[test]
    fn chain_parent_ignores_locked_and_unknown_children() {
        let devices = vec![hub_with_links(vec![link("child1", true)])];
        // Hardware-detected (locked) links are not removable.
        assert_eq!(chain_parent(&devices, &"child1".parse().unwrap()), None);
        // A device that is nobody's child yields nothing.
        assert_eq!(chain_parent(&devices, &"stranger".parse().unwrap()), None);
    }

    fn group(ids: &[(&str, &str)], recommended: &str) -> ConflictGroup {
        ConflictGroup {
            devices: ids
                .iter()
                .map(|(id, name)| ConflictChoiceDevice {
                    id: (*id).parse().unwrap(),
                    name: (*name).into(),
                    source: ConflictDeviceSource::Builtin,
                })
                .collect(),
            recommended_id: recommended.parse().unwrap(),
            pick: recommended.parse().unwrap(),
        }
    }

    /// A minimal device carrying a conflict with `peers`, for group building.
    fn dev_with_conflict(id: &str, peers: &[&str], recommended: &str) -> WireDevice {
        use halod_shared::types::*;
        let mut d = hub_with_links(vec![]);
        d.id = id.parse().unwrap();
        d.name = id.to_uppercase();
        d.conflict = Some(DeviceConflictSummary {
            peer_ids: peers.iter().map(|p| (*p).parse().unwrap()).collect(),
            recommended_id: recommended.parse().unwrap(),
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
        assert_eq!(
            ids_to_disable(&group, &"builtin".parse().unwrap()),
            vec!["openrgb"]
        );
        assert_eq!(
            ids_to_disable(&group, &"openrgb".parse().unwrap()),
            vec!["builtin"]
        );
    }

    #[test]
    fn hidden_and_disabled_devices_never_need_attention() {
        let mut devices: Vec<WireDevice> = ["a", "b", "c", "d"]
            .iter()
            .map(|id| dev_with_conflict(id, &[], id))
            .collect();
        for d in &mut devices[1..] {
            d.connected = false;
        }
        devices[2].active_state = VisibilityState::Hidden;
        devices[3].active_state = VisibilityState::Disabled;
        assert_eq!(device_counts(&devices), (4, 1));
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
            g.pick = "d".parse().unwrap(); // user overrode the recommendation here
            g
        }];
        // Group 1 keeps its recommended 'a'; group 2 keeps the overridden 'd'.
        assert_eq!(disables_for(&groups), vec!["b", "c"]);
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
}
