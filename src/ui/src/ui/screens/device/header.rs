// SPDX-License-Identifier: GPL-3.0-or-later
//! Device page header: name/badge row, status chips, battery chips.

use egui::{Align2, Pos2, Rect, Stroke, Vec2};
use halod_shared::types::{Battery, BatteryStatus, ConnectionType, DeviceCapability, WireDevice};

use crate::ui::theme::{self, a};

/// Total LED count reported across all of the device's RGB capabilities and zones.
pub(super) fn led_count(dev: &WireDevice) -> usize {
    dev.capabilities
        .iter()
        .filter_map(|c| match c {
            DeviceCapability::Rgb(r) => Some(
                r.descriptor
                    .zones
                    .iter()
                    .map(|z| z.leds.len())
                    .sum::<usize>(),
            ),
            _ => None,
        })
        .sum()
}

pub(super) fn header(
    ui: &mut egui::Ui,
    dev: &WireDevice,
    ui_state: &mut super::DeviceUi,
    cmd: &crate::runtime::ipc::CommandTx,
) {
    let color = theme::device_color(dev);
    let editing_rename = ui_state.rename_editing;
    let header_h = if editing_rename { 64.0 } else { 50.0 };
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), header_h),
        egui::Sense::hover(),
    );
    crate::domain::tour::anchor(ui.ctx(), crate::domain::tour::AnchorId::DeviceHeader, rect);
    let cy = rect.center().y;
    let tx = rect.left() + 76.0; // badge width (60) + gap (16)

    {
        let badge = Rect::from_min_size(Pos2::new(rect.left(), cy - 28.0), Vec2::new(60.0, 56.0));
        let p = ui.painter();
        theme::glow(p, badge.center(), 30.0, color, 0.45);
        crate::ui::components::device_badge(p, badge, dev.device_type);
    }

    // ── Name row: edit mode or static ────────────────────────────────────────

    if ui_state.rename_editing {
        let row_cy = cy - 11.0;
        let input_rect = Rect::from_min_size(Pos2::new(tx, row_cy - 15.0), Vec2::new(300.0, 30.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(input_rect).layout(
            egui::Layout::centered_and_justified(egui::Direction::TopDown),
        ));
        let te_resp = child.add(
            egui::TextEdit::singleline(&mut ui_state.rename_val)
                .font(theme::bold(20.0))
                .margin(egui::vec2(10.0, 4.0)),
        );
        if ui_state.rename_just_started {
            te_resp.request_focus();
            ui_state.rename_just_started = false;
        }

        let commit = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));

        let save_rect = Rect::from_min_size(
            Pos2::new(input_rect.right() + 8.0, row_cy - 14.0),
            Vec2::new(56.0, 28.0),
        );
        let cancel_rect = Rect::from_min_size(
            Pos2::new(save_rect.right() + 6.0, row_cy - 14.0),
            Vec2::new(64.0, 28.0),
        );
        let save_resp = crate::ui::components::button_at(
            ui,
            save_rect,
            ui.id().with("rename_save"),
            &t!("devtabs.save"),
            crate::ui::components::ButtonKind::Primary,
        );
        let cancel_resp = crate::ui::components::button_at(
            ui,
            cancel_rect,
            ui.id().with("rename_cancel"),
            &t!("devtabs.cancel"),
            crate::ui::components::ButtonKind::Ghost,
        );

        if commit || save_resp.clicked() {
            let trimmed = ui_state.rename_val.trim().to_string();
            if !trimmed.is_empty() {
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::SetDeviceName {
                        device_id: dev.id.clone(),
                        name: trimmed,
                    },
                );
            }
            ui_state.rename_editing = false;
        }
        if esc || cancel_resp.clicked() {
            ui_state.rename_editing = false;
        }
    } else {
        let name_w = ui
            .painter()
            .layout_no_wrap(dev.name.clone(), theme::bold(22.0), theme::TEXT)
            .size()
            .x;

        let btn_rect = Rect::from_center_size(
            Pos2::new(tx + name_w + 22.0, cy - 10.0),
            Vec2::new(28.0, 28.0),
        );
        let btn_id = egui::Id::new("device_rename_btn").with(&dev.id);
        let btn_resp = ui.interact(btn_rect, btn_id, egui::Sense::click());
        let t = ui
            .ctx()
            .animate_bool_with_time(btn_resp.id, btn_resp.hovered(), 0.12);
        let icon_col = theme::lerp_color(theme::TEXT_MUT, theme::CYAN, t);
        let border_col = theme::lerp_color(theme::BORDER, theme::hex(0x234650), t);
        if btn_resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if btn_resp.clicked() {
            ui_state.rename_editing = true;
            ui_state.rename_just_started = true;
            ui_state.rename_val = dev.name.clone();
        }

        let p = ui.painter();
        p.text(
            Pos2::new(tx, cy - 10.0),
            Align2::LEFT_CENTER,
            &dev.name,
            theme::bold(22.0),
            theme::TEXT,
        );
        p.rect_filled(btn_rect, 8.0, theme::hex(0x10141d));
        p.rect_stroke(
            btn_rect,
            8.0,
            Stroke::new(1.0, border_col),
            egui::StrokeKind::Middle,
        );
        crate::ui::icons::draw_pencil(p, btn_rect, icon_col);
    }

    // ── Subtitle + status chips (always visible) ──────────────────────────────
    let leds = led_count(dev);
    let mut sub = crate::domain::models::device::type_label(dev).to_string();
    if leds > 0 {
        sub.push_str(&format!(" · {leds} LEDs"));
    }
    let vm = format!("{} {}", dev.vendor, dev.model);
    if !vm.trim().is_empty() {
        sub.push_str(&format!(" · {}", vm.trim()));
    }
    let p = ui.painter();
    p.text(
        Pos2::new(tx, cy + if editing_rename { 13.0 } else { 11.0 }),
        Align2::LEFT_CENTER,
        sub,
        theme::body(12.0),
        theme::TEXT_MUT,
    );
    status_chips(p, rect, dev);
    ui.add_space(14.0);
}

