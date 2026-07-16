// SPDX-License-Identifier: GPL-3.0-or-later
//! LED chains tab — per-header chain configuration for ARGB hubs and LED-strip controllers.
//!
//! Shows one card per chainable channel. Each card has a header (badge, usage
//! bar, LED budget, Discover button), a list of link rows, and an inline
//! Add-link form. Only user-added links (non-locked) expose rename/remove
//! controls; hardware-detected links show a DISCOVERED chip and a lock icon.

use crate::ui::components as widgets;
use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{ChainLinkInfo, ChainableChannelInfo, DeviceCapability, ZoneTopology};

use super::TabCtx;
use crate::ui::theme;

/// Maximum LED dots shown inline before the "+N" overflow label.
const DOT_CAP: usize = 20;
/// Default LED count when the Add-link form opens.
const ADD_LEDS_DEFAULT: u32 = 30;

/// Topology choices in the Add-link form: (constructor, LED-count divisor).
/// Display text comes from `topology_label()`, keyed off the variant.
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

/// Short uppercase badge derived from a channel name: "Header 1" → "H1".
fn badge_label(name: &str) -> String {
    let letter: String = name
        .chars()
        .find(|c| c.is_alphabetic())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    let digit: String = name
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(1)
        .collect();
    if !letter.is_empty() && !digit.is_empty() {
        format!("{letter}{digit}")
    } else {
        name.chars()
            .filter(|c| c.is_alphanumeric())
            .take(2)
            .collect::<String>()
            .to_uppercase()
    }
}

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx) {
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::TabChains,
        ui.max_rect(),
    );
    // Borrow the chainable channels directly from the capability to avoid a
    // per-frame Vec clone. Find the first Rgb capability with non-empty channels.
    let channels: &[ChainableChannelInfo] = ctx
        .dev
        .capabilities
        .iter()
        .find_map(|c| match c {
            DeviceCapability::Rgb(r) if !r.chainable_channels.is_empty() => {
                Some(r.chainable_channels.as_slice())
            }
            _ => None,
        })
        .unwrap_or_default();

    if channels.is_empty() {
        return;
    }

    let total_leds: u32 = channels
        .iter()
        .flat_map(|ch| ch.links.iter())
        .map(|l| l.led_count)
        .sum();

    // Page heading
    {
        let (hdr, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::hover());
        let p = ui.painter();
        p.text(
            hdr.left_top() + Vec2::new(0.0, 1.0),
            Align2::LEFT_TOP,
            t!("device.chains_title"),
            theme::heading(),
            theme::TEXT,
        );
        p.text(
            Pos2::new(hdr.right(), hdr.top() + 2.0),
            Align2::RIGHT_TOP,
            t!(
                "device.chains_summary",
                leds = total_leds,
                headers = channels.len()
            ),
            theme::value_sm(),
            theme::TEXT_FAINT,
        );
        p.text(
            hdr.left_top() + Vec2::new(0.0, 21.0),
            Align2::LEFT_TOP,
            t!("device.chains_subtitle"),
            theme::body_md(),
            theme::TEXT_MUT,
        );
    }

    ui.add_space(theme::SPACE_8);

    let dev_id = ctx.dev.id.clone();
    for channel in channels {
        channel_card(ui, ctx, &dev_id, channel);
        ui.add_space(theme::SPACE_7);
    }
}

fn channel_card(ui: &mut egui::Ui, ctx: &TabCtx, dev_id: &str, channel: &ChainableChannelInfo) {
    let used: u32 = channel.links.iter().map(|l| l.led_count).sum();

    egui::Frame::NONE
        .fill(theme::CARD_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::RADIUS_XL)
        .inner_margin(egui::Margin {
            left: 18,
            right: 18,
            top: 16,
            bottom: 16,
        })
        .show(ui, |ui| {
            channel_header(ui, ctx, dev_id, channel, used);
            ui.add_space(theme::SPACE_6);

            let prev_spacing = ui.style().spacing.item_spacing;
            ui.style_mut().spacing.item_spacing = egui::vec2(0.0, 9.0);

            if channel.links.is_empty() {
                empty_placeholder(ui);
            } else {
                for (li, link) in channel.links.iter().enumerate() {
                    link_row(ui, ctx, dev_id, &channel.channel_id, li + 1, link);
                }
            }

            ui.style_mut().spacing.item_spacing = prev_spacing;

            let remaining = channel.max_leds.saturating_sub(used);
            if remaining > 0 {
                ui.add_space(theme::SPACE_4);
                add_link_panel(ui, ctx, dev_id, channel, remaining);
            }
        });
}

