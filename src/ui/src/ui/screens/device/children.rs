// SPDX-License-Identifier: GPL-3.0-or-later
//! Devices tab (hub child devices) and Pairing tab (wireless receiver slots).

use crate::ui::components as widgets;
use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{DeviceCapability, PairingSlot, PairingState, PairingStatus, WireDevice};

use super::TabCtx;
use crate::domain::models::device as model;
use crate::domain::state::Page;
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
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::TabChildrenList,
        card.response.rect,
    );
}

/// The shared card shell of the child-device and pairing-slot lists: `CARD_BG`
/// surface, 48 px title row over a soft divider, then zero-spaced list rows.
fn titled_list_card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .fill(theme::CARD_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::RADIUS_XL)
        .show(ui, |ui| {
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
    let (row, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::click());
    let hovered = resp.hovered();
    let p = ui.painter();
    if hovered {
        p.rect_filled(
            row.shrink2(Vec2::new(6.0, 5.0)),
            theme::RADIUS_MD,
            theme::ROW_ACTIVE,
        );
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    p.line_segment(
        [row.left_bottom(), row.right_bottom()],
        Stroke::new(1.0, theme::BORDER_SOFT),
    );

    let cy = row.center().y;
    let badge_rect =
        Rect::from_center_size(Pos2::new(row.left() + 37.0, cy), Vec2::new(38.0, 28.0));
    p.rect_filled(badge_rect, theme::RADIUS_SM, theme::device_color(d));
    let glyph = Rect::from_center_size(badge_rect.center(), Vec2::splat(badge_rect.height() * 0.8));
    crate::ui::icons::draw_device(p, glyph, d.device_type, theme::hex(0x0a0d13));
    p.text(
        Pos2::new(badge_rect.right() + 13.0, cy),
        Align2::LEFT_CENTER,
        &d.name,
        theme::semibold(12.5),
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
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::TabPairing,
        cols_resp.response.rect,
    );
}

const PAIR_TIMEOUT_SECS: u8 = 30;

fn receiver_panel(ui: &mut egui::Ui, id: &str, ctx: &TabCtx, ps: &PairingStatus, listening: bool) {
    let state_key = egui::Id::new(("pair_started", id));
    let cancel_key = egui::Id::new(("pair_cancelled", id));
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
            widgets::caps_label(ui, &t!("device.children_receiver"));
            ui.add_space(theme::SPACE_3);
            ui.label(
                egui::RichText::new(&ctx.dev.name)
                    .font(theme::title())
                    .color(theme::TEXT),
            );
            ui.add_space(20.0);

            // Big 120px circle
            let r = 60.0_f32;
            let (circ_rect, _) = ui.allocate_exact_size(Vec2::splat(r * 2.0), Sense::hover());
            let center = circ_rect.center();
            let p = ui.painter();

            if listening {
                let t = now as f32;
                let pulse = (t * std::f32::consts::TAU / 1.4).sin() * 0.5 + 0.5;
                theme::glow(p, center, r + 10.0, theme::CYAN, 0.15 + 0.1 * pulse);
                p.circle_stroke(center, r - 1.5, Stroke::new(3.0, theme::CYAN));
                p.text(
                    center,
                    Align2::CENTER_CENTER,
                    remaining_secs.to_string(),
                    theme::mono_bold(28.0),
                    theme::CYAN,
                );
                ui.ctx().request_repaint();
            } else {
                dashed_circle(p, center, r - 1.0, 2.0, theme::hex(0x2a3446));
                p.text(
                    center,
                    Align2::CENTER_CENTER,
                    "+",
                    theme::body(30.0),
                    theme::TEXT_FAINT,
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
                    widgets::ButtonKind::Ghost,
                    egui::vec2(90.0, 36.0),
                )
                .clicked()
                {
                    ui.ctx().data_mut(|d| d.insert_temp(cancel_key, true));
                    crate::runtime::ipc::send(
                        ctx.cmd,
                        halod_shared::commands::DaemonCommand::ReceiverStopPairing {
                            id: id.to_string(),
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
                    crate::runtime::ipc::send(
                        ctx.cmd,
                        halod_shared::commands::DaemonCommand::ReceiverStartPairing {
                            id: id.to_string(),
                            timeout_secs: PAIR_TIMEOUT_SECS,
                        },
                    );
                }
            }
        });
    });
}

