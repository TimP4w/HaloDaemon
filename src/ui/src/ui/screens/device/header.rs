// SPDX-License-Identifier: GPL-3.0-or-later
//! Device page header: name/badge row, status chips, battery chips.

use egui::{Align2, Pos2, Rect, Stroke, Vec2};
use halod_shared::types::{Battery, BatteryStatus, ConnectionType, DeviceCapability, WireDevice};

use crate::ui::theme;

/// Total LED count reported across all of the device's RGB capabilities and channels.
pub(super) fn led_count(dev: &WireDevice) -> usize {
    dev.capabilities
        .iter()
        .filter_map(|c| match c {
            DeviceCapability::Lighting(r) => Some(
                r.descriptor
                    .channels
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
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 48.0), egui::Sense::hover());
    crate::domain::tour::anchor(ui.ctx(), crate::domain::tour::AnchorId::DeviceHeader, rect);
    let cy = rect.center().y;
    let tx = rect.left() + 64.0;

    {
        let tile = Rect::from_min_size(rect.min, Vec2::splat(48.0));
        let p = ui.painter();
        theme::paint_surface(p, tile, theme::RADIUS_MD, theme::TILE_BG, theme::TRACK);
        crate::ui::components::device_badge(p, tile.shrink(8.0), dev.device_type);
    }

    let name_cy = cy - 10.0;
    if ui_state.rename_editing {
        let input_rect = Rect::from_min_size(Pos2::new(tx, name_cy - 15.0), Vec2::new(300.0, 30.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(input_rect).layout(
            egui::Layout::centered_and_justified(egui::Direction::TopDown),
        ));
        let te_resp = child.add(
            egui::TextEdit::singleline(&mut ui_state.rename_val)
                .font(theme::semibold(20.0))
                .margin(egui::vec2(10.0, 4.0)),
        );
        if ui_state.rename_just_started {
            te_resp.request_focus();
            ui_state.rename_just_started = false;
        }

        let commit = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));

        let save_rect = Rect::from_min_size(
            Pos2::new(input_rect.right() + 8.0, name_cy - 14.0),
            Vec2::new(56.0, 28.0),
        );
        let cancel_rect = Rect::from_min_size(
            Pos2::new(save_rect.right() + 6.0, name_cy - 14.0),
            Vec2::new(64.0, 28.0),
        );
        let save_resp = crate::ui::components::button_at(
            ui,
            save_rect,
            ui.scope_id().with("rename_save"),
            &t!("devtabs.save"),
            crate::ui::components::ButtonKind::Primary,
        );
        let cancel_resp = crate::ui::components::button_at(
            ui,
            cancel_rect,
            ui.scope_id().with("rename_cancel"),
            &t!("devtabs.cancel"),
            crate::ui::components::ButtonKind::Secondary,
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
            .layout_no_wrap(dev.name.clone(), theme::semibold(24.0), theme::TEXT)
            .size()
            .x;

        let btn_rect = Rect::from_center_size(
            Pos2::new(tx + name_w + 22.0, name_cy),
            Vec2::new(28.0, 28.0),
        );
        let btn_id = egui::Id::unique("device_rename_btn").with(&dev.id);
        let btn_resp = ui.interact(btn_rect, btn_id, egui::Sense::click());
        let t = ui
            .ctx()
            .animate_bool_with_time(btn_resp.id, btn_resp.hovered(), 0.12);
        let icon_col = theme::lerp_color(theme::TEXT_DIM, theme::TEXT, t);
        let border_col = theme::lerp_color(theme::BORDER_INNER, theme::TEXT_FAINT2, t);
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
            Pos2::new(tx, name_cy),
            Align2::LEFT_CENTER,
            &dev.name,
            theme::semibold(24.0),
            theme::TEXT,
        );
        p.rect_stroke(
            btn_rect,
            theme::RADIUS_SM,
            Stroke::new(1.0, border_col),
            egui::StrokeKind::Middle,
        );
        crate::ui::icons::draw_pencil(p, btn_rect.shrink(7.0), icon_col);
    }

    let p = ui.painter();
    p.text(
        Pos2::new(tx, cy + 17.0),
        Align2::LEFT_CENTER,
        header_meta(dev),
        theme::body(14.0),
        theme::TEXT_DIM,
    );
    if !dev.connected {
        crate::ui::components::paint_tag(
            p,
            Pos2::new(rect.right(), cy),
            &t!("devtabs.offline"),
            crate::ui::components::TagTone::Danger,
        );
    }
}

/// "Type · link · headline stat", e.g. `CPU cooler · USB · 24 LEDs` or
/// `Headset · Wireless · Battery 64%`.
fn header_meta(dev: &WireDevice) -> String {
    let link = if connection(dev).or(dev.connection_type) == Some(ConnectionType::Wireless) {
        t!("devtabs.wireless").to_string()
    } else {
        crate::domain::models::device::transport_label(dev)
    };
    let mut parts = vec![
        crate::domain::models::device::type_label(dev).to_string(),
        link,
    ];
    let known: Vec<_> = batteries(dev)
        .into_iter()
        .filter(|b| !matches!(b.status, BatteryStatus::Unknown))
        .collect();
    let multi = known.len() > 1;
    for (i, b) in known.iter().enumerate() {
        let label = if multi {
            t!("devtabs.battery_n", n = i + 1).to_string()
        } else {
            t!("devtabs.battery").to_string()
        };
        parts.push(if matches!(b.status, BatteryStatus::Charging) {
            format!("{label} {}% {}", b.level, t!("devtabs.charging_bolt"))
        } else {
            format!("{label} {}%", b.level)
        });
    }
    let leds = led_count(dev);
    if known.is_empty() && leds > 0 {
        parts.push(t!("devtabs.led_count", n = leds).to_string());
    }
    parts.join(" · ")
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
        use halod_shared::types::{
            LedPosition, LightingChannel, LightingDescriptor, LightingStatus, ZoneTopology,
        };
        let zone = |n: u32| LightingChannel {
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
            color_order: Default::default(),
            division: Default::default(),
            visibility: Default::default(),
        };
        let cap = DeviceCapability::Lighting(LightingStatus {
            descriptor: LightingDescriptor {
                channels: vec![zone(3), zone(5)],
                native_effects: vec![],
            },
            state: None,
            channel_transforms: Default::default(),
        });
        assert_eq!(led_count(&dev(DeviceType::Keyboard, vec![cap])), 8);
        // No RGB capability → zero.
        assert_eq!(led_count(&dev(DeviceType::Other, vec![])), 0);
    }

    #[test]
    fn meta_reads_type_link_then_battery_or_leds() {
        use halod_shared::types::{ConnectionStatus, DeviceType};
        let mut headset = dev(
            DeviceType::Headset,
            vec![
                DeviceCapability::Connection(ConnectionStatus {
                    connection_type: ConnectionType::Wireless,
                }),
                DeviceCapability::Battery(vec![Battery {
                    key: "main".into(),
                    label: "Battery".into(),
                    level: 64,
                    status: BatteryStatus::Discharging,
                }]),
            ],
        );
        headset.transport = Some("hid".into());
        assert_eq!(header_meta(&headset), "Headset · Wireless · Battery 64%");

        let mut hub = dev(DeviceType::Hub, vec![]);
        hub.transport = Some("smbus".into());
        assert_eq!(header_meta(&hub), "Controller · SMBus");
    }
}