fn channel_header(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    dev_id: &str,
    channel: &ChainableChannelInfo,
    used: u32,
) {
    let flash_key = egui::Id::new(("ch_flash", dev_id, &channel.channel_id));
    let flash_at_key = egui::Id::new(("ch_flash_at", dev_id, &channel.channel_id));

    let flashing = ui
        .ctx()
        .data(|d| d.get_temp::<bool>(flash_key).unwrap_or(false));
    if flashing {
        let clear_at = ui
            .ctx()
            .data(|d| d.get_temp::<f64>(flash_at_key).unwrap_or(0.0));
        if ui.ctx().input(|i| i.time) >= clear_at {
            ui.ctx().data_mut(|d| {
                d.remove::<bool>(flash_key);
                d.remove::<f64>(flash_at_key);
            });
        } else {
            ui.ctx().request_repaint();
        }
    }

    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::hover());

    // Badge
    let badge = Rect::from_min_size(row.left_top(), Vec2::new(30.0, 30.0));
    {
        let p = ui.painter();
        p.rect_filled(badge, 8.0, theme::INNER_BG);
        p.rect_stroke(
            badge,
            8.0,
            Stroke::new(1.0, theme::BORDER),
            egui::StrokeKind::Middle,
        );
        p.text(
            badge.center(),
            Align2::CENTER_CENTER,
            badge_label(&channel.name),
            theme::mono_bold(11.0),
            theme::CYAN,
        );
    }

    // Discover button (right-aligned). Idle uses the shared ghost button; the
    // flashing state is a transient red active indicator, painted directly.
    let disc_rect = Rect::from_min_size(
        Pos2::new(row.right() - 100.0, row.top()),
        Vec2::new(100.0, 30.0),
    );
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::ChainsDiscover,
        disc_rect,
    );
    let disc_id = egui::Id::new(("ch_disc", dev_id, &channel.channel_id));
    let disc_resp = if flashing {
        let p = ui.painter();
        p.rect_filled(disc_rect, 8.0, theme::a(theme::hex(0xff2d2d), 0.18));
        p.rect_stroke(
            disc_rect,
            8.0,
            Stroke::new(1.0, theme::hex(0xff2d2d)),
            egui::StrokeKind::Middle,
        );
        p.text(
            disc_rect.center(),
            Align2::CENTER_CENTER,
            t!("device.chains_flashing"),
            theme::body_sm(),
            theme::TEXT,
        );
        ui.interact(disc_rect, disc_id, Sense::click())
    } else {
        widgets::button_at(
            ui,
            disc_rect,
            disc_id,
            &format!("◎ {}", t!("device.chains_discover")),
            widgets::ButtonKind::Ghost,
        )
    };
    if disc_resp.clicked() && !flashing {
        let now = ui.ctx().input(|i| i.time);
        ui.ctx().data_mut(|d| d.insert_temp(flash_key, true));
        ui.ctx()
            .data_mut(|d| d.insert_temp(flash_at_key, now + 2.4));
        crate::runtime::ipc::send(
            ctx.cmd,
            halod_shared::commands::DaemonCommand::RgbChainDetectChannel {
                id: dev_id.to_string(),
                channel_id: channel.channel_id.clone(),
            },
        );
    }

    let p = ui.painter();
    let usage = t!("device.chains_usage", used = used, max = channel.max_leds);
    let usage_galley = p.layout_no_wrap(usage.to_string(), theme::value_sm(), theme::TEXT_MUT);
    let usage_size = usage_galley.size();
    let usage_r = disc_rect.left() - 8.0;
    p.galley(
        Pos2::new(usage_r - usage_size.x, row.center().y - usage_size.y / 2.0),
        usage_galley,
        theme::TEXT_MUT,
    );

    // Usage bar
    let bar_left = badge.right() + 14.0;
    let bar_right = usage_r - usage_size.x - 8.0;
    let bar_cy = row.center().y;
    let bar = Rect::from_min_max(
        Pos2::new(bar_left, bar_cy - 3.0),
        Pos2::new(bar_right, bar_cy + 3.0),
    );
    if bar.width() > 0.0 {
        p.rect_filled(bar, 3.0, theme::INNER_BG);
        p.rect_stroke(
            bar,
            3.0,
            Stroke::new(1.0, theme::BORDER_INNER),
            egui::StrokeKind::Middle,
        );
        let pct = (used as f32 / channel.max_leds as f32).min(1.0);
        let bar_col = if pct > 0.85 {
            theme::STAT_AMBER
        } else {
            theme::CYAN
        };
        if pct > 0.0 {
            let fill = Rect::from_min_size(bar.min, Vec2::new(bar.width() * pct, bar.height()));
            p.rect_filled(fill, 3.0, bar_col);
        }
    }
}

