// SPDX-License-Identifier: GPL-3.0-or-later
//! Left sidebar: workspace nav, live device list, and the daemon-health footer.

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::AppState;

use crate::domain::models::device as model;
use crate::domain::state::Page;
use crate::ui::icons::{self, Icon};
use crate::ui::theme::{self, a};

const NAV: [(Icon, &str, Page); 5] = [
    (Icon::Home, "Home", Page::Home),
    (Icon::Lighting, "RGB Lighting", Page::Lighting),
    (Icon::Cooling, "Cooling", Page::Cooling),
    (Icon::Plugins, "Plugins", Page::Plugins),
    (Icon::Settings, "Settings", Page::Settings),
];

pub fn sidebar(ui: &mut egui::Ui, state: &AppState, connected: bool, page: &mut Page) {
    let rect = ui.max_rect();
    ui.painter().line_segment(
        [rect.right_top(), rect.right_bottom()],
        Stroke::new(1.0, theme::DIVIDER),
    );

    ui.add_space(14.0);
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.vertical(|ui| {
            ui.set_width(rect.width() - 24.0);
            section_label(ui, &t!("shell.workspace"));
            for (icon, _label, target) in &NAV {
                let row_start = ui.cursor().min;
                if nav_row(ui, *icon, &nav_label(target), *page == *target) {
                    *page = target.clone();
                }
                let row_rect =
                    Rect::from_min_size(row_start, Vec2::new(ui.available_width(), 38.0));
                let anchor = match target {
                    Page::Home => crate::domain::tour::AnchorId::HomeSidebarHome,
                    Page::Lighting => crate::domain::tour::AnchorId::HomeSidebarLighting,
                    Page::Cooling => crate::domain::tour::AnchorId::HomeSidebarCooling,
                    Page::Settings => crate::domain::tour::AnchorId::HomeSidebarSettings,
                    _ => continue,
                };
                crate::domain::tour::anchor(ui.ctx(), anchor, row_rect);
            }

            ui.add_space(10.0);
            section_label(ui, &t!("shell.my_devices"));
        });
        ui.add_space(12.0);
    });

    // Device list (scrolls, with side padding), then a pinned footer.
    let footer_h = 56.0;
    let list_rect = Rect::from_min_max(
        Pos2::new(rect.left() + 10.0, ui.cursor().top()),
        Pos2::new(rect.right() - 8.0, rect.bottom() - footer_h),
    );
    let mut list_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(list_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(&mut list_ui, |ui| {
            ui.add_space(2.0);
            let mut any = false;
            let mut devices: Vec<_> = state
                .devices
                .iter()
                .filter(|d| model::listable(d) && !model::is_hidden(d))
                .collect();
            devices.sort_by_key(|d| d.name.to_lowercase());
            for d in devices {
                any = true;
                let active = matches!(page, Page::Device(id) if *id == d.id);
                if device_row(ui, d, active) {
                    *page = Page::Device(d.id.clone());
                }
            }
            if !any {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.label(
                        egui::RichText::new(t!("shell.no_devices"))
                            .font(theme::body(12.0))
                            .color(theme::TEXT_FAINT),
                    );
                });
            }
        });

    footer(ui, rect, connected);
}

/// Sidebar footer: a daemon health hint. When connected it shows a green dot +
/// "Daemon running"; when offline it shows a red dot, "Daemon offline" and a
/// "Start daemon" button that spawns `halod`.
fn footer(ui: &mut egui::Ui, rect: Rect, connected: bool) {
    let y = rect.bottom() - 56.0;
    {
        let p = ui.painter();
        p.line_segment(
            [
                Pos2::new(rect.left() + 12.0, y),
                Pos2::new(rect.right() - 12.0, y),
            ],
            Stroke::new(1.0, theme::BORDER_SOFT),
        );

        let dot = Pos2::new(rect.left() + 20.0, y + 28.0);
        let (dot_col, title, sub, sub_col) = if connected {
            (
                theme::ONLINE,
                t!("shell.daemon_running"),
                t!("shell.daemon_running_sub"),
                theme::TEXT_FAINT,
            )
        } else {
            (
                theme::OFFLINE,
                t!("shell.daemon_offline"),
                t!("shell.daemon_offline_sub"),
                theme::OFFLINE_TEXT,
            )
        };
        if connected {
            theme::glow(p, dot, 6.0, dot_col, 0.7);
        }
        p.circle_filled(dot, 4.0, dot_col);
        p.text(
            Pos2::new(dot.x + 14.0, y + 21.0),
            Align2::LEFT_CENTER,
            title,
            theme::semibold(12.0),
            theme::TEXT,
        );
        p.text(
            Pos2::new(dot.x + 14.0, y + 35.0),
            Align2::LEFT_CENTER,
            sub,
            theme::body(10.0),
            sub_col,
        );
    }

    // Offline: offer a button to launch the daemon.
    if !connected {
        let btn_rect = Rect::from_min_size(
            Pos2::new(rect.right() - 12.0 - 70.0, y + 14.0),
            Vec2::new(70.0, 28.0),
        );
        let mut btn_ui = ui.new_child(egui::UiBuilder::new().max_rect(btn_rect).layout(
            egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
        ));
        if crate::ui::components::button(
            &mut btn_ui,
            &t!("shell.start"),
            crate::ui::components::ButtonKind::Primary,
            btn_rect.size(),
        )
        .clicked()
        {
            crate::domain::lifecycle::ensure_daemon_up();
        }
    }
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(8.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), Sense::hover());
    let mut job = egui::text::LayoutJob::default();
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: theme::body(10.0),
            color: theme::TEXT_FAINT2,
            extra_letter_spacing: 2.0,
            ..Default::default()
        },
    );
    let galley = ui.painter().layout_job(job);
    let pos = Pos2::new(rect.left() + 10.0, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(pos, galley, theme::TEXT_FAINT2);
}

