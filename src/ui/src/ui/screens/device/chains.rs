// SPDX-License-Identifier: GPL-3.0-or-later
//! LED chains tab — per-header chain configuration for ARGB hubs and LED-strip controllers.

use crate::ui::components as widgets;
use egui::{Align2, Sense, Stroke, Vec2};
use halod_shared::commands::{DaemonCommand, DeviceOp};
use halod_shared::types::{
    DeviceCapability, LightingChannel, LightingDivision, LightingSegmentInfo, ZoneTopology,
};

use super::TabCtx;
#[cfg(test)]
use crate::domain::topic_store::TopicStore;
use crate::domain::tour;
use crate::domain::tour::AnchorId;
use crate::runtime::ipc;
use crate::ui::theme;

const ADD_LEDS_DEFAULT: u32 = 30;

/// Topology choices in the Add-link form: (constructor, LED-count divisor).
type TopologyChoice = (fn() -> ZoneTopology, u32);
const TOPOLOGY_CHOICES: &[TopologyChoice] = &[
    (|| ZoneTopology::Linear, 1),
    (|| ZoneTopology::Ring, 1),
    (|| ZoneTopology::Rings { count: 2 }, 2),
    (|| ZoneTopology::Rings { count: 3 }, 3),
    (|| ZoneTopology::Rings { count: 4 }, 4),
    (|| ZoneTopology::Grid, 1),
];

fn topology_label(t: &ZoneTopology) -> String {
    match t {
        ZoneTopology::Linear => t!("device.chains_topo_linear").to_string(),
        ZoneTopology::Ring => t!("device.chains_topo_ring").to_string(),
        ZoneTopology::Rings { count } => t!("device.chains_topo_rings", count = count).to_string(),
        ZoneTopology::Grid => t!("device.chains_topo_grid").to_string(),
        ZoneTopology::Keyboard { .. } => t!("device.chains_topo_keyboard").to_string(),
    }
}

fn topology_from_idx(idx: usize) -> ZoneTopology {
    TOPOLOGY_CHOICES
        .get(idx)
        .map(|(ctor, _)| ctor())
        .unwrap_or(ZoneTopology::Linear)
}

fn divisor_for_idx(idx: usize) -> u32 {
    TOPOLOGY_CHOICES.get(idx).map(|(_, d)| *d).unwrap_or(1)
}

/// The LED count typed into the Add-link form, if the header can take it with
/// the chosen topology.
fn parse_leds(text: &str, divisor: u32, remaining: u32) -> Option<u32> {
    let n: u32 = text.trim().parse().ok()?;
    (n > 0 && n <= remaining && n.is_multiple_of(divisor.max(1))).then_some(n)
}

fn segments(channel: &LightingChannel) -> &[LightingSegmentInfo] {
    match &channel.division {
        LightingDivision::Divisible { segments, .. } => segments,
        LightingDivision::Indivisible => &[],
    }
}

fn max_leds(channel: &LightingChannel) -> u32 {
    match channel.division {
        LightingDivision::Divisible { max_leds, .. } => max_leds,
        LightingDivision::Indivisible => 0,
    }
}

