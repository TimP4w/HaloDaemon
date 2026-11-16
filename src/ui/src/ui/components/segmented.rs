// SPDX-License-Identifier: GPL-3.0-or-later
//! Segmented control, progress steps and the stat readout strip.

use egui::{Align2, Rect, Sense, Vec2};

use crate::ui::theme;

const SEGMENT_H: f32 = 28.0;
const SEGMENT_PAD: f32 = 3.0;
const SEGMENT_GAP: f32 = 2.0;
const SEGMENT_TEXT_PAD: f32 = 12.0;

/// Widths for each segment: natural widths, or equal shares of `full_width`.
fn segment_widths(natural: &[f32], full_width: Option<f32>) -> Vec<f32> {
    match full_width {
        Some(total) if !natural.is_empty() => {
            let inner = total - 2.0 * SEGMENT_PAD - SEGMENT_GAP * (natural.len() - 1) as f32;
            vec![(inner / natural.len() as f32).max(0.0); natural.len()]
        }
        _ => natural.to_vec(),
    }
}

/// A single-choice segmented control. `full` stretches it across the
/// available width with equal segments. Returns whether `selected` changed.
pub fn segmented(ui: &mut egui::Ui, options: &[&str], selected: &mut usize, full: bool) -> bool {
    segmented_styled(ui, options, selected, full, SegmentedStyle::default())
}

/// Track surface and size of a [`segmented_styled`] control.
#[derive(Clone, Copy, Default)]
pub struct SegmentedStyle {
    /// Card-coloured track for controls sitting on the page background.
    pub raised: bool,
    /// The taller 14 px variant used in page headers.
    pub md: bool,
}

/// [`segmented`] with an explicit [`SegmentedStyle`].
pub fn segmented_styled(
    ui: &mut egui::Ui,
    options: &[&str],
    selected: &mut usize,
    full: bool,
    style: SegmentedStyle,
) -> bool {
    let font = if style.md {
        theme::body_lg()
    } else {
        theme::body_sm()
    };
    let segment_h = if style.md { SEGMENT_H + 2.0 } else { SEGMENT_H };
    let track_bg = if style.raised {
        theme::CARD_BG
    } else {
        theme::INNER_BG
    };
    let natural: Vec<f32> = options
        .iter()
        .map(|label| {
            ui.painter()
                .layout_no_wrap((*label).to_owned(), font.clone(), theme::TEXT)
                .size()
                .x
                + 2.0 * SEGMENT_TEXT_PAD
        })
        .collect();
    let widths = segment_widths(&natural, full.then(|| ui.available_width()));
    let total_w = widths.iter().sum::<f32>()
        + 2.0 * SEGMENT_PAD
        + SEGMENT_GAP * widths.len().saturating_sub(1) as f32;
    let (track, track_resp) = ui.allocate_exact_size(
        Vec2::new(total_w, segment_h + 2.0 * SEGMENT_PAD),
        Sense::hover(),
    );
    theme::paint_surface(
        ui.painter(),
        track,
        theme::RADIUS_SM,
        track_bg,
        theme::BORDER,
    );
    let mut changed = false;
    let mut x = track.left() + SEGMENT_PAD;
    for (i, (label, w)) in options.iter().zip(&widths).enumerate() {
        let rect = Rect::from_min_size(
            egui::pos2(x, track.top() + SEGMENT_PAD),
            Vec2::new(*w, segment_h),
        );
        x += w + SEGMENT_GAP;
        let resp = ui.interact(rect, track_resp.id.with(i), Sense::click());
        let active = i == *selected;
        if active {
            ui.painter()
                .rect_filled(rect, theme::RADIUS_XS + 2.0, theme::ROW_ACTIVE);
        } else if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            *label,
            font.clone(),
            if active || resp.hovered() {
                theme::TEXT
            } else {
                theme::TEXT_DIM
            },
        );
        if resp.clicked() && !active {
            *selected = i;
            changed = true;
        }
    }
    changed
}

pub const OPTION_H: f32 = 36.0;
const OPTION_GAP: f32 = 6.0;