/// All reported battery cells across the device's `Battery` capability.
fn batteries(dev: &WireDevice) -> Vec<&Battery> {
    dev.capabilities
        .iter()
        .find_map(|c| match c {
            DeviceCapability::Battery(b) => Some(b.iter().collect()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The device's link state, if it exposes the `Connection` capability (only
/// wireless-capable devices do).
fn connection(dev: &WireDevice) -> Option<ConnectionType> {
    dev.capabilities.iter().find_map(|c| match c {
        DeviceCapability::Connection(s) => Some(s.connection_type),
        _ => None,
    })
}

pub(super) fn status_chips(p: &egui::Painter, rect: Rect, dev: &WireDevice) {
    let cy = rect.center().y;
    // Only show a status chip when offline; connected devices with no battery
    // need no label ("Connected" is visually redundant — you're on its page).
    if !dev.connected {
        let st = p.text(
            Pos2::new(rect.right(), cy),
            Align2::RIGHT_CENTER,
            t!("devtabs.offline"),
            theme::body(11.5),
            theme::OFFLINE_TEXT,
        );
        p.circle_filled(Pos2::new(st.left() - 9.0, cy), 3.5, theme::OFFLINE);
        return;
    }

    let mut right = rect.right();
    // Filter to cells with a known status; Unknown means the wireless link is
    // down (headset off) — show nothing rather than stale 0% data.
    let known: Vec<_> = batteries(dev)
        .into_iter()
        .filter(|b| !matches!(b.status, BatteryStatus::Unknown))
        .collect();
    let multi = known.len() > 1;
    // Lay chips right-to-left so the first cell sits leftmost.
    for (i, b) in known.iter().enumerate().rev() {
        let label = if multi {
            t!("devtabs.battery_n", n = i + 1).to_string()
        } else {
            t!("devtabs.battery").to_string()
        };
        right = battery_chip(p, right, cy, b, &label) - 8.0;
    }
    if let Some(ct) = connection(dev) {
        connection_chip(p, right, cy, ct);
    }
}

/// Draw a status-chip pill (background + border) of width `w` with its right
/// edge at `right`, returning the pill rect for glyph/text placement.
fn chip_pill(p: &egui::Painter, right: f32, cy: f32, w: f32) -> Rect {
    let pill = Rect::from_min_size(Pos2::new(right - w, cy - 17.0), Vec2::new(w, 34.0));
    p.rect_filled(pill, 9.0, theme::CARD_BG);
    p.rect_stroke(
        pill,
        9.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Middle,
    );
    pill
}

/// Draw the wired/wireless chip with its right edge at `right`.
fn connection_chip(p: &egui::Painter, right: f32, cy: f32, ct: ConnectionType) {
    let wireless = matches!(ct, ConnectionType::Wireless);
    let col = if wireless {
        theme::STAT_CYAN
    } else {
        theme::TEXT_MUT
    };
    let pill = chip_pill(p, right, cy, 140.0);
    let body = Rect::from_min_size(
        Pos2::new(pill.left() + 12.0, cy - 6.0),
        Vec2::new(18.0, 12.0),
    );
    crate::ui::components::connection_glyph(p, body, wireless, col);

    let tx = body.right() + 12.0;
    p.text(
        Pos2::new(tx, cy - 8.0),
        Align2::LEFT_CENTER,
        t!("devtabs.info_connection"),
        theme::body(9.0),
        theme::TEXT_FAINT,
    );
    p.text(
        Pos2::new(tx, cy + 7.0),
        Align2::LEFT_CENTER,
        if wireless {
            t!("devtabs.wireless")
        } else {
            t!("devtabs.wired")
        },
        theme::semibold(12.0),
        a(col, 0.95),
    );
}

/// Draw one battery chip (glyph + percentage + label/state) with its right edge
/// at `right`. Returns the chip's left edge.
fn battery_chip(p: &egui::Painter, right: f32, cy: f32, b: &Battery, label: &str) -> f32 {
    let charging = matches!(b.status, BatteryStatus::Charging);
    let col = theme::battery_color(b.level, charging);
    let pill = chip_pill(p, right, cy, 150.0);
    let body = Rect::from_min_size(
        Pos2::new(pill.left() + 12.0, cy - 6.0),
        Vec2::new(22.0, 12.0),
    );
    crate::ui::components::battery_glyph(p, body, b.level, col);

    let tx = body.right() + 12.0;
    p.text(
        Pos2::new(tx, cy - 8.0),
        Align2::LEFT_CENTER,
        label,
        theme::body(9.0),
        theme::TEXT_FAINT,
    );
    let pct = p.text(
        Pos2::new(tx, cy + 7.0),
        Align2::LEFT_CENTER,
        format!("{}%", b.level),
        theme::mono_semibold(12.0),
        theme::TEXT_BRIGHT,
    );
    let state = if charging {
        t!("devtabs.charging_bolt")
    } else {
        battery_status_label(&b.status)
    };
    p.text(
        Pos2::new(pct.right() + 6.0, cy + 7.0),
        Align2::LEFT_CENTER,
        state,
        theme::body(9.5),
        a(col, 0.95),
    );
    pill.left()
}

/// Greyed "charging"/"discharging" helper kept for battery cards.
pub(super) fn battery_status_label(s: &BatteryStatus) -> std::borrow::Cow<'static, str> {
    match s {
        BatteryStatus::Charging => t!("devtabs.charging"),
        BatteryStatus::Discharging => t!("devtabs.discharging"),
        BatteryStatus::Unknown => t!("devtabs.unknown"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::DeviceType;

    fn dev(ty: DeviceType, caps: Vec<DeviceCapability>) -> WireDevice {
        WireDevice {
            device_type: ty,
            capabilities: caps,
            ..Default::default()
        }
    }

    #[test]
    fn connection_reads_link_from_capability() {
        use halod_shared::types::ConnectionStatus;
        let with = dev(
            DeviceType::Mouse,
            vec![DeviceCapability::Connection(ConnectionStatus {
                connection_type: ConnectionType::Wireless,
            })],
        );
        assert_eq!(connection(&with), Some(ConnectionType::Wireless));
        // Wired-only devices omit the capability → no indicator.
        assert_eq!(connection(&dev(DeviceType::Keyboard, vec![])), None);
    }

    #[test]
    fn led_count_sums_leds_across_zones() {
        use halod_shared::types::{LedPosition, RgbDescriptor, RgbStatus, RgbZone, ZoneTopology};
        let zone = |n: u32| RgbZone {
            id: "z".into(),
            name: "z".into(),
            topology: ZoneTopology::Linear,
            leds: (0..n)
                .map(|i| LedPosition {
                    id: i,
                    x: 0.0,
                    y: 0.0,
                })
                .collect(),
        };
        let cap = DeviceCapability::Rgb(RgbStatus {
            descriptor: RgbDescriptor {
                zones: vec![zone(3), zone(5)],
                native_effects: vec![],
            },
            state: None,
            zone_transforms: Default::default(),
            chainable_channels: vec![],
        });
        assert_eq!(led_count(&dev(DeviceType::Keyboard, vec![cap])), 8);
        // No RGB capability → zero.
        assert_eq!(led_count(&dev(DeviceType::Other, vec![])), 0);
    }
}