fn used_leds(channel: &LightingChannel) -> u32 {
    segments(channel).iter().map(|l| l.led_count).sum()
}

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx) {
    tour::anchor(ui.ctx(), AnchorId::TabChains, ui.max_rect());
    let channels: Vec<&LightingChannel> = ctx
        .dev
        .capabilities
        .iter()
        .find_map(|c| match c {
            DeviceCapability::Lighting(r) => Some(
                r.descriptor
                    .channels
                    .iter()
                    .filter(|channel| {
                        matches!(channel.division, LightingDivision::Divisible { .. })
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default();

    if channels.is_empty() {
        return;
    }

    let total_leds: u32 = channels.iter().map(|c| used_leds(c)).sum();
    let dev_id = ctx.dev.id.as_str();

    ui.horizontal(|ui| {
        let right_w = 260.0;
        ui.allocate_ui_with_layout(
            Vec2::new((ui.available_width() - right_w).max(200.0), 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.spacing_mut().item_spacing.y = theme::SPACE_2;
                ui.label(
                    egui::RichText::new(t!("device.chains_title"))
                        .font(theme::semibold(18.0))
                        .color(theme::TEXT),
                );
                ui.label(
                    egui::RichText::new(t!("device.chains_subtitle"))
                        .font(theme::body_md())
                        .color(theme::TEXT_DIM),
                );
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = theme::SPACE_6;
            discover_button(ui, ctx, dev_id, &channels);
            ui.label(
                egui::RichText::new(t!(
                    "device.chains_summary",
                    leds = total_leds,
                    headers = channels.len()
                ))
                .font(theme::body_lg())
                .color(theme::TEXT_SOFT),
            );
        });
    });
    ui.add_space(theme::SPACE_8);

    const COLS: usize = 3;
    for row in channels.chunks(COLS) {
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = theme::SPACE_8;
            ui.columns(COLS, |columns| {
                for (col, channel) in columns.iter_mut().zip(row) {
                    channel_card(col, ctx, dev_id, channel);
                }
            });
        });
        ui.add_space(theme::SPACE_8);
    }
}

fn channel_card(ui: &mut egui::Ui, ctx: &TabCtx, dev_id: &str, channel: &LightingChannel) {
    let used = used_leds(channel);
    let max = max_leds(channel);
    widgets::card_with_margin(ui, egui::Margin::same(16), |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = theme::SPACE_6;
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.label(
                    egui::RichText::new(&channel.name)
                        .font(theme::heading())
                        .color(theme::TEXT),
                );
            },
            |ui| {
                ui.label(
                    egui::RichText::new(t!("device.chains_usage", used = used, max = max))
                        .font(theme::body_md())
                        .color(theme::TEXT_DIM),
                );
            },
        );
        let frac = if max == 0 {
            0.0
        } else {
            used as f32 / max as f32
        };
        widgets::progress_bar(ui, ui.available_width(), 6.0, Some(frac));

        if segments(channel).is_empty() {
            empty_header(ui);
        }
        for (li, link) in segments(channel).iter().enumerate() {
            link_row(ui, ctx, dev_id, &channel.id, li + 1, link);
        }
        add_link_panel(ui, ctx, dev_id, channel, max.saturating_sub(used));
    });
}

fn empty_header(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 54.0), Sense::hover());
    let p = ui.painter();
    let stroke = Stroke::new(1.0, theme::BORDER_INNER);
    let c = rect.shrink(0.5);
    for seg in [
        [c.left_top(), c.right_top()],
        [c.right_top(), c.right_bottom()],
        [c.right_bottom(), c.left_bottom()],
        [c.left_bottom(), c.left_top()],
    ] {
        p.extend(egui::Shape::dashed_line(&seg, stroke, 4.0, 3.0));
    }
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        t!("device.chains_no_links"),
        theme::body_md(),
        theme::TEXT_MUT,
    );
}

fn discover_button(ui: &mut egui::Ui, ctx: &TabCtx, dev_id: &str, channels: &[&LightingChannel]) {
    let flash_at_key = egui::Id::unique(("ch_flash_at", dev_id));
    let now = ui.ctx().input(|i| i.time);
    let flashing = ui
        .ctx()
        .data(|d| d.get_temp::<f64>(flash_at_key))
        .is_some_and(|until| now < until);
    if flashing {
        ui.ctx().request_repaint();
    }
    let label = if flashing {
        t!("device.chains_flashing")
    } else {
        t!("device.chains_discover")
    };
    let resp = widgets::button(
        ui,
        &label,
        widgets::ButtonKind::Secondary,
        Vec2::new(88.0, 36.0),
    );
    tour::anchor(ui.ctx(), AnchorId::ChainsDiscover, resp.rect);
    if resp.clicked() && !flashing {
        ui.ctx()
            .data_mut(|d| d.insert_temp(flash_at_key, now + 2.4));
        for channel in channels {
            ipc::send(
                ctx.cmd,
                DaemonCommand::Device {
                    device_id: dev_id.to_string(),
                    op: DeviceOp::LightingDetectSegments {
                        channel_id: channel.id.clone(),
                    },
                },
            );
        }
    }
}

