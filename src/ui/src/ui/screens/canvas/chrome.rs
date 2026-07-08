// SPDX-License-Identifier: GPL-3.0-or-later
//! Canvas header bar: FPS chip and transport controls.

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::commands::EngineKind;

use crate::runtime::ipc::CommandTx;
use crate::ui::theme::{self, a};

use super::CanvasUi;

/// The canvas transport cluster (play/pause/stop · FPS), right aligned.
/// Rendered in the shared RGB Lighting header, above the tab switcher, so it
/// stays visible on both the Effects Canvas and Direct Effects tabs.
pub(crate) fn chrome(ui: &mut egui::Ui, canvas_ui: &mut CanvasUi, cmd: &CommandTx, playing: bool) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if fps_chip(ui, canvas_ui) {
            canvas_ui.fps_modal_open = true;
        }
        ui.add_space(4.0);
        transport_controls(ui, cmd, playing);
    });
}

fn transport_controls(ui: &mut egui::Ui, cmd: &CommandTx, playing: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(104.0, 34.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 9.0, theme::CARD_BG);
    p.rect_stroke(
        rect,
        9.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Middle,
    );

    let inner = rect.shrink(4.0);
    let bw = inner.width() / 3.0;
    let active_flags = [playing, !playing, false];
    for (idx, active) in active_flags.into_iter().enumerate() {
        let br = Rect::from_min_size(
            Pos2::new(inner.left() + idx as f32 * bw, inner.top()),
            Vec2::new(bw, inner.height()),
        );
        let resp = ui.interact(br, egui::Id::new(("transport", idx)), Sense::click());
        let fill = if active {
            a(theme::CYAN, 0.16)
        } else if resp.hovered() {
            a(Color32::WHITE, 0.05)
        } else {
            Color32::TRANSPARENT
        };
        p.rect_filled(br, 6.0, fill);
        let col = if active { theme::CYAN } else { theme::TEXT_DIM };
        let ic = br.center();
        match idx {
            0 => {
                let pts = vec![
                    Pos2::new(ic.x - 4.0, ic.y - 5.0),
                    Pos2::new(ic.x + 5.0, ic.y),
                    Pos2::new(ic.x - 4.0, ic.y + 5.0),
                ];
                p.add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
            }
            1 => {
                let bh = 9.0_f32;
                p.rect_filled(
                    Rect::from_center_size(Pos2::new(ic.x - 3.0, ic.y), Vec2::new(3.0, bh)),
                    1.0,
                    col,
                );
                p.rect_filled(
                    Rect::from_center_size(Pos2::new(ic.x + 3.0, ic.y), Vec2::new(3.0, bh)),
                    1.0,
                    col,
                );
            }
            _ => {
                p.rect_filled(Rect::from_center_size(ic, Vec2::splat(9.0)), 1.0, col);
            }
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            match idx {
                0 => crate::domain::actions::system::set_engine_enabled(
                    cmd,
                    EngineKind::Canvas,
                    true,
                ),
                1 => crate::domain::actions::system::set_engine_enabled(
                    cmd,
                    EngineKind::Canvas,
                    false,
                ),
                _ => crate::domain::actions::canvas::stop(cmd),
            }
        }
    }
}

/// The FPS readout chip. Clicking it opens the FPS-adjust modal; returns whether
/// it was clicked.
fn fps_chip(ui: &mut egui::Ui, canvas_ui: &CanvasUi) -> bool {
    let fps_txt = canvas_ui.fps_label.as_str();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(86.0, 34.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    p.rect_filled(rect, 9.0, theme::CARD_BG);
    p.rect_stroke(
        rect,
        9.0,
        Stroke::new(
            1.0,
            if resp.hovered() {
                theme::CYAN
            } else {
                theme::BORDER
            },
        ),
        egui::StrokeKind::Middle,
    );
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        fps_txt,
        theme::mono(11.0),
        theme::CYAN,
    );
    resp.clicked()
}
