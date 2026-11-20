// SPDX-License-Identifier: GPL-3.0-or-later
//! A checkbox row with tag chips and a chevron that reveals a detail list.

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};

use super::{paint_checkbox, paint_tag, TagTone};
use crate::ui::theme;

const PAD: Vec2 = Vec2::new(14.0, 12.0);
const BOX: f32 = 16.0;
const CHEVRON: f32 = 32.0;
const CHIP_H: f32 = 24.0;
const CHIP_GAP: f32 = 6.0;
const DETAIL_GAP: f32 = 10.0;

pub struct ExpandRow<'a> {
    pub label: &'a str,
    pub chips: &'a [(String, TagTone)],
    pub meta: Option<(&'a str, Color32)>,
    pub details: &'a [(String, String)],
    pub empty_details: &'a str,
    pub expand_hint: &'a str,
    pub checked: bool,
    pub expanded: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExpandRowResponse {
    pub toggled: bool,
    pub expand_clicked: bool,
}

fn header_zones(header: Rect) -> (Rect, Rect) {
    let chevron = Rect::from_center_size(
        Pos2::new(header.right() - 10.0 - CHEVRON / 2.0, header.center().y),
        Vec2::splat(CHEVRON),
    );
    let check = Rect::from_min_max(header.min, Pos2::new(chevron.left(), header.max.y));
    (check, chevron)
}

pub fn expand_row(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    row: &ExpandRow,
) -> ExpandRowResponse {
    let width = ui.available_width();
    let p = ui.painter().clone();
    let title = p.layout_no_wrap(row.label.to_owned(), theme::medium(14.0), theme::TEXT);
    let chips_h = if row.chips.is_empty() {
        0.0
    } else {
        CHIP_GAP + CHIP_H
    };
    let header_h = PAD.y * 2.0 + title.size().y + chips_h;
    let text_x = PAD.x + BOX + 12.0;
    let wrap = (width - text_x - PAD.x).max(40.0);
    let details: Vec<_> = if row.expanded {
        let pairs: Vec<(&str, &str)> = if row.details.is_empty() {
            vec![("", row.empty_details)]
        } else {
            row.details
                .iter()
                .map(|(t, d)| (t.as_str(), d.as_str()))
                .collect()
        };
        pairs
            .into_iter()
            .map(|(t, d)| {
                let title = (!t.is_empty())
                    .then(|| p.layout(t.to_owned(), theme::medium(13.0), theme::TEXT, wrap));
                let desc = p.layout(d.to_owned(), theme::body_sm(), theme::TEXT_DIM, wrap);
                (title, desc)
            })
            .collect()
    } else {
        Vec::new()
    };
    let body_h = if details.is_empty() {
        0.0
    } else {
        let content: f32 = details
            .iter()
            .map(|(t, d)| t.as_ref().map_or(0.0, |g| g.size().y + 2.0) + d.size().y)
            .sum::<f32>()
            + DETAIL_GAP * (details.len() - 1) as f32;
        1.0 + PAD.y * 2.0 + content
    };

    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, header_h + body_h), Sense::hover());
    let header = Rect::from_min_size(rect.min, Vec2::new(width, header_h));
    let (check_zone, chevron_zone) = header_zones(header);
    let id = ui.scope_id().with(("expand_row", id_salt));
    let check = ui.interact(check_zone, id.with("check"), Sense::click());
    let chevron = ui
        .interact(chevron_zone, id.with("chevron"), Sense::click())
        .on_hover_text(row.expand_hint);

    let (fill, border) = if row.checked {
        (theme::CHECKED_BG, theme::ACCENT_BORDER)
    } else if check.hovered() || chevron.hovered() {
        (theme::INNER_BG, theme::BORDER_INNER)
    } else {
        (theme::INNER_BG, theme::BORDER)
    };
    theme::paint_surface(&p, rect, 10.0, fill, border);

    let top = rect.top() + PAD.y;
    let cb = Rect::from_min_size(Pos2::new(rect.left() + PAD.x, top + 1.0), Vec2::splat(BOX));
    paint_checkbox(&p, cb, row.checked);
    let x = rect.left() + text_x;
    let title_h = title.size().y;
    p.galley(Pos2::new(x, top), title, theme::TEXT);
    let mut chip_x = x;
    let chip_y = top + title_h + CHIP_GAP + CHIP_H / 2.0;
    for (label, tone) in row.chips {
        let w = p
            .layout_no_wrap(label.clone(), theme::value_sm(), theme::TEXT)
            .size()
            .x
            + 16.0;
        paint_tag(&p, Pos2::new(chip_x + w, chip_y), label, *tone);
        chip_x += w + CHIP_GAP;
    }
    if let Some((meta, ink)) = row.meta {
        p.text(
            Pos2::new(chevron_zone.left() - 6.0, header.center().y),
            Align2::RIGHT_CENTER,
            meta,
            theme::body_sm(),
            ink,
        );
    }
    if chevron.hovered() {
        p.rect_filled(chevron_zone, 6.0, theme::SELECTED_BG);
    }
    paint_chevron(&p, chevron_zone.center(), row.expanded);

    if !details.is_empty() {
        let line_y = header.bottom();
        p.line_segment(
            [
                Pos2::new(rect.left() + 1.0, line_y),
                Pos2::new(rect.right() - 1.0, line_y),
            ],
            Stroke::new(1.0, theme::BORDER),
        );
        let mut y = line_y + 1.0 + PAD.y;
        for (title, desc) in details {
            if let Some(g) = title {
                let h = g.size().y;
                p.galley(Pos2::new(x, y), g, theme::TEXT);
                y += h + 2.0;
            }
            let h = desc.size().y;
            p.galley(Pos2::new(x, y), desc, theme::TEXT_DIM);
            y += h + DETAIL_GAP;
        }
    }

    if check.hovered() || chevron.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    ExpandRowResponse {
        toggled: check.clicked(),
        expand_clicked: chevron.clicked(),
    }
}

fn paint_chevron(p: &egui::Painter, c: Pos2, open: bool) {
    let dy = if open { -2.0 } else { 2.0 };
    p.add(egui::Shape::line(
        vec![
            c + Vec2::new(-4.0, -dy),
            c + Vec2::new(0.0, dy),
            c + Vec2::new(4.0, -dy),
        ],
        Stroke::new(1.6, theme::TEXT_DIM),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_zones_partition_the_header_without_overlap() {
        let header = Rect::from_min_size(Pos2::new(20.0, 40.0), Vec2::new(584.0, 66.0));
        let (check, chevron) = header_zones(header);
        assert_eq!(check.min, header.min);
        assert_eq!(check.max.x, chevron.min.x);
        assert!(header.contains_rect(chevron));
        assert!(!check.intersects(chevron.shrink(0.5)));
        assert_eq!(chevron.size(), Vec2::splat(CHEVRON));
    }
}