fn empty_placeholder(ui: &mut egui::Ui) {
    egui::Frame::NONE
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::RADIUS_MD)
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(t!("device.chains_no_links"))
                        .font(theme::body_md())
                        .color(theme::TEXT_FAINT),
                );
            });
        });
}

fn link_row(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    dev_id: &str,
    channel_id: &str,
    index: usize,
    link: &ChainLinkInfo,
) {
    egui::Frame::NONE
        .fill(theme::INNER_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::RADIUS_MD)
        .inner_margin(egui::Margin {
            left: 13,
            right: 13,
            top: 11,
            bottom: 11,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // Index badge
                let (badge, _) = ui.allocate_exact_size(Vec2::new(22.0, 22.0), Sense::hover());
                let p = ui.painter();
                p.rect_filled(badge, 6.0, theme::CARD_BG);
                p.rect_stroke(
                    badge,
                    6.0,
                    Stroke::new(1.0, theme::BORDER),
                    egui::StrokeKind::Middle,
                );
                p.text(
                    badge.center(),
                    Align2::CENTER_CENTER,
                    index.to_string(),
                    theme::mono_bold(10.0),
                    theme::TEXT_FAINT,
                );

                ui.add_space(theme::SPACE_6);

                // Name + dots column
                ui.vertical(|ui| {
                    name_field(ui, ctx, link);
                    ui.add_space(theme::SPACE_4);
                    led_dots(ui, link);
                });

                // Right-side controls
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if link.locked {
                        locked_right(ui, link);
                    } else {
                        editable_right(ui, ctx, dev_id, channel_id, link);
                    }
                });
            });
        });
}

fn name_field(ui: &mut egui::Ui, ctx: &TabCtx, link: &ChainLinkInfo) {
    if link.locked {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(&link.name)
                    .font(theme::semibold(12.5))
                    .color(theme::TEXT),
            );
            ui.add_space(theme::SPACE_5);
            let (chip, _) = ui.allocate_exact_size(Vec2::new(94.0, 19.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(chip, 6.0, theme::hex(0x181d29));
            p.rect_stroke(
                chip,
                6.0,
                Stroke::new(1.0, theme::BORDER),
                egui::StrokeKind::Middle,
            );
            p.text(
                chip.center(),
                Align2::CENTER_CENTER,
                t!("device.chains_discovered"),
                theme::micro(),
                theme::TEXT_DIM,
            );
        });
    } else {
        // Inline rename: preserve edit buffer while focused, re-seed from daemon when not.
        let buf_key = egui::Id::new(("ln_buf", &link.child_device_id));
        let focused_key = egui::Id::new(("ln_focused", &link.child_device_id));
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
                .font(theme::semibold(12.5))
                .desired_width(180.0),
        );
        // `has_focus()` locks the egui context internally, so it must be read
        // *before* entering `data_mut` (which already holds that lock) — calling
        // it inside the closure re-enters the lock and deadlocks the UI thread.
        let has_focus = resp.has_focus();
        ui.ctx().data_mut(|d| {
            d.insert_temp(buf_key, buf.clone());
            d.insert_temp(focused_key, has_focus);
        });

        if resp.lost_focus() {
            let trimmed = buf.trim().to_string();
            if !trimmed.is_empty() && trimmed != link.name {
                crate::runtime::ipc::send(
                    ctx.cmd,
                    halod_shared::commands::DaemonCommand::SetDeviceName {
                        device_id: link.child_device_id.clone(),
                        name: trimmed,
                    },
                );
            }
        }
    }
}

