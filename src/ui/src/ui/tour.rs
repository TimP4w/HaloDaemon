// SPDX-License-Identifier: GPL-3.0-or-later
//! Spotlight rendering: a dim scrim with a cutout around the current step's
//! anchor rect, plus a callout bubble with Next/Skip. Geometry is plain
//! functions on `Rect` so it's testable without a live egui frame; only
//! [`overlay`] touches egui. [`show`] is the frame driver called once at the
//! end of `App::draw`, tying the `domain::tour` reducer to this rendering.

use std::collections::BTreeSet;

use crate::ui::components as widgets;
use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};

use crate::ui::components::ButtonKind;
use crate::ui::theme::{self, a};

use crate::domain::tour::{self, Event, TourKey, TourState};
use crate::runtime::ipc::CommandTx;

const GAP: f32 = 12.0;

/// The highlighted region: the anchor rect padded a little, clamped on-screen.
fn spotlight_rect(anchor: Rect, screen: Rect) -> Rect {
    anchor.expand(6.0).intersect(screen)
}

/// Four strips tiling `screen` minus `cutout` (top/bottom/left/right), so the
/// dim mask never overlaps or misses part of the screen.
fn dim_rects(cutout: Rect, screen: Rect) -> [Rect; 4] {
    let c = cutout.intersect(screen);
    [
        Rect::from_min_max(screen.left_top(), Pos2::new(screen.right(), c.top())),
        Rect::from_min_max(Pos2::new(screen.left(), c.bottom()), screen.right_bottom()),
        Rect::from_min_max(
            Pos2::new(screen.left(), c.top()),
            Pos2::new(c.left(), c.bottom()),
        ),
        Rect::from_min_max(
            Pos2::new(c.right(), c.top()),
            Pos2::new(screen.right(), c.bottom()),
        ),
    ]
}

/// Where the callout bubble goes: below the cutout if it fits, else above,
/// else to the side; always clamped to stay fully on-screen.
fn bubble_rect(cutout: Rect, bubble_size: Vec2, screen: Rect) -> Rect {
    let below = Rect::from_min_size(Pos2::new(cutout.left(), cutout.bottom() + GAP), bubble_size);
    let above = Rect::from_min_size(
        Pos2::new(cutout.left(), cutout.top() - GAP - bubble_size.y),
        bubble_size,
    );
    let side = Rect::from_min_size(Pos2::new(cutout.right() + GAP, cutout.top()), bubble_size);

    let chosen = if below.bottom() <= screen.bottom() {
        below
    } else if above.top() >= screen.top() {
        above
    } else {
        side
    };
    clamp_within(chosen, screen)
}

fn clamp_within(r: Rect, screen: Rect) -> Rect {
    let size = r.size();
    let max_x = (screen.right() - size.x).max(screen.left());
    let max_y = (screen.bottom() - size.y).max(screen.top());
    Rect::from_min_size(
        Pos2::new(
            r.left().clamp(screen.left(), max_x),
            r.top().clamp(screen.top(), max_y),
        ),
        size,
    )
}

/// Render the spotlight + callout for the current step. Returns `Some` if the
/// user clicked Next or Skip this frame.
#[allow(clippy::too_many_arguments)]
fn overlay(
    ctx: &egui::Context,
    anchor: Rect,
    screen: Rect,
    title: &str,
    body: &str,
    step_index: usize,
    step_count: usize,
) -> Option<Event> {
    let cutout = spotlight_rect(anchor, screen);
    let fade = ctx.animate_bool_with_time(egui::Id::new("tour_fade"), true, 0.15);
    let mut event = None;

    egui::Area::new(egui::Id::new("tour_overlay"))
        .order(egui::Order::Foreground)
        .fixed_pos(Pos2::ZERO)
        .show(ctx, |ui| {
            // Swallow all input so only the bubble's own buttons are clickable.
            ui.interact(
                screen,
                ui.id().with("tour_barrier"),
                Sense::click_and_drag(),
            );

            let p = ui.painter();
            for strip in dim_rects(cutout, screen) {
                p.rect_filled(strip, 0.0, a(Color32::BLACK, 0.55 * fade));
            }
            p.rect_stroke(
                cutout,
                8.0,
                Stroke::new(2.0, a(theme::CYAN, fade)),
                egui::StrokeKind::Outside,
            );

            let body_w = 260.0;
            let galley = p.layout(body.to_string(), theme::body(12.5), theme::TEXT_DIM, body_w);
            let bubble_size = Vec2::new(body_w + 32.0, 24.0 + 22.0 + galley.size().y + 16.0 + 32.0);
            let bubble = bubble_rect(cutout, bubble_size, screen);

            theme::halo(p, bubble, 12.0, a(Color32::BLACK, 0.5 * fade), 24.0);
            p.rect_filled(bubble, 12.0, theme::CARD_BG);
            p.rect_stroke(
                bubble,
                12.0,
                Stroke::new(1.0, theme::BORDER),
                egui::StrokeKind::Inside,
            );

            p.text(
                Pos2::new(bubble.left() + 16.0, bubble.top() + 16.0),
                Align2::LEFT_TOP,
                title,
                theme::semibold(14.0),
                theme::TEXT,
            );
            p.galley(
                Pos2::new(bubble.left() + 16.0, bubble.top() + 16.0 + 22.0),
                galley,
                theme::TEXT_DIM,
            );

            p.text(
                Pos2::new(bubble.left() + 16.0, bubble.bottom() - 16.0),
                Align2::LEFT_BOTTOM,
                format!("{} / {step_count}", step_index + 1),
                theme::body(11.0),
                theme::TEXT_FAINT,
            );

            let btn_size = Vec2::new(70.0, 28.0);
            let next_rect = Rect::from_min_size(
                Pos2::new(
                    bubble.right() - 16.0 - btn_size.x,
                    bubble.bottom() - 16.0 - btn_size.y,
                ),
                btn_size,
            );
            let skip_rect = Rect::from_min_size(
                Pos2::new(next_rect.left() - 8.0 - btn_size.x, next_rect.top()),
                btn_size,
            );

            let is_last = step_index + 1 >= step_count;
            let skip_resp = widgets::button_at(
                ui,
                skip_rect,
                ui.id().with("tour_skip"),
                &t!("tour.skip"),
                ButtonKind::Ghost,
            );
            let next_resp = widgets::button_at(
                ui,
                next_rect,
                ui.id().with("tour_next"),
                &if is_last {
                    t!("tour.done")
                } else {
                    t!("tour.next")
                },
                ButtonKind::Primary,
            );

            if next_resp.clicked() {
                event = Some(Event::Next);
            } else if skip_resp.clicked() {
                event = Some(Event::Skip);
            }
        });

    event
}

