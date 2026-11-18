// SPDX-License-Identifier: GPL-3.0-or-later
//! The square app tile: an app icon over its process name with a remove
//! button, plus the dashed "add" variant.

use egui::{Align2, Color32, Pos2, Rect, Response, Sense, Shape, Stroke, Vec2};

use crate::ui::components;
use crate::ui::theme;

pub const APP_TILE: f32 = 104.0;
const RADIUS: f32 = 28.0;
const ICON: f32 = 44.0;

/// What sits in the tile's icon slot.
pub enum AppIcon {
    Texture(egui::TextureId),
    /// A letter on a tinted square, for apps without an icon.
    Letter(char, Color32),
}

/// An app tile. Returns whether its remove button was clicked.
pub fn app_tile(ui: &mut egui::Ui, label: &str, icon: AppIcon) -> bool {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(APP_TILE), Sense::hover());
    let remove = Rect::from_min_size(
        Pos2::new(rect.right() - 28.0, rect.top() + 6.0),
        Vec2::splat(22.0),
    );
    let remove_resp = ui.interact(
        remove,
        ui.scope_id().with(("app_tile_remove", label)),
        Sense::click(),
    );
    let p = ui.painter();
    theme::paint_surface(p, rect, RADIUS, theme::RAISED_BG, theme::TRACK);
    let slot = Rect::from_min_size(
        Pos2::new(rect.center().x - ICON / 2.0, rect.top() + 14.0),
        Vec2::splat(ICON),
    );
    match icon {
        AppIcon::Texture(id) => {
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            p.image(id, slot, uv, Color32::WHITE);
        }
        AppIcon::Letter(letter, tint) => {
            p.rect_filled(slot, 13.0, tint);
            p.text(
                slot.center(),
                Align2::CENTER_CENTER,
                letter.to_string(),
                theme::bold(18.0),
                theme::ON_ACCENT,
            );
        }
    }
    let label = components::truncate_to_width(label, APP_TILE - 16.0, |s| {
        p.layout_no_wrap(s.to_owned(), theme::body_sm(), theme::TEXT)
            .size()
            .x
    });
    p.text(
        Pos2::new(rect.center().x, rect.bottom() - 18.0),
        Align2::CENTER_CENTER,
        label,
        theme::body_sm(),
        theme::TEXT_SOFT,
    );
    let hovered = remove_resp.hovered();
    p.circle_filled(
        remove.center(),
        11.0,
        if hovered {
            theme::BORDER_INNER
        } else {
            theme::TRACK
        },
    );
    let c = remove.center();
    let stroke = Stroke::new(
        1.6,
        if hovered {
            theme::TEXT
        } else {
            theme::TEXT_SOFT
        },
    );
    p.line_segment([c + Vec2::new(-2.5, -2.5), c + Vec2::new(2.5, 2.5)], stroke);
    p.line_segment([c + Vec2::new(2.5, -2.5), c + Vec2::new(-2.5, 2.5)], stroke);
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    remove_resp.clicked()
}

/// The dashed "add" tile.
pub fn add_app_tile(ui: &mut egui::Ui, label: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(APP_TILE), Sense::click());
    let hovered = resp.hovered();
    let p = ui.painter();
    if hovered {
        p.rect_filled(rect, RADIUS, theme::RAISED_BG);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    p.extend(Shape::dashed_line(
        &rounded_outline(rect.shrink(0.5), RADIUS),
        Stroke::new(1.0, theme::BORDER_STRONG),
        4.0,
        3.0,
    ));
    let c = Pos2::new(rect.center().x, rect.top() + 14.0 + ICON / 2.0);
    let ink = if hovered {
        theme::TEXT
    } else {
        theme::TEXT_DIM
    };
    let stroke = Stroke::new(1.8, ink);
    p.line_segment([c + Vec2::new(0.0, -7.0), c + Vec2::new(0.0, 7.0)], stroke);
    p.line_segment([c + Vec2::new(-7.0, 0.0), c + Vec2::new(7.0, 0.0)], stroke);
    p.text(
        Pos2::new(rect.center().x, rect.bottom() - 18.0),
        Align2::CENTER_CENTER,
        label,
        theme::body_sm(),
        ink,
    );
    resp
}

/// Closed polyline around a rounded rect, for dashed outlines.
pub fn rounded_outline(rect: Rect, r: f32) -> Vec<Pos2> {
    use std::f32::consts::FRAC_PI_2;
    let corners = [
        (Pos2::new(rect.right() - r, rect.top() + r), -FRAC_PI_2),
        (Pos2::new(rect.right() - r, rect.bottom() - r), 0.0),
        (Pos2::new(rect.left() + r, rect.bottom() - r), FRAC_PI_2),
        (Pos2::new(rect.left() + r, rect.top() + r), 2.0 * FRAC_PI_2),
    ];
    let mut pts: Vec<Pos2> = corners
        .iter()
        .flat_map(|&(c, start)| {
            (0..=8).map(move |i| {
                let ang = start + FRAC_PI_2 * i as f32 / 8.0;
                c + Vec2::new(ang.cos(), ang.sin()) * r
            })
        })
        .collect();
    pts.push(pts[0]);
    pts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounded_outline_stays_inside_and_closes() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::splat(APP_TILE));
        let pts = rounded_outline(rect, RADIUS);
        assert_eq!(pts.first(), pts.last());
        assert!(pts.iter().all(|p| rect.expand(0.01).contains(*p)));
    }
}