fn dashed_circle(p: &egui::Painter, center: Pos2, r: f32, stroke_w: f32, color: egui::Color32) {
    const N: usize = 16;
    const FILL: f32 = 0.55;
    for i in 0..N {
        let a0 = std::f32::consts::TAU * i as f32 / N as f32;
        let a1 = a0 + std::f32::consts::TAU * FILL / N as f32;
        let pts: Vec<Pos2> = (0..=4)
            .map(|j| {
                let a = a0 + (a1 - a0) * j as f32 / 4.0;
                Pos2::new(center.x + a.cos() * r, center.y + a.sin() * r)
            })
            .collect();
        p.add(egui::Shape::line(pts, Stroke::new(stroke_w, color)));
    }
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
    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), Sense::hover());
    let p = ui.painter();
    p.line_segment(
        [row.left_bottom(), row.right_bottom()],
        Stroke::new(1.0, theme::BORDER_SOFT),
    );

    let badge_rect = Rect::from_center_size(
        Pos2::new(row.left() + 37.0, row.center().y),
        Vec2::new(38.0, 28.0),
    );
    let name_x = badge_rect.right() + 13.0;
    let cy = row.center().y;

    if let Some(slot) = slot {
        let badge_color = theme::DEVICE_HUES[slot_idx as usize % theme::DEVICE_HUES.len()];
        p.rect_filled(badge_rect, theme::RADIUS_SM, badge_color);
        let code = slot_code(&slot.name);
        p.text(
            badge_rect.center(),
            Align2::CENTER_CENTER,
            &code,
            theme::mono_bold(10.0),
            theme::hex(0x0a0d13),
        );
        p.text(
            Pos2::new(name_x, cy),
            Align2::LEFT_CENTER,
            &slot.name,
            theme::body_md(),
            theme::TEXT,
        );

        let btn_area = Rect::from_min_max(
            Pos2::new(row.right() - 130.0, row.top()),
            row.right_bottom(),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(btn_area), |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(theme::SPACE_9);
                if widgets::button(
                    ui,
                    &t!("device.children_unpair"),
                    widgets::ButtonKind::Ghost,
                    egui::vec2(72.0, 28.0),
                )
                .clicked()
                {
                    crate::runtime::ipc::send(
                        ctx.cmd,
                        halod_shared::commands::DaemonCommand::ReceiverUnpair {
                            id: id.to_string(),
                            slot: slot_idx,
                        },
                    );
                }
            });
        });
    } else {
        p.rect_filled(badge_rect, theme::RADIUS_SM, theme::BORDER);
        p.text(
            Pos2::new(name_x, cy),
            Align2::LEFT_CENTER,
            t!("device.children_empty_slot"),
            theme::body_md(),
            theme::TEXT_FAINT,
        );
        p.text(
            Pos2::new(row.right() - 18.0, cy),
            Align2::RIGHT_CENTER,
            t!("device.children_available"),
            theme::body_sm(),
            theme::TEXT_FAINT,
        );
    }
}

/// Two-letter uppercase code derived from a device name (initials of words,
/// falling back to the first two alphanumeric characters).
fn slot_code(name: &str) -> String {
    let initials: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .filter(|c| c.is_alphanumeric())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    if initials.len() >= 2 {
        return initials;
    }
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::slot_code;

    #[test]
    fn slot_code_uses_word_initials() {
        assert_eq!(slot_code("Wireless Mouse"), "WM");
        assert_eq!(slot_code("logitech g pro"), "LG");
    }

    #[test]
    fn slot_code_falls_back_to_first_two_chars() {
        // Single word → first two alphanumerics.
        assert_eq!(slot_code("Keyboard"), "KE");
        // Leading non-alphanumerics are skipped in the fallback.
        assert_eq!(slot_code("-x1"), "X1");
    }

    #[test]
    fn slot_code_handles_short_names() {
        assert_eq!(slot_code("A"), "A");
        assert_eq!(slot_code(""), "");
    }
}
