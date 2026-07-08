// SPDX-License-Identifier: GPL-3.0-or-later
//! Draggable 2-D curve editor (fan/pump curves).

use std::ops::RangeInclusive;

use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};

use crate::ui::theme::{self, a};

/// An editable monotonic curve over a normalized domain. `points` are
/// `[x, y]` in data units; `x_range`/`y_range` map them into `rect`. Click on
/// empty space adds a point, drag a handle to move it, right-click removes.
/// Returns `true` when the point set changed.
pub fn curve_editor(
    ui: &mut egui::Ui,
    rect: Rect,
    points: &mut Vec<[f32; 2]>,
    x_range: RangeInclusive<f32>,
    y_range: RangeInclusive<f32>,
    color: Color32,
    op: Option<[f32; 2]>,
) -> bool {
    let (xlo, xhi) = (*x_range.start(), *x_range.end());
    let (ylo, yhi) = (*y_range.start(), *y_range.end());
    let to_screen = |pt: [f32; 2]| -> Pos2 {
        let tx = ((pt[0] - xlo) / (xhi - xlo)).clamp(0.0, 1.0);
        let ty = ((pt[1] - ylo) / (yhi - ylo)).clamp(0.0, 1.0);
        Pos2::new(
            rect.left() + tx * rect.width(),
            rect.bottom() - ty * rect.height(),
        )
    };
    let to_data = |pos: Pos2| -> [f32; 2] {
        let tx = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let ty = ((rect.bottom() - pos.y) / rect.height()).clamp(0.0, 1.0);
        [xlo + tx * (xhi - xlo), ylo + ty * (yhi - ylo)]
    };

    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 8.0, theme::INNER_BG);
    p.rect_stroke(
        rect,
        8.0,
        Stroke::new(1.0, theme::BORDER_INNER),
        egui::StrokeKind::Middle,
    );
    for i in 1..4 {
        let y = rect.top() + rect.height() * i as f32 / 4.0;
        p.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(1.0, theme::hex(0x161d29)),
        );
        let x = rect.left() + rect.width() * i as f32 / 4.0;
        p.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.0, theme::hex(0x161d29)),
        );
    }

    let mut changed = false;
    let id = ui.id().with("curve");
    let screen_pts: Vec<Pos2> = points.iter().map(|&pt| to_screen(pt)).collect();

    // Detect right-click via raw input so the bg widget (registered last)
    // can't steal the event from a handle via egui's overlap-priority rules.
    let right_click_pos: Option<Pos2> = ui.input(|i| {
        if i.pointer.secondary_clicked() {
            i.pointer.interact_pos()
        } else {
            None
        }
    });
    let mut remove: Option<usize> = None;
    if let Some(rpos) = right_click_pos {
        if rect.contains(rpos) && points.len() > 2 {
            // Find the nearest handle within the 18px hit zone.
            remove = screen_pts
                .iter()
                .enumerate()
                .find(|(_, &sp)| Rect::from_center_size(sp, Vec2::splat(18.0)).contains(rpos))
                .map(|(i, _)| i);
        }
    }

    for i in 0..points.len() {
        let hit = Rect::from_center_size(screen_pts[i], Vec2::splat(18.0));
        let resp = ui.interact(hit, id.with(i), Sense::drag());
        if resp.dragged() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let mut d = to_data(pos);
                let xmin = if i > 0 { points[i - 1][0] + 1.0 } else { xlo };
                let xmax = if i + 1 < points.len() {
                    points[i + 1][0] - 1.0
                } else {
                    xhi
                };
                d[0] = d[0].clamp(xmin.min(xmax), xmax.max(xmin));
                points[i] = d;
                changed = true;
            }
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
    }
    if let Some(i) = remove {
        points.remove(i);
        changed = true;
    }
    let bg = ui.interact(rect, id.with("bg"), Sense::click());
    if bg.clicked() {
        if let Some(pos) = bg.interact_pointer_pos() {
            // Only add a point on left-click outside any handle.
            let over_handle = screen_pts
                .iter()
                .any(|&sp| Rect::from_center_size(sp, Vec2::splat(18.0)).contains(pos));
            if !over_handle {
                points.push(to_data(pos));
                points.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));
                changed = true;
            }
        }
    }

    let handle_pts: Vec<Pos2> = points.iter().map(|&pt| to_screen(pt)).collect();
    // The drawn line/fill extends flat to both graph edges (held at the first
    // and last point's height), matching the GTK fan-curve.
    let mut pts = handle_pts.clone();
    if let (Some(&first), Some(&last)) = (handle_pts.first(), handle_pts.last()) {
        if first.x > rect.left() {
            pts.insert(0, Pos2::new(rect.left(), first.y));
        }
        if last.x < rect.right() {
            pts.push(Pos2::new(rect.right(), last.y));
        }
    }
    if pts.len() >= 2 {
        let mut mesh = egui::Mesh::default();
        let top_fill = a(color, 0.28);
        let bot_fill = a(color, 0.0);
        for w in pts.windows(2) {
            let base = mesh.vertices.len() as u32;
            for (v, c) in [
                (w[0], top_fill),
                (w[1], top_fill),
                (Pos2::new(w[1].x, rect.bottom()), bot_fill),
                (Pos2::new(w[0].x, rect.bottom()), bot_fill),
            ] {
                mesh.colored_vertex(v, c);
            }
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base, base + 2, base + 3);
        }
        p.add(egui::Shape::mesh(mesh));
        p.add(egui::Shape::line(pts.clone(), Stroke::new(2.5, color)));
    }
    if let Some(op) = op {
        let s = to_screen(op);
        p.line_segment(
            [Pos2::new(s.x, rect.top()), Pos2::new(s.x, rect.bottom())],
            Stroke::new(1.0, a(color, 0.5)),
        );
        theme::glow(&p, s, 8.0, color, 0.6);
        p.circle_filled(s, 4.5, Color32::WHITE);
    }
    for &s in &handle_pts {
        p.circle_filled(s, 8.0, color);
        p.circle_stroke(s, 8.0, Stroke::new(2.5, theme::INNER_BG));
    }
    changed
}