/// Frame driver: called once at the end of `App::draw`, after every page has
/// registered its anchors for this frame. `key` is the tour applicable to
/// whatever the user is currently viewing (`None` if none applies).
pub(crate) fn show(
    ctx: &egui::Context,
    st: &mut TourState,
    daemon_seen: &BTreeSet<String>,
    cmd: &CommandTx,
    key: Option<TourKey>,
    connected: bool,
    suppressed: bool,
) {
    if tour::take_reset_request(ctx) {
        st.clear_local_seen();
    }
    if !connected || suppressed {
        return;
    }
    if let Some(key) = key {
        tour::maybe_start(st, daemon_seen, key);
    }

    let Some((step_index, step_count, title, body, anchor_id)) = st.current_step() else {
        return;
    };

    let now = ctx.input(|i| i.time);
    let anchor_rect = tour::take_anchor(ctx, anchor_id);
    let btn_event = anchor_rect.and_then(|rect| {
        let screen = ctx.content_rect();
        overlay(ctx, rect, screen, &title, &body, step_index, step_count)
    });

    if let Some(completed) = tour::advance(st, anchor_rect.is_some(), now, btn_event) {
        st.mark_locally_seen(completed.id);
        crate::domain::actions::system::mark_tour_seen(cmd, completed.id);
    }

    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn full_screen() -> Rect {
        Rect::from_min_size(Pos2::ZERO, Vec2::new(1920.0, 1080.0))
    }

    /// An anchor rect that always overlaps `screen` — the realistic domain,
    /// since a registered anchor always comes from a currently-rendered,
    /// on-screen widget. Origin inside the screen, size may extend past it.
    fn arb_onscreen_rect(screen: Rect) -> impl Strategy<Value = Rect> {
        (
            screen.left()..screen.right(),
            screen.top()..screen.bottom(),
            1.0f32..500.0,
            1.0f32..500.0,
        )
            .prop_map(|(x, y, w, h)| Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h)))
    }

    #[test]
    fn spotlight_rect_stays_within_a_generous_screen() {
        let screen = full_screen();
        let anchor = Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(50.0, 20.0));
        let cutout = spotlight_rect(anchor, screen);
        assert!(screen.contains_rect(cutout));
        assert!(cutout.contains_rect(anchor));
    }

    #[test]
    fn dim_rects_tile_the_screen_around_the_cutout() {
        let screen = full_screen();
        let cutout = Rect::from_min_size(Pos2::new(400.0, 300.0), Vec2::new(200.0, 100.0));
        let strips = dim_rects(cutout, screen);
        let strip_area: f32 = strips.iter().map(|r| r.area()).sum();
        let expected = screen.area() - cutout.area();
        assert!(
            (strip_area - expected).abs() < 1.0,
            "strip_area={strip_area} expected={expected}"
        );
    }

    #[test]
    fn bubble_rect_fits_on_a_generous_screen() {
        let screen = full_screen();
        let cutout = Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(50.0, 20.0));
        let bubble = bubble_rect(cutout, Vec2::new(300.0, 150.0), screen);
        assert!(screen.contains_rect(bubble));
    }

    proptest! {
        #[test]
        fn spotlight_rect_always_contains_the_anchor_intersected_with_screen(
            anchor in arb_onscreen_rect(full_screen()),
        ) {
            let screen = full_screen();
            let cutout = spotlight_rect(anchor, screen);
            prop_assert!(screen.contains_rect(cutout));
            prop_assert!(cutout.contains_rect(anchor.intersect(screen)));
        }

        #[test]
        fn bubble_rect_never_escapes_a_screen_larger_than_the_bubble(
            cutout in arb_onscreen_rect(Rect::from_min_size(Pos2::ZERO, Vec2::new(2400.0, 1400.0))),
            bw in 100.0f32..280.0,
            bh in 80.0f32..220.0,
        ) {
            let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(2400.0, 1400.0));
            let bubble = bubble_rect(cutout, Vec2::new(bw, bh), screen);
            prop_assert!(screen.contains_rect(bubble));
        }
    }
}