/// Localized sidebar label for a workspace nav target.
fn nav_label(page: &Page) -> std::borrow::Cow<'static, str> {
    match page {
        Page::Lighting => t!("shell.nav_lighting"),
        Page::Cooling => t!("shell.nav_cooling"),
        Page::Plugins => t!("shell.nav_plugins"),
        Page::Settings => t!("shell.nav_settings"),
        _ => t!("shell.nav_home"),
    }
}

fn nav_row(ui: &mut egui::Ui, icon: Icon, label: &str, active: bool) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
    let hovered = resp.hovered();
    if active {
        ui.painter().rect_filled(rect, 9.0, theme::ROW_ACTIVE);
        // Subtle cyan accent bar on the left edge.
        let bar_h = 14.0;
        let bar = Rect::from_min_size(
            Pos2::new(rect.left(), rect.center().y - bar_h / 2.0),
            Vec2::new(2.5, bar_h),
        );
        ui.painter().rect_filled(bar, 1.5, theme::CYAN);
        theme::glow(ui.painter(), bar.center(), 5.0, theme::CYAN, 0.3);
    } else if hovered {
        ui.painter().rect_filled(rect, 9.0, a(Color32::WHITE, 0.03));
    }
    // Icons carry their own hue; dim inactive rows via opacity so the row still
    // reads active/inactive without desaturating the artwork.
    let icon_tint = if active || hovered {
        Color32::WHITE
    } else {
        Color32::from_white_alpha(150)
    };
    let text_color = if active { theme::TEXT } else { theme::TEXT_DIM };
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 22.0, rect.center().y),
        Vec2::splat(26.0),
    );
    icons::draw(ui, icon_rect, icon, icon_tint);
    let font = if active {
        theme::semibold(13.0)
    } else {
        theme::body(13.0)
    };
    ui.painter().text(
        Pos2::new(rect.left() + 44.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        font,
        text_color,
    );
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

fn device_row(ui: &mut egui::Ui, d: &halod_shared::types::WireDevice, active: bool) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
    let hovered = resp.hovered();
    if active {
        ui.painter().rect_filled(rect, 8.0, theme::ROW_ACTIVE);
        let bar_h = 12.0;
        let bar = Rect::from_min_size(
            Pos2::new(rect.left(), rect.center().y - bar_h / 2.0),
            Vec2::new(2.5, bar_h),
        );
        ui.painter().rect_filled(bar, 1.5, theme::CYAN);
        theme::glow(ui.painter(), bar.center(), 5.0, theme::CYAN, 0.28);
    } else if hovered {
        ui.painter().rect_filled(rect, 8.0, a(Color32::WHITE, 0.05));
        let bar_h = 10.0;
        let bar = Rect::from_min_size(
            Pos2::new(rect.left(), rect.center().y - bar_h / 2.0),
            Vec2::new(2.0, bar_h),
        );
        ui.painter()
            .rect_filled(bar, 1.0, a(theme::device_color(d), 0.45));
    }
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    // Code chip.
    let chip = Rect::from_min_size(
        Pos2::new(rect.left() + 8.0, rect.center().y - 12.0),
        Vec2::new(34.0, 24.0),
    );
    // The selected device gets a subtle thread of light weaving across the row.
    if active {
        use std::f32::consts::TAU;
        let time = ui.input(|i| i.time) as f32;
        let clip = ui.painter().with_clip_rect(rect);
        let color = theme::device_color(d);
        let cy = rect.center().y;
        let amp = rect.height() * 0.16;
        // A handful of soft radial blobs follow an invisible weaving path. Their
        // radial falloff means no edge or band shows — only a faint glow that
        // drifts as the path animates.
        const N: usize = 28;
        for i in 0..N {
            let fx = (i as f32 + 0.5) / N as f32;
            let y = cy
                + amp
                    * ((fx * TAU * 1.5 - time * 1.6).sin()
                        + 0.4 * (fx * TAU * 3.0 + time * 1.1).sin())
                    / 1.4;
            // Fade toward the row edges so the glow has no hard start/stop.
            let edge = (fx * std::f32::consts::PI).sin();
            theme::glow(
                &clip,
                Pos2::new(rect.left() + fx * rect.width(), y),
                rect.height() * 0.7,
                color,
                0.04 * edge,
            );
        }
        ui.ctx().request_repaint();
    }
    let p = ui.painter();
    crate::ui::components::device_badge(p, chip, d.device_type, theme::device_color(d), 6.0, 1.4);
    // Name (truncated by clip).
    let name_clip = Rect::from_min_max(
        Pos2::new(chip.right() + 10.0, rect.top()),
        Pos2::new(rect.right() - 8.0, rect.bottom()),
    );
    p.with_clip_rect(name_clip).text(
        Pos2::new(chip.right() + 10.0, rect.center().y),
        Align2::LEFT_CENTER,
        &d.name,
        theme::body(12.0),
        theme::TEXT,
    );
    resp.clicked()
}
