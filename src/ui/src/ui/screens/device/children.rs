// SPDX-License-Identifier: GPL-3.0-or-later
//! Devices tab (hub child devices) and Pairing tab (wireless receiver slots).

use crate::ui::components as widgets;
use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::commands::DeviceOp;
use halod_shared::types::{DeviceCapability, PairingSlot, PairingState, PairingStatus, WireDevice};

use super::TabCtx;
use crate::domain::models::device as model;
use crate::domain::state::Page;
use crate::domain::tour;
use crate::domain::tour::AnchorId;
use crate::runtime::ipc;
use crate::ui::icons;
use crate::ui::theme;

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx, page: &mut Page) {
    let children: Vec<WireDevice> = ctx
        .dev
        .capabilities
        .iter()
        .find_map(|c| match c {
            DeviceCapability::Children(ch) => Some(ch.clone()),
            _ => None,
        })
        .unwrap_or_default();

    let card = ui.scope(|ui| {
        titled_list_card(ui, &t!("device.children_connected_devices"), |ui| {
            if children.is_empty() {
                let (row, _) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), Sense::hover());
                ui.painter().text(
                    Pos2::new(row.left() + 18.0, row.center().y),
                    Align2::LEFT_CENTER,
                    t!("device.children_no_child_devices"),
                    theme::body_md(),
                    theme::TEXT_FAINT,
                );
                return;
            }
            for child in &children {
                if child_row(ui, child) {
                    *page = Page::Device(child.id.clone());
                }
            }
        });
    });
    tour::anchor(ui.ctx(), AnchorId::TabChildrenList, card.response.rect);
}

/// The shared card shell of the child-device and pairing-slot lists: `CARD_BG`
/// surface, 48 px title row over a soft divider, then zero-spaced list rows.
fn titled_list_card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    widgets::card_frameless(ui, |ui| {
        let (title_rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 48.0), Sense::hover());
        ui.painter().text(
            Pos2::new(title_rect.left() + 18.0, title_rect.center().y),
            Align2::LEFT_CENTER,
            title,
            theme::heading(),
            theme::TEXT,
        );
        ui.painter().line_segment(
            [title_rect.left_bottom(), title_rect.right_bottom()],
            Stroke::new(1.0, theme::BORDER_SOFT),
        );
        ui.style_mut().spacing.item_spacing = egui::vec2(0.0, 0.0);
        body(ui);
    });
}

fn child_row(ui: &mut egui::Ui, d: &WireDevice) -> bool {
    let (row, resp) = widgets::hover_row(ui, 52.0, false);
    let p = ui.painter();
    p.line_segment(
        [row.left_bottom(), row.right_bottom()],
        Stroke::new(1.0, theme::BORDER_SOFT),
    );

    let cy = row.center().y;
    let badge_rect = Rect::from_center_size(Pos2::new(row.left() + 35.0, cy), Vec2::splat(34.0));
    p.rect_filled(badge_rect, 8.0, theme::TILE_BG);
    let glyph = Rect::from_center_size(badge_rect.center(), Vec2::splat(20.0));
    icons::draw_device(p, glyph, d.device_type, theme::TEXT_DIM);
    p.text(
        Pos2::new(badge_rect.right() + 12.0, cy),
        Align2::LEFT_CENTER,
        &d.name,
        theme::medium(13.0),
        theme::TEXT,
    );
    p.text(
        Pos2::new(row.right() - 18.0, cy),
        Align2::RIGHT_CENTER,
        model::type_label(d),
        theme::body_sm(),
        theme::TEXT_MUT,
    );
    resp.clicked()
}

pub fn pairing(ui: &mut egui::Ui, ctx: &TabCtx) {
    let Some(ps) = ctx.dev.capabilities.iter().find_map(|c| match c {
        DeviceCapability::Pairing(p) => Some(p.clone()),
        _ => None,
    }) else {
        return;
    };
    let id = ctx.dev.id.clone();
    let listening = matches!(ps.state, PairingState::Listening);

    let cols_resp = ui.scope(|ui| {
        ui.columns(2, |cols| {
            receiver_panel(&mut cols[0], &id, ctx, &ps, listening);
            slots_panel(&mut cols[1], &id, ctx, &ps);
        });
    });
    tour::anchor(ui.ctx(), AnchorId::TabPairing, cols_resp.response.rect);
}

const PAIR_TIMEOUT_SECS: u8 = 30;

