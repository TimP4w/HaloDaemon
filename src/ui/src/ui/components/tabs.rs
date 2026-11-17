// SPDX-License-Identifier: GPL-3.0-or-later
//! The underline-style tab bar (device-page tabs).

use egui::{Pos2, Rect, Sense, Stroke, Vec2};

use crate::ui::theme;

/// Underline-style tab bar (the device-page tabs). Mutates `sel` on click.
/// Returns the bar's full rect (e.g. for a tour to anchor on).
pub fn tab_bar(ui: &mut egui::Ui, sel: &mut usize, tabs: &[&str]) -> Rect {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::hover());
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, theme::DIVIDER),
    );
    let mut x = rect.left();
    for (i, label) in tabs.iter().enumerate() {
        let active = i == *sel;
        let font = if active {
            theme::medium(14.0)
        } else {
            theme::body(14.0)
        };
        let galley = ui
            .painter()
            .layout_no_wrap(label.to_string(), font, theme::TEXT);
        let hit = Rect::from_min_size(
            Pos2::new(x, rect.top()),
            Vec2::new(galley.size().x + 2.0 * TAB_PAD, rect.height()),
        );
        let resp = ui.interact(hit, ui.scope_id().with(("tab", i)), Sense::click());
        let color = if active || resp.hovered() {
            theme::TEXT
        } else {
            theme::TEXT_DIM
        };
        ui.painter().galley(
            Pos2::new(x + TAB_PAD, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
        if active {
            ui.painter().line_segment(
                [
                    Pos2::new(hit.left(), rect.bottom() - 1.0),
                    Pos2::new(hit.right(), rect.bottom() - 1.0),
                ],
                Stroke::new(2.0, theme::CYAN),
            );
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            *sel = i;
        }
        x = hit.right() + 4.0;
    }
    rect
}

const TAB_PAD: f32 = 12.0;