fn link_row(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    dev_id: &str,
    channel_id: &str,
    index: usize,
    link: &LightingSegmentInfo,
) {
    widgets::inset(ui, egui::Margin::symmetric(12, 10), |ui| {
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = theme::SPACE_5;
            let (badge, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(badge, 5.0, theme::TILE_BG);
            p.text(
                badge.center(),
                Align2::CENTER_CENTER,
                index.to_string(),
                theme::caption(),
                theme::TEXT_DIM,
            );

            let trailing_w = if link.locked { 80.0 } else { 30.0 };
            ui.allocate_ui_with_layout(
                Vec2::new((ui.available_width() - trailing_w).max(40.0), 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    name_field(ui, ctx, link);
                    ui.label(
                        egui::RichText::new(format!(
                            "{} · {}",
                            topology_label(&link.topology),
                            t!("device.chains_led_count", count = link.led_count)
                        ))
                        .font(theme::body_sm())
                        .color(theme::TEXT_MUT),
                    );
                },
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if link.locked {
                    widgets::tag(
                        ui,
                        &t!("device.chains_discovered"),
                        widgets::TagTone::Neutral,
                    );
                    return;
                }
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::hover());
                let id = egui::Id::unique(("ln_rm", &link.device_id));
                if widgets::glyph_button_at(ui, rect, id, widgets::paint_close_glyph).clicked() {
                    ipc::send(
                        ctx.cmd,
                        DaemonCommand::Device {
                            device_id: dev_id.to_string(),
                            op: DeviceOp::LightingRemoveSegment {
                                channel_id: channel_id.to_string(),
                                segment_id: link.device_id.clone(),
                            },
                        },
                    );
                }
            });
        });
    });
}

fn name_field(ui: &mut egui::Ui, ctx: &TabCtx, link: &LightingSegmentInfo) {
    if link.locked {
        ui.label(
            egui::RichText::new(&link.name)
                .font(theme::medium(14.0))
                .color(theme::TEXT),
        );
        return;
    }
    // Preserve the edit buffer while focused; re-seed from the daemon otherwise.
    let buf_key = egui::Id::unique(("ln_buf", &link.device_id));
    let focused_key = egui::Id::unique(("ln_focused", &link.device_id));
    let was_focused = ui
        .ctx()
        .data(|d| d.get_temp::<bool>(focused_key).unwrap_or(false));
    let mut buf: String = if was_focused {
        ui.ctx().data(|d| {
            d.get_temp::<String>(buf_key)
                .unwrap_or_else(|| link.name.clone())
        })
    } else {
        link.name.clone()
    };

    let resp = ui.add(
        egui::TextEdit::singleline(&mut buf)
            .font(theme::medium(14.0))
            .text_color(theme::TEXT)
            .frame(egui::Frame::NONE)
            .margin(egui::Margin::ZERO)
            .desired_width(ui.available_width()),
    );
    // Read before `data_mut`: `has_focus()` takes the context lock and would deadlock inside it.
    let has_focus = resp.has_focus();
    ui.ctx().data_mut(|d| {
        d.insert_temp(buf_key, buf.clone());
        d.insert_temp(focused_key, has_focus);
    });

    if resp.lost_focus() {
        let trimmed = buf.trim().to_string();
        if !trimmed.is_empty() && trimmed != link.name {
            ipc::send(
                ctx.cmd,
                DaemonCommand::Device {
                    device_id: link.device_id.clone(),
                    op: DeviceOp::SetDeviceName { name: trimmed },
                },
            );
        }
    }
}

