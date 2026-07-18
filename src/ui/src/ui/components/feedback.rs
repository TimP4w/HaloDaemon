// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared progress affordances — the ring spinner, the gradient loading bar,
//! and the success badge. Every "something is happening / it's done" surface in
//! the app routes through these so they read as one family: the Drift lavender→
//! magenta gradient ([`theme::PROGRESS_A`]→[`theme::PROGRESS_B`]) over a common
//! [`theme::TRACK`].

use egui::{Pos2, Rect, Sense, Shape, Stroke, Vec2};

use crate::ui::icons;
use crate::ui::theme;

/// A rotating ring spinner: a faint full-circle track with a bright accent arc
/// sweeping around it. Drives its own repaint while visible.
pub fn spinner(ui: &mut egui::Ui, size: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let time = ui.input(|i| i.time) as f32;
    paint_spinner(ui.painter(), rect.center(), size, time);
    ui.ctx().request_repaint();
    resp
}

/// Paint a spinner of diameter `size` centered at `center` for clock `time`.
pub fn paint_spinner(painter: &egui::Painter, center: Pos2, size: f32, time: f32) {
    let stroke_w = (size / 11.0).clamp(2.0, 4.0);
    let radius = size / 2.0 - stroke_w / 2.0;
    painter.circle_stroke(center, radius, Stroke::new(stroke_w, theme::TRACK));
    let start = time * 3.2;
    let sweep = std::f32::consts::PI * 0.62;
    let steps = 24;
    let pts: Vec<Pos2> = (0..=steps)
        .map(|i| center + Vec2::angled(start + sweep * i as f32 / steps as f32) * radius)
        .collect();
    painter.add(Shape::line(pts, Stroke::new(stroke_w, theme::PROGRESS_A)));
}

/// A gradient loading bar filling `width`. `fraction: Some(f)` draws a
/// determinate fill (`f` clamped to `0..=1`); `None` animates an indeterminate
/// segment sweeping left→right and drives its own repaint.
pub fn progress_bar(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    fraction: Option<f32>,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let p = ui.painter();
    let r = rect.height() / 2.0;
    p.rect_filled(rect, r, theme::TRACK);
    // The gradient maps across the full inner width and is revealed through a
    // clip, so at any fraction the visible hues are the left slice of the same
    // spectrum rather than a squished full sweep.
    let inner = rect.shrink(1.0);
    match fraction {
        Some(f) => {
            let f = f.clamp(0.0, 1.0);
            if f > 0.0 {
                let clip =
                    Rect::from_min_size(inner.min, Vec2::new(inner.width() * f, inner.height()));
                gradient_fill(p, inner, clip);
            }
        }
        None => {
            let t = (ui.input(|i| i.time) as f32 * 0.9).rem_euclid(1.0);
            let (x0, x1) = indeterminate_window(t, 0.4);
            if x1 > x0 {
                let clip = Rect::from_min_max(
                    Pos2::new(inner.left() + inner.width() * x0, inner.top()),
                    Pos2::new(inner.left() + inner.width() * x1, inner.bottom()),
                );
                gradient_fill(p, inner, clip);
            }
            ui.ctx().request_repaint();
        }
    }
    resp
}

fn gradient_fill(painter: &egui::Painter, full: Rect, clip: Rect) {
    theme::h_gradient(
        &painter.with_clip_rect(clip),
        full,
        &[theme::PROGRESS_A, theme::PROGRESS_B],
    );
}

/// Visible sub-segment `(x0, x1)` in `0..=1` of an indeterminate bar at loop
/// phase `t` (`0..1`) for a segment of relative width `seg`: it enters from the
/// left and exits at the right, so both edges stay clamped to `0..=1`.
fn indeterminate_window(t: f32, seg: f32) -> (f32, f32) {
    let travel = 1.0 + seg;
    let start = t.rem_euclid(1.0) * travel - seg;
    (start.clamp(0.0, 1.0), (start + seg).clamp(0.0, 1.0))
}

/// The success badge: a deep-green disc + ring with a centered check. Shared by
/// the onboarding "done" page and the integration-pairing "done" step so both
/// finish on the same mark.
pub fn success_check(ui: &mut egui::Ui, diameter: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(diameter), Sense::hover());
    let r = diameter / 2.0;
    let c = rect.center();
    // Green halo behind the disc, echoing the logo mark's glow.
    theme::glow(ui.painter(), c, r * 1.6, theme::ONLINE, 0.32);
    ui.painter().circle_filled(c, r, theme::SUCCESS_FILL);
    ui.painter()
        .circle_stroke(c, r, Stroke::new(1.0, theme::SUCCESS_RING));
    icons::draw(
        ui,
        Rect::from_center_size(c, Vec2::splat(r)),
        icons::Icon::Check,
        theme::ONLINE,
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indeterminate_window_stays_ordered_and_clamped() {
        // At every phase the window is a valid sub-interval of the bar: ordered
        // and fully inside 0..=1 (no fill spilling past either end).
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            let (x0, x1) = indeterminate_window(t, 0.4);
            assert!((0.0..=1.0).contains(&x0), "x0 out of range at t={t}: {x0}");
            assert!((0.0..=1.0).contains(&x1), "x1 out of range at t={t}: {x1}");
            assert!(x0 <= x1, "window inverted at t={t}: {x0}..{x1}");
        }
    }

    #[test]
    fn indeterminate_window_enters_left_and_exits_right() {
        // Enters collapsed at the left edge, spans the full segment mid-loop,
        // and is exiting off the right edge as the loop nears its end (the loop
        // wraps at t=1, so the tail sits just below it).
        assert_eq!(indeterminate_window(0.0, 0.4), (0.0, 0.0));
        let (x0, x1) = indeterminate_window(0.5, 0.4);
        assert!(x0 > 0.0 && x1 < 1.0 && (x1 - x0 - 0.4).abs() < 1e-5);
        let (x0, x1) = indeterminate_window(0.99, 0.4);
        assert!(x0 > 0.9 && (x1 - 1.0).abs() < 1e-5);
    }
}