fn led_dots(ui: &mut egui::Ui, link: &ChainLinkInfo) {
    let count = link.led_count as usize;
    let dot_count = count.min(DOT_CAP);
    let extra = count.saturating_sub(DOT_CAP);
    let dot_color = if link.locked {
        theme::hex(0x5d6679)
    } else {
        theme::CYAN
    };

    ui.horizontal(|ui| {
        ui.style_mut().spacing.item_spacing = egui::vec2(3.0, 0.0);
        for _ in 0..dot_count {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
            ui.painter().rect_filled(r, 1.5, dot_color);
        }
        if extra > 0 {
            ui.label(
                egui::RichText::new(format!("+{extra}"))
                    .font(theme::value_xs())
                    .color(theme::TEXT_FAINT),
            );
        }
    });
}

fn locked_right(ui: &mut egui::Ui, link: &ChainLinkInfo) {
    ui.label(
        egui::RichText::new("🔒")
            .font(theme::body_md())
            .color(theme::hex(0x3a4860)),
    );
    ui.add_space(theme::SPACE_4);
    ui.label(
        egui::RichText::new(t!("device.chains_led_count", count = link.led_count))
            .font(theme::mono(12.0))
            .color(theme::TEXT_MUT),
    );
    ui.add_space(theme::SPACE_4);
    ui.label(
        egui::RichText::new(topology_label(&link.topology))
            .font(theme::body_sm())
            .color(theme::TEXT_MUT),
    );
}

fn editable_right(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    dev_id: &str,
    channel_id: &str,
    link: &ChainLinkInfo,
) {
    // Remove button — use an explicit ID so egui can track hover/click state
    // across frames even when the number of link rows changes.
    let btn_id = egui::Id::new(("ln_rm", &link.child_device_id));
    let (btn, _) = ui.allocate_exact_size(Vec2::new(30.0, 30.0), Sense::hover());
    let btn_resp = ui.interact(btn, btn_id, Sense::click());
    if btn_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let t = ui
        .ctx()
        .animate_bool_with_time(btn_id, btn_resp.hovered(), 0.12);
    let border_col = theme::lerp_color(theme::BORDER, theme::hex(0x3a2730), t);
    let icon_col = theme::lerp_color(theme::TEXT_MUT, theme::hex(0xef8b8d), t);
    let p = ui.painter();
    p.rect_filled(btn, 8.0, Color32::TRANSPARENT);
    p.rect_stroke(
        btn,
        8.0,
        Stroke::new(1.0, border_col),
        egui::StrokeKind::Middle,
    );
    p.text(
        btn.center(),
        Align2::CENTER_CENTER,
        "×",
        theme::body_lg(),
        icon_col,
    );
    if btn_resp.clicked() {
        crate::runtime::ipc::send(
            ctx.cmd,
            halod_shared::commands::DaemonCommand::RgbChainRemoveLink {
                id: dev_id.to_string(),
                channel_id: channel_id.to_string(),
                child_device_id: link.child_device_id.clone(),
            },
        );
    }

    ui.add_space(theme::SPACE_4);
    ui.label(
        egui::RichText::new(t!("device.chains_led_count", count = link.led_count))
            .font(theme::mono(12.0))
            .color(theme::TEXT_DIM),
    );
    ui.add_space(theme::SPACE_4);
    ui.label(
        egui::RichText::new(topology_label(&link.topology))
            .font(theme::body_sm())
            .color(theme::TEXT_MUT),
    );
}