/// Tile rects for `n` options in `cols` equal columns across `width`.
fn option_rects(origin: egui::Pos2, width: f32, n: usize, cols: usize) -> Vec<Rect> {
    let cols = cols.clamp(1, n.max(1));
    let w = (width - OPTION_GAP * (cols - 1) as f32) / cols as f32;
    (0..n)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            Rect::from_min_size(
                origin
                    + Vec2::new(
                        col as f32 * (w + OPTION_GAP),
                        row as f32 * (OPTION_H + OPTION_GAP),
                    ),
                Vec2::new(w, OPTION_H),
            )
        })
        .collect()
}

/// A grid of mutually exclusive option tiles. Returns one response per option.
pub fn option_grid(
    ui: &mut egui::Ui,
    options: &[&str],
    selected: Option<usize>,
    cols: usize,
) -> Vec<egui::Response> {
    let width = ui.available_width();
    let rects = option_rects(ui.cursor().min, width, options.len(), cols);
    let bottom = rects.last().map_or(0.0, |r| r.bottom() - ui.cursor().min.y);
    let (_, grid) = ui.allocate_exact_size(Vec2::new(width, bottom), Sense::hover());
    options
        .iter()
        .zip(rects)
        .enumerate()
        .map(|(i, (label, rect))| {
            let resp = ui.interact(rect, grid.id.with(i), Sense::click());
            paint_option(ui, &resp, label, selected == Some(i), Align2::CENTER_CENTER);
            resp
        })
        .collect()
}

/// The option-tile look shared by grids, pills and pill rows.
pub(super) fn paint_option(
    ui: &egui::Ui,
    resp: &egui::Response,
    label: &str,
    active: bool,
    align: Align2,
) {
    let rect = resp.rect;
    let (bg, border, ink) = if active {
        (
            theme::SELECTED_BG,
            theme::SELECTED_BORDER,
            theme::SELECTED_TEXT,
        )
    } else if resp.hovered() {
        (theme::RAISED_BG, theme::BORDER_INNER, theme::TEXT)
    } else {
        (theme::INNER_BG, theme::TRACK, theme::TEXT_DIM)
    };
    theme::paint_surface(ui.painter(), rect, theme::RADIUS_SM, bg, border);
    let inner = rect.shrink2(Vec2::new(12.0, 0.0));
    ui.painter().with_clip_rect(rect.shrink(2.0)).text(
        align.pos_in_rect(&inner),
        align,
        label,
        theme::body_md(),
        ink,
    );
    if resp.hovered() && !active {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
}

/// [`option_grid`] over `(label, value)` pairs; returns the newly picked value.
pub fn option_grid_value<T: PartialEq + Clone>(
    ui: &mut egui::Ui,
    options: &[(impl AsRef<str>, T)],
    current: &T,
    cols: usize,
) -> Option<T> {
    let labels: Vec<&str> = options.iter().map(|(l, _)| l.as_ref()).collect();
    let selected = options.iter().position(|(_, v)| v == current);
    option_grid(ui, &labels, selected, cols)
        .iter()
        .position(|r| r.clicked())
        .filter(|&i| Some(i) != selected)
        .map(|i| options[i].1.clone())
}

/// A horizontal progress rail: one bar + label per step, filled through
/// `current` (zero-based).
pub fn progress_steps(ui: &mut egui::Ui, labels: &[String], current: usize) {
    if labels.is_empty() {
        return;
    }
    let gap = theme::SPACE_4;
    let w = ui.available_width();
    let step_w = (w - gap * (labels.len() - 1) as f32) / labels.len() as f32;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 26.0), Sense::hover());
    for (i, label) in labels.iter().enumerate() {
        let left = rect.left() + i as f32 * (step_w + gap);
        let reached = i <= current;
        let bar = Rect::from_min_size(egui::pos2(left, rect.top()), Vec2::new(step_w, 3.0));
        ui.painter()
            .rect_filled(bar, 2.0, if reached { theme::CYAN } else { theme::TRACK });
        ui.painter().text(
            egui::pos2(left, rect.top() + 11.0),
            Align2::LEFT_TOP,
            label,
            if i == current {
                theme::semibold(12.0)
            } else {
                theme::body_sm()
            },
            if reached {
                theme::TEXT
            } else {
                theme::TEXT_FAINT
            },
        );
    }
}

