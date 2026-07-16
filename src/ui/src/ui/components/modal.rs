// SPDX-License-Identifier: GPL-3.0-or-later
//! Modal dialogs and the shared delete-confirmation reducer.

use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};

use crate::ui::theme;

/// Inner margin of every modal frame; the close × is inset from the frame edge.
const MODAL_MARGIN: f32 = 18.0;
/// Inset of the close × from the modal's top and right edges.
const CLOSE_INSET: f32 = 16.0;

/// A centered modal dialog over a dimmed backdrop, with a title bar, close ×
/// button, and a scrollable body. Returns `true` when the user closed it
/// (backdrop click or ×). `id` must be unique per concurrently-open modal.
pub fn modal_frame<R>(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    default_w: f32,
    default_h: f32,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> bool {
    modal_frame_raw(ctx, id, title, default_w, default_h, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, body);
    })
}

/// The single modal implementation: dimmed backdrop, title bar, × button, then
/// `contents`. Every modal in the app routes through this.
fn modal_shell(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    subtitle: Option<&str>,
    width: f32,
    contents: impl FnOnce(&mut egui::Ui),
) -> bool {
    let w = width.min(ctx.content_rect().width() - 80.0);
    let mut x_clicked = false;
    let resp = egui::Modal::new(egui::Id::new((id, "modal")))
        .frame(
            egui::Frame::NONE
                .fill(theme::MODAL_BG)
                .stroke(Stroke::new(1.0, theme::BORDER))
                .corner_radius(theme::RADIUS_LG)
                .inner_margin(theme::PAD_MODAL),
        )
        .show(ctx, |ui| {
            ui.set_width(w);
            let title_center_y = ui
                .vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    let title_resp = ui.label(
                        egui::RichText::new(title)
                            .font(theme::title())
                            .color(theme::TEXT),
                    );
                    if let Some(subtitle) = subtitle {
                        ui.label(
                            egui::RichText::new(subtitle)
                                .font(theme::body_sm())
                                .color(theme::TEXT_MUT),
                        );
                    }
                    title_resp.rect.center().y
                })
                .inner;
            ui.add_space(theme::SPACE_4);
            contents(ui);
            if close_button(ui, id, title_center_y) {
                x_clicked = true;
            }
        });
    x_clicked || resp.should_close()
}

fn close_button_rect(content: Rect, size: f32, center_y: f32) -> Rect {
    let frame_right = content.right() + MODAL_MARGIN;
    Rect::from_min_size(
        Pos2::new(frame_right - CLOSE_INSET - size, center_y - size / 2.0),
        Vec2::splat(size),
    )
}

fn close_button(ui: &mut egui::Ui, id: &str, center_y: f32) -> bool {
    let rect = close_button_rect(ui.min_rect(), 24.0, center_y);
    let resp = ui.interact(rect, ui.id().with((id, "close")), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let col = if resp.hovered() {
        theme::TEXT
    } else {
        theme::TEXT_FAINT
    };
    ui.ctx().layer_painter(ui.layer_id()).text(
        rect.center(),
        Align2::CENTER_CENTER,
        "×",
        theme::title(),
        col,
    );
    resp.clicked()
}

/// Like [`modal_frame`] but without the outer vertical scroll area, so the
/// caller controls scrolling (e.g. scrolling only a list while keeping action
/// buttons pinned).
pub fn modal_frame_raw<R>(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    default_w: f32,
    default_h: f32,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> bool {
    // Fix the height (min == max) so a body that sizes a scroll area from
    // `available_height` stays bounded instead of growing every frame. Pin it
    // on a fresh child ui: `set_max_height` on a ui that already holds the
    // title row rewinds its cursor to the top, painting the body over the
    // title bar.
    let h = default_h.min(ctx.content_rect().height() - 80.0);
    modal_shell(ctx, id, title, None, default_w, |ui| {
        ui.allocate_ui(egui::Vec2::new(ui.available_width(), h), |ui| {
            ui.set_min_height(h);
            ui.set_max_height(h);
            body(ui);
        });
    })
}

/// Content-sized modal: title, `body`, and right-aligned `actions` (add the
/// confirm button first). Returns `true` when dismissed.
pub fn dialog(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    width: f32,
    body: impl FnOnce(&mut egui::Ui),
    actions: impl FnOnce(&mut egui::Ui),
) -> bool {
    let max_body = (ctx.content_rect().height() - 220.0).max(120.0);
    modal_shell(ctx, id, title, None, width, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(max_body)
            .show(ui, body);
        ui.add_space(theme::SPACE_8);
        // Height-bound the centered action row: with padding below it, an
        // unbounded row claims the auto-sizing modal's full height and grows it
        // every frame. Zero desired height still expands to fit tall buttons.
        ui.allocate_ui_with_layout(
            egui::Vec2::new(ui.available_width(), 0.0),
            egui::Layout::right_to_left(egui::Align::Center),
            actions,
        );
        ui.add_space(theme::SPACE_3);
    })
}

/// Content-sized dialog with a compact subtitle directly beneath its title.
pub fn dialog_with_subtitle(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    subtitle: &str,
    width: f32,
    body: impl FnOnce(&mut egui::Ui),
    actions: impl FnOnce(&mut egui::Ui),
) -> bool {
    let max_body = (ctx.content_rect().height() - 220.0).max(120.0);
    modal_shell(ctx, id, title, Some(subtitle), width, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(max_body)
            .show(ui, body);
        ui.add_space(theme::SPACE_8);
        ui.separator();
        // The modal frame's 18px bottom margin is the button→bottom gap, so match
        // the divider→button gap to it and add no trailing space of our own.
        ui.add_space(theme::SPACE_9);
        ui.allocate_ui_with_layout(
            egui::Vec2::new(ui.available_width(), 0.0),
            egui::Layout::right_to_left(egui::Align::Center),
            actions,
        );
    })
}