fn receiver_panel(ui: &mut egui::Ui, id: &str, ctx: &TabCtx, ps: &PairingStatus, listening: bool) {
    let state_key = egui::Id::unique(("pair_started", id));
    let cancel_key = egui::Id::unique(("pair_cancelled", id));
    let now = ui.ctx().input(|i| i.time);

    let cancelled = ui
        .ctx()
        .data(|d| d.get_temp::<bool>(cancel_key).unwrap_or(false));
    let listening = listening && !cancelled;

    // Track when listening started; clear when not listening.
    if listening {
        let already_set = ui.ctx().data(|d| d.get_temp::<f64>(state_key).is_some());
        if !already_set {
            ui.ctx().data_mut(|d| d.insert_temp(state_key, now));
        }
    } else {
        ui.ctx().data_mut(|d| d.remove::<f64>(state_key));
    }

    let remaining_secs: u8 = if listening {
        let started = ui
            .ctx()
            .data(|d| d.get_temp::<f64>(state_key).unwrap_or(now));
        let elapsed = (now - started).min(u8::MAX as f64) as u8;
        PAIR_TIMEOUT_SECS.saturating_sub(elapsed)
    } else {
        0
    };

    widgets::card(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new(&ctx.dev.name)
                    .font(theme::title())
                    .color(theme::TEXT),
            );
            widgets::section_label(ui, &t!("device.children_receiver"));
            ui.add_space(20.0);

            // Big 120px circle
            let r = 60.0_f32;
            let (circ_rect, _) = ui.allocate_exact_size(Vec2::splat(r * 2.0), Sense::hover());
            let center = circ_rect.center();
            let p = ui.painter();

            if listening {
                let fraction = f32::from(remaining_secs) / f32::from(PAIR_TIMEOUT_SECS.max(1));
                widgets::ring_gauge(p, center, r * 2.0, fraction, theme::CYAN);
                p.text(
                    center,
                    Align2::CENTER_CENTER,
                    remaining_secs.to_string(),
                    theme::semibold(30.0),
                    theme::TEXT,
                );
                ui.ctx().request_repaint();
            } else {
                p.circle_filled(center, r, theme::TILE_BG);
                p.circle_stroke(center, r, Stroke::new(1.0, theme::BORDER_INNER));
                p.text(
                    center,
                    Align2::CENTER_CENTER,
                    "+",
                    theme::body(30.0),
                    theme::TEXT_DIM,
                );
            }

            ui.add_space(theme::SPACE_9);

            let used = ps.slots.len();
            let max = ps.max_slots as usize;

            if listening {
                ui.label(
                    egui::RichText::new(t!("device.children_hold_pair_button"))
                        .font(theme::body_md())
                        .color(theme::TEXT_DIM),
                );
                ui.add_space(theme::SPACE_8);
                if widgets::button(
                    ui,
                    &t!("device.children_cancel"),
                    widgets::ButtonKind::Secondary,
                    egui::vec2(90.0, 36.0),
                )
                .clicked()
                {
                    ui.ctx().data_mut(|d| d.insert_temp(cancel_key, true));
                    ipc::send(
                        ctx.cmd,
                        halod_shared::commands::DaemonCommand::Device {
                            device_id: id.to_string(),
                            op: DeviceOp::ReceiverStopPairing,
                        },
                    );
                }
            } else {
                ui.label(
                    egui::RichText::new(t!("device.children_slots_in_use", used = used, max = max))
                        .font(theme::body_md())
                        .color(theme::TEXT_MUT),
                );
                ui.add_space(theme::SPACE_8);
                if widgets::button(
                    ui,
                    &t!("device.children_pair_new_device"),
                    widgets::ButtonKind::Primary,
                    egui::vec2(130.0, 36.0),
                )
                .clicked()
                {
                    ui.ctx().data_mut(|d| d.remove::<bool>(cancel_key));
                    ipc::send(
                        ctx.cmd,
                        halod_shared::commands::DaemonCommand::Device {
                            device_id: id.to_string(),
                            op: DeviceOp::ReceiverStartPairing {
                                timeout_secs: PAIR_TIMEOUT_SECS,
                            },
                        },
                    );
                }
            }
        });
    });
}

fn slots_panel(ui: &mut egui::Ui, id: &str, ctx: &TabCtx, ps: &PairingStatus) {
    titled_list_card(ui, &t!("device.children_paired_slots"), |ui| {
        let max = ps
            .max_slots
            .max(ps.slots.iter().map(|s| s.slot).max().unwrap_or(0));
        for i in 1..=max {
            let slot = ps.slots.iter().find(|s| s.slot == i);
            pair_slot_row(ui, id, ctx, i, slot);
        }
    });
}

fn pair_slot_row(
    ui: &mut egui::Ui,
    id: &str,
    ctx: &TabCtx,
    slot_idx: u8,
    slot: Option<&PairingSlot>,
) {
    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::hover());
    let p = ui.painter();
    p.line_segment(
        [row.left_bottom(), row.right_bottom()],
        Stroke::new(1.0, theme::BORDER_SOFT),
    );
    let cy = row.center().y;
    let dot = Pos2::new(row.left() + 22.0, cy);
    let name_x = dot.x + 16.0;
    let slot_label = t!("devtabs.slot_n", n = slot_idx);

    let Some(slot) = slot else {
        p.circle_filled(dot, 4.0, theme::BORDER_INNER);
        p.text(
            Pos2::new(name_x, cy - 9.0),
            Align2::LEFT_CENTER,
            slot_label,
            theme::medium(13.0),
            theme::TEXT_DIM,
        );
        p.text(
            Pos2::new(name_x, cy + 9.0),
            Align2::LEFT_CENTER,
            t!("device.children_available"),
            theme::body_sm(),
            theme::TEXT_MUT,
        );
        return;
    };
    p.circle_filled(dot, 4.0, theme::ONLINE);
    p.text(
        Pos2::new(name_x, cy - 9.0),
        Align2::LEFT_CENTER,
        &slot.name,
        theme::medium(13.0),
        theme::TEXT,
    );
    p.text(
        Pos2::new(name_x, cy + 9.0),
        Align2::LEFT_CENTER,
        slot_label,
        theme::body_sm(),
        theme::TEXT_MUT,
    );
    let btn = Rect::from_min_size(
        Pos2::new(row.right() - 18.0 - 76.0, cy - 15.0),
        Vec2::new(76.0, 30.0),
    );
    if widgets::button_at(
        ui,
        btn,
        egui::Id::unique(("unpair", id, slot_idx)),
        &t!("device.children_unpair"),
        widgets::ButtonKind::Danger,
    )
    .clicked()
    {
        ipc::send(
            ctx.cmd,
            halod_shared::commands::DaemonCommand::Device {
                device_id: id.to_string(),
                op: DeviceOp::ReceiverUnpair { slot: slot_idx },
            },
        );
    }
}