fn add_link_panel(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    dev_id: &str,
    channel: &LightingChannel,
    remaining: u32,
) {
    let open_key = egui::Id::unique(("ch_add_open", dev_id, &channel.id));
    let name_key = egui::Id::unique(("ch_add_name", dev_id, &channel.id));
    let topo_key = egui::Id::unique(("ch_add_topo", dev_id, &channel.id));
    let leds_key = egui::Id::unique(("ch_add_leds", dev_id, &channel.id));

    let open = ui
        .ctx()
        .data(|d| d.get_temp::<bool>(open_key).unwrap_or(false));

    if !open {
        if remaining == 0 {
            return;
        }
        let resp = widgets::button(
            ui,
            &t!("device.chains_add_link"),
            widgets::ButtonKind::Secondary,
            Vec2::new(ui.available_width(), 30.0),
        );
        tour::anchor(ui.ctx(), AnchorId::ChainsAddLink, resp.rect);
        if resp.clicked() {
            ui.ctx().data_mut(|d| {
                d.insert_temp(open_key, true);
                d.insert_temp(name_key, t!("device.chains_new_link").to_string());
                d.insert_temp(topo_key, 0usize);
                d.insert_temp(leds_key, ADD_LEDS_DEFAULT.min(remaining).to_string());
            });
        }
        return;
    }

    let mut name = ui
        .ctx()
        .data(|d| d.get_temp::<String>(name_key).unwrap_or_default());
    let mut topo_idx = ui
        .ctx()
        .data(|d| d.get_temp::<usize>(topo_key).unwrap_or(0));
    let mut leds = ui
        .ctx()
        .data(|d| d.get_temp::<String>(leds_key).unwrap_or_default());

    egui::Frame::NONE
        .fill(theme::CHECKED_BG)
        .stroke(Stroke::new(1.0, theme::SELECTED_BORDER))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = theme::SPACE_4;
            form_field(ui, &t!("device.chains_name"), |ui| {
                widgets::text_field(ui, &mut name, "", ui.available_width());
            });
            ui.columns(2, |cols| {
                form_field(&mut cols[0], &t!("device.chains_type"), |ui| {
                    let options: Vec<(String, String)> = TOPOLOGY_CHOICES
                        .iter()
                        .enumerate()
                        .map(|(i, (ctor, _))| (i.to_string(), topology_label(&ctor())))
                        .collect();
                    if let Some(i) = widgets::combo_picker_full(
                        ui,
                        ("ch_topo_cb", dev_id, &channel.id),
                        &options,
                        &topo_idx.to_string(),
                        None,
                    )
                    .and_then(|v| v.parse().ok())
                    {
                        topo_idx = i;
                        let d = divisor_for_idx(i);
                        if let Ok(n) = leds.trim().parse::<u32>() {
                            leds = n.max(d).div_ceil(d).saturating_mul(d).to_string();
                        }
                    }
                });
                form_field(&mut cols[1], &t!("device.chains_leds"), |ui| {
                    widgets::text_field(ui, &mut leds, "", ui.available_width());
                });
            });
            let parsed = parse_leds(&leds, divisor_for_idx(topo_idx), remaining);
            let can_add = !name.trim().is_empty() && parsed.is_some();
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), 0.0),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = theme::SPACE_3;
                    if widgets::button_enabled(
                        ui,
                        &t!("device.chains_add"),
                        widgets::ButtonKind::Primary,
                        Vec2::new(48.0, 30.0),
                        can_add,
                    ) {
                        if let Some(led_count) = parsed {
                            ipc::send(
                                ctx.cmd,
                                DaemonCommand::Device {
                                    device_id: dev_id.to_string(),
                                    op: DeviceOp::LightingAddSegment {
                                        channel_id: channel.id.clone(),
                                        name: name.trim().to_string(),
                                        led_count,
                                        topology: topology_from_idx(topo_idx),
                                    },
                                },
                            );
                        }
                        ui.ctx().data_mut(|d| d.insert_temp(open_key, false));
                    }
                    if widgets::button(
                        ui,
                        &t!("device.chains_cancel"),
                        widgets::ButtonKind::Ghost,
                        Vec2::new(60.0, 30.0),
                    )
                    .clicked()
                    {
                        ui.ctx().data_mut(|d| d.insert_temp(open_key, false));
                    }
                },
            );
        });

    ui.ctx().data_mut(|d| {
        d.insert_temp(name_key, name);
        d.insert_temp(topo_key, topo_idx);
        d.insert_temp(leds_key, leds);
    });
}

