// SPDX-License-Identifier: GPL-3.0-or-later
//! Column-splitting and page-frame layout helpers.

/// Two top-down columns split at `left_w`, separated by `gap`. `egui::Ui::columns`
/// only divides the width *evenly*, so this exists for the asymmetric splits the
/// device/lighting designs call for. A single closure drives both columns (as
/// `egui`'s own `columns` does) so a body may mutably borrow shared state once.
pub fn split_columns<R>(
    ui: &mut egui::Ui,
    left_w: f32,
    gap: f32,
    body: impl FnOnce(&mut egui::Ui, &mut egui::Ui) -> R,
) -> R {
    let (left_w, right_w) = split_widths(ui.available_width(), gap, left_w);
    let top_left = ui.cursor().min;
    let bottom = ui.max_rect().bottom();
    let column = |x: f32, w: f32| {
        egui::UiBuilder::new()
            .max_rect(egui::Rect::from_min_max(
                egui::pos2(x, top_left.y),
                egui::pos2(x + w, bottom),
            ))
            .layout(egui::Layout::top_down(egui::Align::LEFT))
    };
    let mut left = ui.new_child(column(top_left.x, left_w));
    left.set_width(left_w);
    let mut right = ui.new_child(column(top_left.x + left_w + gap, right_w));
    right.set_width(right_w);
    let result = body(&mut left, &mut right);
    let max_h = left.min_size().y.max(right.min_size().y);
    ui.advance_cursor_after_rect(egui::Rect::from_min_size(
        top_left,
        egui::vec2(ui.available_width(), max_h),
    ));
    result
}

/// Column widths for [`split_columns`]: `left_w` clamped into the space left after
/// `gap`, the right column taking whatever remains.
fn split_widths(avail: f32, gap: f32, left_w: f32) -> (f32, f32) {
    let left = left_w.clamp(0.0, (avail - gap).max(0.0));
    (left, (avail - gap - left).max(0.0))
}

/// The standard page inner margin (`36 px` sides, `26/30 px` top/bottom) every
/// top-level page wraps its body in. Use instead of hand-rolling the `Frame`.
pub fn page_frame<R>(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 36,
            right: 36,
            top: 26,
            bottom: 30,
        })
        .show(ui, body)
        .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_widths_partitions_available_minus_gap() {
        // Left + gap + right always reconstruct the available width, and the
        // left column honours the requested width until it runs out of room.
        let (l, r) = split_widths(500.0, 18.0, 200.0);
        assert_eq!(l, 200.0);
        assert!((l + 18.0 + r - 500.0).abs() < 1e-4);
        // Oversized left is clamped so right never goes negative.
        let (l, r) = split_widths(100.0, 18.0, 300.0);
        assert_eq!(l, 82.0);
        assert_eq!(r, 0.0);
    }
}