/// Labelled values side by side in one inset strip, painted into `rect`.
pub fn paint_stat_strip(p: &egui::Painter, rect: Rect, items: &[(&str, &str)]) {
    if items.is_empty() {
        return;
    }
    p.rect_filled(rect, theme::RADIUS_SM, theme::BORDER);
    for (cell, (key, value)) in stat_cells(rect, items.len()).into_iter().zip(items) {
        p.rect_filled(cell, theme::RADIUS_SM - 1.0, theme::INNER_BG);
        let clip = p.with_clip_rect(cell.shrink(1.0));
        clip.text(
            cell.left_top() + Vec2::new(10.0, 8.0),
            Align2::LEFT_TOP,
            *key,
            theme::caption(),
            theme::TEXT_MUT,
        );
        clip.text(
            cell.left_bottom() + Vec2::new(10.0, -8.0),
            Align2::LEFT_BOTTOM,
            *value,
            theme::medium(15.0),
            theme::TEXT,
        );
    }
}

/// Equal cells across `rect`, separated by 1 px rules.
fn stat_cells(rect: Rect, n: usize) -> Vec<Rect> {
    let w = (rect.width() - (n.saturating_sub(1)) as f32) / n.max(1) as f32;
    (0..n)
        .map(|i| {
            Rect::from_min_size(
                egui::pos2(rect.left() + i as f32 * (w + 1.0), rect.top()),
                Vec2::new(w, rect.height()),
            )
        })
        .collect()
}

pub const STAT_STRIP_H: f32 = 56.0;

/// Two labelled values side by side, laid out in the current flow.
pub fn stat_pair(ui: &mut egui::Ui, first: (&str, &str), second: (&str, &str)) {
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), STAT_STRIP_H),
        Sense::hover(),
    );
    paint_stat_strip(ui.painter(), rect, &[first, second]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn stat_cells_tile_the_strip(n in 1usize..6, w in 60.0f32..900.0) {
            let rect = Rect::from_min_size(egui::pos2(10.0, 0.0), Vec2::new(w, 56.0));
            let cells = stat_cells(rect, n);
            prop_assert_eq!(cells.len(), n);
            prop_assert!((cells[0].left() - rect.left()).abs() < 0.01);
            prop_assert!((cells[n - 1].right() - rect.right()).abs() < 0.01);
            for pair in cells.windows(2) {
                prop_assert!((pair[1].left() - pair[0].right() - 1.0).abs() < 0.01);
            }
        }
    }

    proptest! {
        #[test]
        fn option_tiles_fill_the_row_without_overlap(n in 1usize..14, cols in 1usize..5, w in 120.0f32..900.0) {
            let rects = option_rects(egui::pos2(0.0, 0.0), w, n, cols);
            prop_assert_eq!(rects.len(), n);
            for r in &rects {
                prop_assert!(r.left() >= -0.01 && r.right() <= w + 0.01);
            }
            for (i, a) in rects.iter().enumerate() {
                for b in &rects[i + 1..] {
                    prop_assert!(!a.shrink(0.01).intersects(b.shrink(0.01)));
                }
            }
        }
    }

    #[test]
    fn natural_widths_pass_through() {
        assert_eq!(segment_widths(&[40.0, 70.0], None), vec![40.0, 70.0]);
    }

    proptest! {
        #[test]
        fn full_segments_are_equal_and_fill_the_track(
            natural in proptest::collection::vec(1.0f32..200.0, 1..8),
            total in 200.0f32..1200.0,
        ) {
            let widths = segment_widths(&natural, Some(total));
            prop_assert_eq!(widths.len(), natural.len());
            prop_assert!(widths.windows(2).all(|w| w[0] == w[1]));
            let used = widths.iter().sum::<f32>()
                + 2.0 * SEGMENT_PAD
                + SEGMENT_GAP * (widths.len() - 1) as f32;
            prop_assert!((used - total).abs() < 0.01);
        }
    }
}