fn form_field(ui: &mut egui::Ui, label: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = theme::SPACE_2;
        ui.label(
            egui::RichText::new(label)
                .font(theme::body_sm())
                .color(theme::TEXT_DIM),
        );
        body(ui);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::KeyboardFormFactor;
    use halod_shared::types::KeyboardLayout;

    #[test]
    fn topology_labels_all_variants() {
        assert_eq!(topology_label(&ZoneTopology::Linear), "Linear");
        assert_eq!(topology_label(&ZoneTopology::Ring), "Ring");
        assert_eq!(
            topology_label(&ZoneTopology::Rings { count: 2 }),
            "Rings ×2"
        );
        assert_eq!(
            topology_label(&ZoneTopology::Rings { count: 3 }),
            "Rings ×3"
        );
        assert_eq!(
            topology_label(&ZoneTopology::Rings { count: 9 }),
            "Rings ×9"
        );
        assert_eq!(topology_label(&ZoneTopology::Grid), "Grid");
        assert_eq!(
            topology_label(&ZoneTopology::Keyboard {
                form_factor: KeyboardFormFactor::FullSize,
                layout: KeyboardLayout::US,
            }),
            "Keyboard"
        );
    }

    #[test]
    fn typed_led_count_must_fit_header_and_topology() {
        assert_eq!(parse_leds(" 30 ", 1, 120), Some(30));
        assert_eq!(parse_leds("30", 3, 120), Some(30));
        assert_eq!(parse_leds("31", 3, 120), None);
        assert_eq!(parse_leds("121", 1, 120), None);
        assert_eq!(parse_leds("0", 1, 120), None);
        assert_eq!(parse_leds("abc", 1, 120), None);
    }

    #[test]
    fn topology_round_trips_through_index() {
        for (i, (ctor, _)) in TOPOLOGY_CHOICES.iter().enumerate() {
            let t = ctor();
            let recovered = topology_from_idx(i);
            assert_eq!(
                topology_label(&t),
                topology_label(&recovered),
                "round-trip failed for index {i}"
            );
        }
    }

    fn hub_with_link() -> halod_shared::types::WireDevice {
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
                            segments: vec![LightingSegmentInfo {
                                device_id: "child1".into(),
                                channel_id: "lighting".into(),
                                name: "Strip".into(),
                                topology: ZoneTopology::Linear,
                                led_count: 30,
                                color_order: None,
                                locked: false,
                            }],
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

    fn dev_with_links(
        links: Vec<halod_shared::types::LightingSegmentInfo>,
    ) -> halod_shared::types::WireDevice {
        let mut d = hub_with_link();
        if let halod_shared::types::DeviceCapability::Lighting(r) = &mut d.capabilities[0] {
            if let LightingDivision::Divisible { segments, .. } =
                &mut r.descriptor.channels[0].division
            {
                *segments = links;
            }
        }
        d
    }

    /// Render `show` for the given device in a worker thread; panic if it does
    /// not finish within 5 s (a deadlock/hang reproduces the reported crash).
    fn render_with_watchdog(label: &str, dev: halod_shared::types::WireDevice) {
        use std::sync::mpsc;
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let state = TopicStore::default();
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 700.0),
                )),
                ..Default::default()
            };
            for _ in 0..3 {
                let _ = crate::ui::run_headless(&ctx, input.clone(), |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let tab = TabCtx {
                            state: &state,
                            dev: &dev,
                            cmd: &tx,
                            time: 0.0,
                            debug: None,
                            lcd_images: &[],
                            lcd_preview: None,
                            lcd_upload: None,
                            lcd_upload_terminal: None,
                            lcd_template: None,
                            lcd_editor_render: None,
                            led_colors: crate::ui::screens::device::empty_led_colors(),
                            write_rate_history: None,
                            plugin_assets: crate::ui::screens::device::empty_plugin_assets(),
                        };
                        show(ui, &tab);
                    });
                });
            }
            let _ = done_tx.send(());
        });
        match done_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(()) => {
                let _ = handle.join();
            }
            Err(_) => panic!("`{label}` hung (deadlock / infinite layout)"),
        }
    }

    #[test]
    fn render_empty_channel() {
        render_with_watchdog("empty", dev_with_links(vec![]));
    }

    #[test]
    fn render_locked_link() {
        use halod_shared::types::*;
        render_with_watchdog(
            "locked",
            dev_with_links(vec![LightingSegmentInfo {
                device_id: "c".into(),
                channel_id: "lighting".into(),
                name: "Locked".into(),
                topology: ZoneTopology::Linear,
                led_count: 30,
                color_order: None,
                locked: true,
            }]),
        );
    }

    /// Regression test for the inline-rename deadlock: an editable (user-added)
    /// link renders a `TextEdit` whose `has_focus()` was once read *inside* a
    /// `data_mut` closure, re-entering the egui context lock and freezing the UI
    /// thread the moment a non-locked link appeared (i.e. right after adding one).
    #[test]
    fn render_editable_link() {
        render_with_watchdog("editable", hub_with_link());
    }
}