fn add_link_panel(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    dev_id: &str,
    channel: &ChainableChannelInfo,
    remaining: u32,
) {
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::ChainsAddLink,
        Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), 34.0)),
    );
    let open_key = egui::Id::new(("ch_add_open", dev_id, &channel.channel_id));
    let name_key = egui::Id::new(("ch_add_name", dev_id, &channel.channel_id));
    let topo_key = egui::Id::new(("ch_add_topo", dev_id, &channel.channel_id));
    let leds_key = egui::Id::new(("ch_add_leds", dev_id, &channel.channel_id));

    let open = ui
        .ctx()
        .data(|d| d.get_temp::<bool>(open_key).unwrap_or(false));

    if !open {
        let label = format!("+ {}", t!("device.chains_add_link"));
        let btn_w = ui
            .painter()
            .layout_no_wrap(label.clone(), theme::body_sm(), theme::TEXT_DIM)
            .size()
            .x
            + 26.0;
        if widgets::button(
            ui,
            &label,
            widgets::ButtonKind::Ghost,
            Vec2::new(btn_w, 34.0),
        )
        .clicked()
        {
            ui.ctx().data_mut(|d| {
                d.insert_temp(open_key, true);
                d.insert_temp(name_key, t!("device.chains_new_link").to_string());
                d.insert_temp(topo_key, 0usize);
                d.insert_temp(leds_key, ADD_LEDS_DEFAULT.min(remaining));
            });
        }
    } else {
        let mut name = ui.ctx().data(|d| {
            d.get_temp::<String>(name_key)
                .unwrap_or_else(|| t!("device.chains_new_link").to_string())
        });
        let mut topo_idx = ui
            .ctx()
            .data(|d| d.get_temp::<usize>(topo_key).unwrap_or(0));
        let prev_divisor = divisor_for_idx(topo_idx);
        let mut leds = ui
            .ctx()
            .data(|d| d.get_temp::<u32>(leds_key).unwrap_or(ADD_LEDS_DEFAULT));

        egui::Frame::NONE
            .fill(theme::INNER_BG)
            .stroke(Stroke::new(1.0, theme::BORDER))
            .corner_radius(theme::RADIUS_MD)
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                // Name field
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(t!("device.chains_name"))
                            .font(theme::body_sm())
                            .color(theme::TEXT_MUT),
                    );
                    ui.add_space(theme::SPACE_4);
                    ui.add(
                        egui::TextEdit::singleline(&mut name)
                            .font(theme::semibold(12.5))
                            .desired_width(180.0)
                            .margin(egui::Margin::symmetric(10, 7)),
                    );
                });
                ui.add_space(theme::SPACE_5);

                // Topology picker
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(t!("device.chains_type"))
                            .font(theme::body_sm())
                            .color(theme::TEXT_MUT),
                    );
                    ui.add_space(theme::SPACE_4);
                    let current = topology_label(&topology_from_idx(topo_idx));
                    egui::ComboBox::from_id_salt(("ch_topo_cb", dev_id, &channel.channel_id))
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            for (i, (ctor, _)) in TOPOLOGY_CHOICES.iter().enumerate() {
                                ui.selectable_value(&mut topo_idx, i, topology_label(&ctor()));
                            }
                        });
                });

                // Snap LED count when topology divisor changed
                let new_divisor = divisor_for_idx(topo_idx);
                if new_divisor != prev_divisor && new_divisor > 0 {
                    leds = leds.max(new_divisor).div_ceil(new_divisor) * new_divisor;
                }

                ui.add_space(theme::SPACE_5);

                // LED count stepper
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(t!("device.chains_leds"))
                            .font(theme::body_sm())
                            .color(theme::TEXT_MUT),
                    );
                    ui.add_space(theme::SPACE_4);
                    led_stepper(
                        ui,
                        &mut leds,
                        new_divisor,
                        remaining,
                        dev_id,
                        &channel.channel_id,
                    );
                });

                ui.add_space(theme::SPACE_7);

                // Cancel (left) / Add (right)
                ui.horizontal(|ui| {
                    let can_add = !name.trim().is_empty() && leds > 0 && leds <= remaining;
                    if widgets::button(
                        ui,
                        &t!("device.chains_cancel"),
                        widgets::ButtonKind::Ghost,
                        Vec2::new(72.0, 32.0),
                    )
                    .clicked()
                    {
                        ui.ctx().data_mut(|d| d.insert_temp(open_key, false));
                    }
                    ui.add_space(theme::SPACE_4);
                    if widgets::button(
                        ui,
                        &t!("device.chains_add"),
                        widgets::ButtonKind::Primary,
                        Vec2::new(72.0, 32.0),
                    )
                    .clicked()
                        && can_add
                    {
                        crate::runtime::ipc::send(
                            ctx.cmd,
                            halod_shared::commands::DaemonCommand::RgbChainAddLink {
                                id: dev_id.to_string(),
                                channel_id: channel.channel_id.clone(),
                                name: name.trim().to_string(),
                                led_count: leds,
                                topology: topology_from_idx(topo_idx),
                            },
                        );
                        ui.ctx().data_mut(|d| d.insert_temp(open_key, false));
                    }
                });
            });

        // Persist form state
        ui.ctx().data_mut(|d| {
            d.insert_temp(name_key, name);
            d.insert_temp(topo_key, topo_idx);
            d.insert_temp(leds_key, leds);
        });
    }
}

