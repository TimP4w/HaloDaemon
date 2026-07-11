// SPDX-License-Identifier: GPL-3.0-or-later
//! The discovery radar overlay: concentric rings, a rotating sweep, a pulsing
//! center and one animated blip per device the daemon has discovered so far.
//! Shown until the user enters the workspace.

use std::hash::{Hash, Hasher};

use egui::{epaint::Vertex, Align2, Color32, Mesh, Pos2, Rect, Sense, Shape, Stroke, Vec2};
use halod_shared::types::{AppState, DiscoveryPhase, WireDevice};

use crate::domain::models::device as model;
use crate::ui::theme::{self, a};

/// Returns true when the overlay should be dismissed
pub fn show(ui: &mut egui::Ui, state: &AppState, connected: bool, time: f64) -> bool {
    let mut dismissed = false;
    let ctx = ui.ctx().clone();
    // Border on top of the radar screen (same as main app).
    ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("win_border"),
    ))
    .rect_stroke(
        ctx.content_rect(),
        12.0,
        egui::Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Middle,
    );

    egui::Panel::top("radar_title")
        .exact_size(46.0)
        .frame(egui::Frame::NONE)
        .show(ui, |ui| {
            ui.painter().rect_filled(
                ui.max_rect(),
                egui::CornerRadius {
                    nw: 12,
                    ne: 12,
                    sw: 0,
                    se: 0,
                },
                theme::TITLE_BG,
            );
            crate::ui::shell::title_bar_plain(ui, state);
        });

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(ui, |ui| {
            let full = ui.max_rect();

            let drag = ui.interact(full, egui::Id::new("radar_drag"), Sense::click_and_drag());
            if drag.drag_started() {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                crate::ui::shell::arm_pointer_release_workaround(&ctx);
            }

            ui.painter().rect_filled(
                full,
                egui::CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 12,
                    se: 12,
                },
                theme::hex(0x070a0f),
            );
            let p = ui.painter().clone();
            let complete = matches!(state.discovery.phase, DiscoveryPhase::Complete);
            let found = state.devices.iter().filter(|d| model::listable(d)).count();

            // Brand wordmark, top-center of the radar body.
            let halo = p.layout_no_wrap("halo".to_string(), theme::bold(22.0), theme::TEXT);
            let daemon = p.layout_no_wrap(
                "daemon".to_string(),
                theme::bold(22.0),
                theme::hex(0x9b7fe0),
            );
            let icon_size = 48.0_f32;
            let total = icon_size + 14.0 + halo.size().x + daemon.size().x;
            let mark = Rect::from_min_size(
                Pos2::new(full.center().x - total / 2.0, full.top() + 20.0),
                Vec2::splat(icon_size),
            );
            theme::logo_icon(&p, &ctx, mark, time as f32);
            let halo_pos = Pos2::new(mark.right() + 14.0, mark.center().y - halo.size().y / 2.0);
            p.galley(halo_pos, halo.clone(), theme::TEXT);
            p.galley(
                Pos2::new(halo_pos.x + halo.size().x, halo_pos.y),
                daemon,
                theme::hex(0x9b7fe0),
            );

            let center = Pos2::new(full.center().x, full.top() + full.height() * 0.42);
            let radius = (full.height() * 0.45).min(full.width() * 0.34).min(210.0);
            draw_radar(&p, center, radius, time);

            for (i, d) in state
                .devices
                .iter()
                .filter(|d| model::listable(d))
                .enumerate()
            {
                draw_blip(&p, center, radius, d, i, time);
            }

            // Title + subtitle. When the daemon is down there is nothing to scan,
            // so prompt the user to start it instead.
            let title_y = center.y + radius + if connected { 34.0 } else { 78.0 };
            let (title, sub) = if !connected {
                (
                    t!("misc.radar_daemon_down").to_string(),
                    t!("misc.radar_daemon_down_sub").to_string(),
                )
            } else if complete {
                let title = if found == 1 {
                    t!("misc.radar_one_device_ready").to_string()
                } else {
                    t!("misc.radar_devices_ready", found = found).to_string()
                };
                (title, t!("misc.radar_all_connected").to_string())
            } else {
                (
                    t!("misc.radar_scanning").to_string(),
                    scan_subtitle(found, &state.discovery.detail),
                )
            };
            p.text(
                Pos2::new(center.x, title_y),
                Align2::CENTER_CENTER,
                title,
                theme::semibold(19.0),
                theme::TEXT,
            );
            p.text(
                Pos2::new(center.x, title_y + 24.0),
                Align2::CENTER_CENTER,
                sub,
                theme::mono(12.5),
                theme::TEXT_MUT,
            );

            if connected && complete {
                dismissed = true;
            }
        });
    dismissed
}

