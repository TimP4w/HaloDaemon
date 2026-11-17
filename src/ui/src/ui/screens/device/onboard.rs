// SPDX-License-Identifier: GPL-3.0-or-later
//! Onboard tab — onboard-memory profile slots (switch active, restore ROM default).

use crate::ui::components as widgets;
use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::commands::DeviceOp;
use halod_shared::types::{Boolean, DeviceCapability, OnboardProfileSlot, OnboardProfiles};

use super::TabCtx;
use crate::domain::tour;
use crate::domain::tour::AnchorId;
use crate::runtime::ipc;
use crate::ui::theme;

/// Boolean-capability key for the ONBOARD_PROFILES host/onboard mode switch.
/// Rendered here rather than in the generic Controls tab.
pub(crate) const HOST_MODE_KEY: &str = "host_mode";

/// The writable host-mode boolean, if the device exposes one.
fn host_mode_boolean(caps: &[DeviceCapability]) -> Option<&Boolean> {
    caps.iter().find_map(|c| match c {
        DeviceCapability::Boolean(items) => items.iter().find(|b| b.key == HOST_MODE_KEY),
        _ => None,
    })
}

/// Whether a slot's Switch button is inert. In host mode the firmware doesn't
/// drive onboard profiles, so switching is meaningless; the already-active slot
/// is also a no-op to switch to.
fn switch_disabled(host_mode: bool, active: bool) -> bool {
    host_mode || active
}

fn slot_can_toggle(slot: &OnboardProfileSlot) -> bool {
    slot.index != 1 && !slot.read_only
}

fn slot_can_restore(slot: &OnboardProfileSlot) -> bool {
    !slot.read_only && slot.has_rom_default && slot.enabled
}

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx) {
    tour::anchor(ui.ctx(), AnchorId::TabOnboard, ui.max_rect());
    let Some(op) = ctx.dev.capabilities.iter().find_map(|c| match c {
        DeviceCapability::OnboardProfiles(p) => Some(p),
        _ => None,
    }) else {
        return;
    };
    let id = ctx.dev.id.clone();

    ui.label(
        egui::RichText::new(t!("devtabs.onboard_intro"))
            .font(theme::body_md())
            .color(theme::TEXT_MUT),
    );
    ui.add_space(theme::SPACE_6);

    if let Some(b) = host_mode_boolean(&ctx.dev.capabilities) {
        host_mode_row(ui, &id, ctx, b);
        ui.add_space(theme::SPACE_6);
    }

    widgets::card_frameless(ui, |ui| {
        ui.style_mut().spacing.item_spacing = egui::vec2(0.0, 0.0);
        let host_mode = host_mode_boolean(&ctx.dev.capabilities).is_some_and(|b| b.value);
        for slot in &op.slots {
            slot_row(ui, &id, ctx, op, slot, host_mode);
        }
    });
}

fn host_mode_row(ui: &mut egui::Ui, id: &str, ctx: &TabCtx, b: &Boolean) {
    widgets::card(ui, |ui| {
        widgets::setting_row(
            ui,
            &t!("devtabs.host_mode"),
            &t!("devtabs.host_mode_desc"),
            |ui| {
                if !b.read_only && widgets::toggle(ui, b.value) != b.value {
                    ipc::send(
                        ctx.cmd,
                        halod_shared::commands::DaemonCommand::Device {
                            device_id: id.to_string(),
                            op: DeviceOp::SetBoolean {
                                key: b.key.clone(),
                                value: !b.value,
                            },
                        },
                    );
                }
            },
        );
    });
}