/// A modal showing a plugin issue's full detail text (monospace), with Copy and
/// Close actions. Returns `true` when dismissed (backdrop, ×, or Close).
pub fn issue_modal(ctx: &egui::Context, id: &str, title: &str, detail: &str) -> bool {
    use super::button::{button, ButtonKind};
    let copied_key = egui::Id::new((id, "copied_at"));
    let mut close = false;
    let dismissed = dialog(
        ctx,
        id,
        title,
        520.0,
        |ui| {
            ui.label(
                egui::RichText::new(detail)
                    .font(theme::mono(12.0))
                    .color(theme::TEXT_DIM),
            );
        },
        |ui| {
            if button(
                ui,
                &t!("plugins.issue_close"),
                ButtonKind::Primary,
                egui::vec2(110.0, 32.0),
            )
            .clicked()
            {
                close = true;
            }
            ui.add_space(theme::SPACE_4);
            if button(
                ui,
                &t!("plugins.issue_copy"),
                ButtonKind::Ghost,
                egui::vec2(110.0, 32.0),
            )
            .clicked()
            {
                ui.ctx().copy_text(detail.to_owned());
                let now = ui.ctx().input(|i| i.time);
                ui.ctx().data_mut(|d| d.insert_temp(copied_key, now));
            }
            let copied_at = ui.ctx().data(|d| d.get_temp::<f64>(copied_key));
            let now = ui.ctx().input(|i| i.time);
            if crate::ui::screens::settings::copied_feedback_visible(copied_at, now) {
                ui.add_space(theme::SPACE_5);
                ui.label(
                    egui::RichText::new(t!("plugins.issue_copied"))
                        .font(theme::subhead())
                        .color(theme::TRAFFIC_GREEN),
                );
                ui.ctx().request_repaint();
            }
        },
    );
    dismissed || close
}