fn draw_radar(p: &egui::Painter, center: Pos2, radius: f32, time: f64) {
    // Concentric rings (ratios + alphas from the design).
    for (ratio, alpha) in [(1.0, 0.16), (0.686, 0.13), (0.371, 0.11), (0.114, 0.22)] {
        p.circle_stroke(
            center,
            radius * ratio,
            Stroke::new(1.0, a(theme::CYAN, alpha)),
        );
    }
    // Crosshairs.
    let ch = a(theme::CYAN, 0.10);
    p.line_segment(
        [
            Pos2::new(center.x, center.y - radius),
            Pos2::new(center.x, center.y + radius),
        ],
        Stroke::new(1.0, ch),
    );
    p.line_segment(
        [
            Pos2::new(center.x - radius, center.y),
            Pos2::new(center.x + radius, center.y),
        ],
        Stroke::new(1.0, ch),
    );

    let omega = std::f32::consts::TAU / 2.4; // one revolution / 2.4s
    let lead = (time as f32) * omega;
    draw_sweep(p, center, radius, lead, 60f32.to_radians());

    // Center pulse + expanding ping.
    theme::glow(p, center, 18.0, theme::CYAN, 0.8);
    p.circle_filled(center, 7.0, theme::CYAN);
    let t = (time / 2.4).fract() as f32;
    ping(p, center, 20.0, theme::CYAN, t);
}

/// A triangle-fan sector whose vertex alpha fades from `lead` backwards.
fn draw_sweep(p: &egui::Painter, center: Pos2, radius: f32, lead: f32, span: f32) {
    const SEG: usize = 24;
    let mut mesh = Mesh::default();
    let c_idx = mesh.vertices.len() as u32;
    mesh.vertices.push(Vertex {
        pos: center,
        uv: Pos2::ZERO,
        color: a(theme::CYAN, 0.42),
    });
    for i in 0..=SEG {
        let f = i as f32 / SEG as f32;
        let ang = lead - f * span;
        let alpha = 0.42 * (1.0 - f);
        mesh.vertices.push(Vertex {
            pos: center + Vec2::angled(ang) * radius,
            uv: Pos2::ZERO,
            color: a(theme::CYAN, alpha),
        });
        if i > 0 {
            let n = mesh.vertices.len() as u32;
            mesh.add_triangle(c_idx, n - 2, n - 1);
        }
    }
    p.add(Shape::mesh(mesh));
}

fn draw_blip(
    p: &egui::Painter,
    center: Pos2,
    radius: f32,
    d: &WireDevice,
    index: usize,
    time: f64,
) {
    let pos = blip_pos(center, radius, &d.id);
    let color = theme::device_color(d);
    let phase = (index as f64) * 0.2;
    let t = ((time * (1.0 / 1.6)) + phase).fract() as f32;
    ping(p, pos, 13.0, color, t);
    theme::glow(p, pos, 10.0, color, 0.9);
    p.circle_filled(pos, 4.5, color);
    p.text(
        Pos2::new(pos.x, pos.y + 16.0),
        Align2::CENTER_CENTER,
        model::code(d),
        theme::mono(8.5),
        color,
    );
}

fn ping(p: &egui::Painter, center: Pos2, base_r: f32, color: Color32, t: f32) {
    let scale = 0.6 + t * 1.8;
    let alpha = 0.7 * (1.0 - t);
    p.circle_stroke(center, base_r * scale, Stroke::new(1.0, a(color, alpha)));
}

/// Angle 0 = up, like the design's `sin/-cos` layout.
fn blip_pos(center: Pos2, radius: f32, id: &str) -> Pos2 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    let seed = h.finish();
    let ang = (seed & 0xFFFF) as f32 / 65536.0 * std::f32::consts::TAU;
    let r = radius * (0.44 + ((seed >> 16) & 0xFFFF) as f32 / 65536.0 * 0.50);
    Pos2::new(center.x + ang.sin() * r, center.y - ang.cos() * r)
}

/// Subtitle shown while scanning: the live step the daemon is on (e.g. which
/// transport), falling back to a generic label before the first detail arrives.
fn scan_subtitle(found: usize, detail: &str) -> String {
    if detail.is_empty() {
        t!("misc.radar_scan_fallback", found = found).to_string()
    } else {
        t!("misc.radar_scan_detail", found = found, detail = detail).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blip_pos_is_deterministic_for_an_id() {
        let center = Pos2::new(200.0, 150.0);
        let radius = 180.0;
        let a = blip_pos(center, radius, "dev-abc");
        let b = blip_pos(center, radius, "dev-abc");
        assert_eq!(a, b);
    }

    #[test]
    fn blip_pos_stays_within_the_radar_radius() {
        let center = Pos2::new(200.0, 150.0);
        let radius = 180.0;
        for id in ["a", "kbd-01", "mouse", "fan-hub-xyz", "", "💡"] {
            let p = blip_pos(center, radius, id);
            assert!(p.distance(center) <= radius + 1e-3, "{id} escaped radius");
        }
    }

    #[test]
    fn scan_subtitle_shows_daemon_detail_when_present() {
        assert_eq!(scan_subtitle(2, "SMBus"), "2 found · Scanning SMBus");
    }

    #[test]
    fn scan_subtitle_falls_back_when_detail_empty() {
        assert_eq!(scan_subtitle(0, ""), "0 found · scanning…");
    }

    #[test]
    fn blip_pos_differs_between_ids() {
        let center = Pos2::new(200.0, 150.0);
        let radius = 180.0;
        assert_ne!(
            blip_pos(center, radius, "one"),
            blip_pos(center, radius, "two")
        );
    }
}