fn slot_row(
    ui: &mut egui::Ui,
    id: &str,
    ctx: &TabCtx,
    op: &OnboardProfiles,
    slot: &OnboardProfileSlot,
    host_mode: bool,
) {
    let active = slot.active || slot.index == op.active_slot;
    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 56.0), Sense::hover());
    let p = ui.painter();
    p.line_segment(
        [row.left_bottom(), row.right_bottom()],
        Stroke::new(1.0, theme::BORDER_SOFT),
    );

    let cy = row.center().y;
    let (dot, status) = match (active, slot.enabled) {
        (true, _) => (theme::CYAN, t!("devtabs.slot_active")),
        (false, true) => (theme::TEXT_FAINT2, t!("devtabs.slot_saved")),
        (false, false) => (theme::BORDER_INNER, t!("devtabs.slot_empty")),
    };
    p.circle_filled(Pos2::new(row.left() + 22.0, cy), 4.0, dot);
    let title = p.text(
        Pos2::new(row.left() + 38.0, cy - 9.0),
        Align2::LEFT_CENTER,
        t!("devtabs.slot_n", n = slot.index),
        theme::medium(14.0),
        theme::TEXT,
    );
    p.text(
        Pos2::new(row.left() + 38.0, cy + 10.0),
        Align2::LEFT_CENTER,
        status,
        theme::body_sm(),
        theme::TEXT_MUT,
    );
    if slot.read_only {
        let w =
            p.layout_no_wrap(
                t!("devtabs.read_only").into(),
                theme::caption(),
                theme::TEXT,
            )
            .size()
            .x + 16.0;
        widgets::paint_tag(
            p,
            Pos2::new(title.right() + 10.0 + w, title.center().y),
            &t!("devtabs.read_only"),
            widgets::TagTone::Neutral,
        );
    }

    let btn_area = Rect::from_min_max(
        Pos2::new(row.right() - 310.0, row.top()),
        row.right_bottom(),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(btn_area), |ui| {
        // Laid out right-to-left, so buttons are added in reverse of their
        // visual order: Restore to ROM → Delete → Switch (left to right).
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.add_space(theme::SPACE_8);
            // Switch — greyed out (non-interactive) in host mode or on the
            // already-active slot.
            if slot.enabled
                && widgets::button_enabled(
                    ui,
                    &t!("devtabs.switch"),
                    widgets::ButtonKind::Secondary,
                    egui::vec2(68.0, 28.0),
                    !switch_disabled(host_mode, active),
                )
            {
                ipc::send(
                    ctx.cmd,
                    halod_shared::commands::DaemonCommand::Device {
                        device_id: id.to_string(),
                        op: DeviceOp::OnboardProfileSwitch { slot: slot.index },
                    },
                );
            }
            if slot_can_toggle(slot) {
                let (label, enabled_after, kind) = if slot.enabled {
                    (t!("devtabs.remove"), false, widgets::ButtonKind::Danger)
                } else {
                    (t!("devtabs.create"), true, widgets::ButtonKind::Ghost)
                };
                if widgets::button(ui, &label, kind, egui::vec2(68.0, 28.0)).clicked() {
                    ipc::send(
                        ctx.cmd,
                        halod_shared::commands::DaemonCommand::Device {
                            device_id: id.to_string(),
                            op: DeviceOp::OnboardProfileSetEnabled {
                                slot: slot.index,
                                enabled: enabled_after,
                            },
                        },
                    );
                }
            }
            if slot_can_restore(slot)
                && widgets::button(
                    ui,
                    &t!("devtabs.restore_to_rom"),
                    widgets::ButtonKind::Secondary,
                    egui::vec2(112.0, 28.0),
                )
                .clicked()
            {
                ipc::send(
                    ctx.cmd,
                    halod_shared::commands::DaemonCommand::Device {
                        device_id: id.to_string(),
                        op: DeviceOp::OnboardProfileRestore { slot: slot.index },
                    },
                );
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rom_slots_are_selectable_but_never_mutable() {
        let mut slot = OnboardProfileSlot {
            index: 0x101,
            read_only: true,
            enabled: true,
            active: false,
            has_rom_default: true,
        };
        assert!(!switch_disabled(false, false));
        assert!(!slot_can_toggle(&slot));
        assert!(!slot_can_restore(&slot));
        slot.index = 1;
        slot.read_only = false;
        assert!(!slot_can_toggle(&slot));
        assert!(slot_can_restore(&slot));
        slot.index = 2;
        assert!(slot_can_toggle(&slot));
        slot.enabled = false;
        assert!(!slot_can_restore(&slot));
    }

    fn boolean(key: &str) -> Boolean {
        Boolean {
            key: key.to_string(),
            label: key.to_string(),
            value: true,
            read_only: false,
            category: "Profiles".to_string(),
            visible_when: None,
        }
    }

    #[test]
    fn host_mode_boolean_finds_key_among_capabilities() {
        let caps = vec![
            DeviceCapability::OnboardProfiles(OnboardProfiles {
                active_slot: 0,
                slots: vec![],
            }),
            DeviceCapability::Boolean(vec![boolean("something_else"), boolean(HOST_MODE_KEY)]),
        ];
        let found = host_mode_boolean(&caps).expect("host mode present");
        assert_eq!(found.key, HOST_MODE_KEY);
    }

    #[test]
    fn switch_disabled_in_host_mode_or_when_active() {
        assert!(switch_disabled(true, false), "host mode disables switch");
        assert!(switch_disabled(true, true), "host mode disables switch");
        assert!(switch_disabled(false, true), "active slot disables switch");
        assert!(
            !switch_disabled(false, false),
            "onboard mode, non-active slot is switchable"
        );
    }

    #[test]
    fn host_mode_boolean_absent_when_no_matching_key() {
        let caps = vec![DeviceCapability::Boolean(vec![boolean("something_else")])];
        assert!(host_mode_boolean(&caps).is_none());
        assert!(host_mode_boolean(&[]).is_none());
    }
}