/// Reduce a delete/remove-confirmation modal's outcome: `Some(target)` means
/// the action was confirmed; the pending state is cleared on any outcome.
pub fn resolve_delete_confirm<T>(
    pending: &mut Option<T>,
    confirm: bool,
    dismiss: bool,
) -> Option<T> {
    if confirm {
        pending.take()
    } else {
        if dismiss {
            *pending = None;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::button::{button, ButtonKind};
    use super::*;
    use egui::{Pos2, Rect, Sense, Vec2};

    #[test]
    fn close_button_is_16px_from_right_and_centered_on_the_title() {
        // Right edge inset 16px from the frame edge (18px beyond content);
        // vertically centered on the title's line, wherever that sits.
        let content = Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(300.0, 200.0));
        let size = 24.0;
        let title_center_y = 118.0;
        let r = close_button_rect(content, size, title_center_y);
        assert_eq!(r.right(), content.right() + MODAL_MARGIN - CLOSE_INSET);
        assert_eq!(r.center().y, title_center_y);
        assert_eq!(r.size(), Vec2::splat(size));
    }

    #[test]
    fn modal_frame_raw_height_stays_bounded_across_frames() {
        // Reproduces the picker layout: a scroll area sized from
        // `available_height` with `auto_shrink=false`. Before the height was
        // capped, this fed the auto-sizing modal and grew every frame.
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 1000.0))),
            ..Default::default()
        };
        let avail = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        for _ in 0..6 {
            let avail = avail.clone();
            let _ = ctx.run_ui(input(), |ui| {
                modal_frame_raw(ui.ctx(), "t", "T", 440.0, 520.0, |ui| {
                    ui.label("Choose which running apps trigger this profile.");
                    ui.add_space(theme::SPACE_6);
                    let mut s = String::new();
                    ui.add(
                        egui::TextEdit::singleline(&mut s)
                            .desired_width(f32::INFINITY)
                            .margin(egui::vec2(10.0, 9.0)),
                    );
                    ui.add_space(theme::SPACE_4);
                    avail.borrow_mut().push(ui.available_height());
                    let list = (ui.available_height() - 60.0).max(60.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height(list)
                        .show(ui, |ui| {
                            for _ in 0..3 {
                                ui.allocate_exact_size(
                                    Vec2::new(ui.available_width(), 40.0),
                                    Sense::hover(),
                                );
                            }
                        });
                    ui.add_space(theme::SPACE_6);
                    ui.separator();
                    ui.add_space(theme::SPACE_4);
                    let _ = button(ui, "OK", ButtonKind::Primary, Vec2::new(160.0, 34.0));
                });
            });
        }
        let avail = avail.borrow();
        // Settle after the first frame, then the available height must not creep
        // upward frame over frame (it did before the modal height was capped).
        assert!(
            (avail.last().unwrap() - avail[1]).abs() < 1.0,
            "modal height grew across frames: {avail:?}"
        );
    }

    #[test]
    fn modal_body_starts_below_the_title_row() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 1000.0))),
            ..Default::default()
        };
        let body_top = std::rc::Rc::new(std::cell::RefCell::new(0.0f32));
        for _ in 0..2 {
            let body_top = body_top.clone();
            let _ = ctx.run_ui(input(), |ui| {
                modal_frame(ui.ctx(), "t", "Title", 400.0, 300.0, |ui| {
                    *body_top.borrow_mut() = ui.max_rect().top();
                    ui.label("body");
                });
            });
        }
        let modal_top = ctx
            .memory(|m| m.area_rect(egui::Id::new(("t", "modal"))))
            .expect("modal area rect")
            .top();
        // 18px frame margin, then a real title row before the body starts.
        let body_top = *body_top.borrow();
        assert!(
            body_top >= modal_top + 18.0 + 15.0,
            "modal body overlaps the title row: body_top={body_top} modal_top={modal_top}"
        );
    }
}

#[cfg(test)]
mod dialog_tests {
    use super::super::button::{button, ButtonKind};
    use super::*;
    use egui::{Pos2, Rect, Vec2};

    fn dialog_heights_over_frames(button_h: f32) -> Vec<f32> {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 1000.0))),
            ..Default::default()
        };
        let mut heights = Vec::new();
        for _ in 0..8 {
            let _ = ctx.run_ui(input(), |ui| {
                dialog(
                    ui.ctx(),
                    "d",
                    "Title",
                    420.0,
                    |ui| {
                        ui.label("Body text.");
                        ui.add_space(theme::SPACE_6);
                    },
                    |ui| {
                        let _ = button(ui, "OK", ButtonKind::Primary, Vec2::new(120.0, button_h));
                    },
                );
            });
            heights.push(
                ctx.memory(|m| m.area_rect(egui::Id::new(("d", "modal"))))
                    .map(|r| r.height())
                    .unwrap_or(0.0),
            );
        }
        heights
    }

    #[test]
    fn dialog_height_stays_bounded_across_frames() {
        // The action row's vertically-centered layout followed by bottom
        // padding used to claim the auto-sizing modal's full height and feed
        // back, growing the modal every frame.
        let h = dialog_heights_over_frames(34.0);
        assert!(
            (h.last().unwrap() - h[1]).abs() < 1.0,
            "dialog modal grew across frames: {h:?}"
        );
    }

    #[test]
    fn dialog_action_row_grows_to_fit_tall_buttons() {
        // A taller action button must enlarge the modal (not get clipped by the
        // height-bounded action row) — and still not creep frame over frame.
        let short = dialog_heights_over_frames(34.0);
        let tall = dialog_heights_over_frames(60.0);
        assert!(
            tall[1] > short[1] + 20.0,
            "tall action button did not enlarge the modal: short={short:?} tall={tall:?}"
        );
        assert!(
            (tall.last().unwrap() - tall[1]).abs() < 1.0,
            "dialog with tall buttons grew across frames: {tall:?}"
        );
    }
}
