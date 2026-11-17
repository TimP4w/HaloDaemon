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

/// Widest a page body grows before it centres in the window.
pub const PAGE_MAX_W: f32 = 1360.0;

/// The standard page body: `24/28 px` padding, capped at [`PAGE_MAX_W`] and
/// centred. Every top-level page wraps its body in this.
pub fn page_frame<R>(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let avail = ui.available_rect_before_wrap();
    let rect = centered_column(avail, PAGE_MAX_W);
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 28,
                right: 28,
                top: 24,
                bottom: 32,
            })
            .show(ui, body)
            .inner
    })
    .inner
}

/// `avail` narrowed to at most `max_w`, centred horizontally.
fn centered_column(avail: egui::Rect, max_w: f32) -> egui::Rect {
    let w = avail.width().min(max_w);
    egui::Rect::from_min_size(
        egui::pos2(avail.center().x - w / 2.0, avail.top()),
        egui::vec2(w, avail.height()),
    )
}

/// The page title row: title with an inline muted summary on the left and
/// `actions` laid right-to-left on the right.
pub fn page_title(
    ui: &mut egui::Ui,
    title: &str,
    summary: &str,
    actions: impl FnOnce(&mut egui::Ui),
) -> egui::Rect {
    use crate::ui::theme;
    page_title_with(
        ui,
        title,
        |ui| {
            ui.label(
                egui::RichText::new(summary)
                    .font(theme::body_md())
                    .color(theme::TEXT_DIM),
            );
        },
        actions,
    )
}

/// [`page_title`] with custom content (e.g. a view switcher) after the title.
pub fn page_title_with(
    ui: &mut egui::Ui,
    title: &str,
    after_title: impl FnOnce(&mut egui::Ui),
    actions: impl FnOnce(&mut egui::Ui),
) -> egui::Rect {
    use crate::ui::theme;
    let mut title_rect = egui::Rect::NOTHING;
    egui::Sides::new().height(36.0).show(
        ui,
        |ui| {
            title_rect = ui
                .label(
                    egui::RichText::new(title)
                        .font(theme::display())
                        .color(theme::TEXT),
                )
                .rect;
            ui.add_space(theme::SPACE_6);
            after_title(ui);
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = theme::SPACE_5;
            actions(ui);
        },
    );
    ui.add_space(theme::SPACE_8);
    title_rect
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_column_is_capped_and_centred() {
        let avail = egui::Rect::from_min_size(egui::pos2(0.0, 10.0), egui::vec2(2000.0, 500.0));
        let col = centered_column(avail, PAGE_MAX_W);
        assert_eq!(col.width(), PAGE_MAX_W);
        assert!((col.center().x - avail.center().x).abs() < 0.01);
        let narrow = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 500.0));
        assert_eq!(centered_column(narrow, PAGE_MAX_W), narrow);
    }

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