/// − N + stepper drawn inline without egui layout allocation.
fn led_stepper(
    ui: &mut egui::Ui,
    leds: &mut u32,
    divisor: u32,
    max: u32,
    dev_id: &str,
    channel_id: &str,
) {
    let (frame, _) = ui.allocate_exact_size(Vec2::new(110.0, 30.0), Sense::hover());
    let p = ui.painter();
    p.rect_stroke(
        frame,
        8.0,
        Stroke::new(1.0, theme::hex(0x222b3a)),
        egui::StrokeKind::Middle,
    );

    let btn_w = 28.0;
    let dec_rect = Rect::from_min_size(frame.min, Vec2::new(btn_w, frame.height()));
    let inc_rect = Rect::from_min_size(
        Pos2::new(frame.right() - btn_w, frame.top()),
        Vec2::new(btn_w, frame.height()),
    );

    let dec_id = egui::Id::new(("ch_dec", dev_id, channel_id));
    let inc_id = egui::Id::new(("ch_inc", dev_id, channel_id));
    let dec_resp = ui.interact(dec_rect, dec_id, Sense::click());
    let inc_resp = ui.interact(inc_rect, inc_id, Sense::click());

    if dec_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if inc_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let t_dec = ui
        .ctx()
        .animate_bool_with_time(dec_id, dec_resp.hovered(), 0.1);
    let t_inc = ui
        .ctx()
        .animate_bool_with_time(inc_id, inc_resp.hovered(), 0.1);

    p.rect_filled(dec_rect, 8.0, theme::a(Color32::WHITE, 0.05 * t_dec));
    p.text(
        dec_rect.center(),
        Align2::CENTER_CENTER,
        "−",
        theme::body(15.0),
        theme::lerp_color(theme::TEXT_MUT, theme::TEXT, t_dec),
    );
    p.rect_filled(inc_rect, 8.0, theme::a(Color32::WHITE, 0.05 * t_inc));
    p.text(
        inc_rect.center(),
        Align2::CENTER_CENTER,
        "+",
        theme::body(15.0),
        theme::lerp_color(theme::TEXT_MUT, theme::TEXT, t_inc),
    );

    let center = Rect::from_min_max(
        Pos2::new(dec_rect.right(), frame.top()),
        Pos2::new(inc_rect.left(), frame.bottom()),
    );
    p.text(
        center.center(),
        Align2::CENTER_CENTER,
        leds.to_string(),
        theme::mono(12.0),
        theme::TEXT_BRIGHT,
    );

    if dec_resp.clicked() && *leds > divisor {
        *leds = leds.saturating_sub(divisor).max(divisor);
    }
    if inc_resp.clicked() {
        let next = *leds + divisor;
        if next <= max {
            *leds = next;
        }
    }
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
    fn badge_label_derives_short_names() {
        assert_eq!(badge_label("Header 1"), "H1");
        assert_eq!(badge_label("Channel 2"), "C2");
        assert_eq!(badge_label("A"), "A");
        assert_eq!(badge_label("Ch"), "CH");
    }

    #[test]
    fn divisor_snaps_led_count_up() {
        let div = divisor_for_idx(3); // Rings ×3
        assert_eq!(div, 3);
        let snapped = 10u32.div_ceil(div) * div;
        assert_eq!(snapped, 12);
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
                    links: vec![ChainLinkInfo {
                        child_device_id: "child1".into(),
                        name: "Strip".into(),
                        topology: ZoneTopology::Linear,
                        led_count: 30,
                        locked: false,
                    }],
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

    fn dev_with_links(
        links: Vec<halod_shared::types::ChainLinkInfo>,
    ) -> halod_shared::types::WireDevice {
        let mut d = hub_with_link();
        if let halod_shared::types::DeviceCapability::Rgb(r) = &mut d.capabilities[0] {
            r.chainable_channels[0].links = links;
        }
        d
    }

    /// Render `show` for the given device in a worker thread; panic if it does
    /// not finish within 5 s (a deadlock/hang reproduces the reported crash).
    fn render_with_watchdog(label: &str, dev: halod_shared::types::WireDevice) {
        use std::sync::mpsc;
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let state = halod_shared::types::AppState::default();
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
                let _ = ctx.run_ui(input.clone(), |ui| {
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
            dev_with_links(vec![ChainLinkInfo {
                child_device_id: "c".into(),
                name: "Locked".into(),
                topology: ZoneTopology::Linear,
                led_count: 30,
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
