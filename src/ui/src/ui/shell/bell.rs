// SPDX-License-Identifier: GPL-3.0-or-later
//! Notifications bell: unread badge in the title bar and the history panel.

use egui::{Align2, Pos2, Rect, Sense, Stroke, Vec2};

use crate::domain::models::notifications::notification_text;
use crate::ui::components::{self as widgets, toast::Toasts, ButtonKind};
use crate::ui::theme;

const PANEL_W: f32 = 440.0;
const LIST_MAX_H: f32 = 680.0;

fn open_id() -> egui::Id {
    egui::Id::unique("notifications_open")
}

/// Short relative age for a notification received `secs` ago.
fn format_age(secs: f64) -> String {
    let secs = secs.max(0.0) as u64;
    match secs {
        0..60 => t!("shell.age_now").to_string(),
        60..3600 => t!("shell.age_min", n = secs / 60).to_string(),
        3600..86400 => t!("shell.age_hour", n = secs / 3600).to_string(),
        _ => t!("shell.age_day", n = secs / 86400).to_string(),
    }
}

/// The bell button with its right edge at `right`. Returns its rect.
pub fn bell(ui: &mut egui::Ui, right: f32, cy: f32, toasts: &mut Toasts) -> Rect {
    let rect = Rect::from_min_size(Pos2::new(right - 36.0, cy - 16.0), Vec2::new(36.0, 32.0));
    let resp = ui.interact(rect, open_id().with("button"), Sense::click());
    let mut open = toasts.panel_open;
    if open || resp.hovered() {
        theme::paint_surface(
            ui.painter(),
            rect,
            theme::RADIUS_SM,
            theme::ROW_ACTIVE,
            theme::BORDER_INNER,
        );
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    paint_bell(ui.painter(), rect.center());
    let unread = toasts.unread();
    if unread > 0 {
        let label = if unread > 9 {
            "9+".to_owned()
        } else {
            unread.to_string()
        };
        let galley = ui
            .painter()
            .layout_no_wrap(label, theme::bold(10.0), theme::ON_ACCENT);
        let w = (galley.size().x + 8.0).max(16.0);
        let pill = Rect::from_min_size(
            rect.right_top() + Vec2::new(5.0 - w, -5.0),
            Vec2::new(w, 16.0),
        );
        ui.painter().rect_filled(pill, 8.0, theme::CYAN);
        ui.painter().galley(
            pill.center() - galley.size() / 2.0,
            galley,
            theme::ON_ACCENT,
        );
    }
    if resp.clicked() {
        open = !open;
    }
    if open && panel(ui.ctx(), rect, toasts, resp.clicked()) {
        open = false;
    }
    toasts.panel_open = open;
    rect
}

fn paint_bell(p: &egui::Painter, c: Pos2) {
    let stroke = Stroke::new(1.4, theme::TEXT_SOFT);
    let mut pts = vec![c + Vec2::new(-4.0, 3.0)];
    pts.extend((0..=12).map(|i| {
        let ang = std::f32::consts::PI * (1.0 + i as f32 / 12.0);
        c + Vec2::new(4.0 * ang.cos(), -1.0 + 4.0 * ang.sin())
    }));
    pts.extend([
        c + Vec2::new(4.0, 3.0),
        c + Vec2::new(5.2, 4.5),
        c + Vec2::new(-5.2, 4.5),
        c + Vec2::new(-4.0, 3.0),
    ]);
    p.add(egui::Shape::line(pts, stroke));
    p.line_segment([c + Vec2::new(-1.5, 6.0), c + Vec2::new(1.5, 6.0)], stroke);
}

/// The history dropdown under `anchor`. Returns whether it should close.
fn panel(ctx: &egui::Context, anchor: Rect, toasts: &mut Toasts, just_opened: bool) -> bool {
    let now = ctx.input(|i| i.time);
    let area = egui::Area::new(open_id().with("panel"))
        .order(egui::Order::Foreground)
        .fixed_pos(Pos2::new(anchor.right() - PANEL_W, anchor.bottom() + 6.0))
        .show(ctx, |ui| {
            egui::Frame::NONE
                .fill(theme::MODAL_BG)
                .stroke(Stroke::new(1.0, theme::BORDER_INNER))
                .corner_radius(theme::RADIUS_LG)
                .shadow(egui::epaint::Shadow {
                    offset: [0, 16],
                    blur: 40,
                    spread: 0,
                    color: theme::a(egui::Color32::BLACK, 0.45),
                })
                .show(ui, |ui| {
                    ui.set_width(PANEL_W);
                    egui::Frame::NONE
                        .inner_margin(egui::Margin::symmetric(16, 10))
                        .show(ui, |ui| {
                            egui::Sides::new().height(30.0).show(
                                ui,
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(t!("shell.notifications"))
                                            .font(theme::semibold(15.0))
                                            .color(theme::TEXT),
                                    );
                                },
                                |ui| {
                                    if widgets::button(
                                        ui,
                                        &t!("shell.clear"),
                                        ButtonKind::Ghost,
                                        Vec2::new(0.0, 30.0),
                                    )
                                    .clicked()
                                    {
                                        toasts.clear_history();
                                    }
                                    if widgets::button(
                                        ui,
                                        &t!("shell.mark_all_read"),
                                        ButtonKind::Ghost,
                                        Vec2::new(0.0, 30.0),
                                    )
                                    .clicked()
                                    {
                                        toasts.mark_all_read();
                                    }
                                },
                            );
                        });
                    ui.painter().hline(
                        ui.min_rect().x_range(),
                        ui.min_rect().bottom(),
                        Stroke::new(1.0, theme::BORDER_SOFT),
                    );
                    egui::ScrollArea::vertical()
                        .max_height(list_height(ctx, anchor))
                        .show(ui, |ui| {
                            egui::Frame::NONE
                                .inner_margin(egui::Margin::same(6))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    if toasts.history().next().is_none() {
                                        ui.add_space(theme::SPACE_8);
                                        ui.vertical_centered(|ui| {
                                            ui.label(
                                                egui::RichText::new(t!("shell.no_notifications"))
                                                    .font(theme::body_sm())
                                                    .color(theme::TEXT_MUT),
                                            );
                                        });
                                        ui.add_space(theme::SPACE_8);
                                    }
                                    let clicked = toasts
                                        .history()
                                        .filter(|logged| history_row(ui, logged, now))
                                        .last()
                                        .map(|logged| logged.note.clone());
                                    if let Some(note) = clicked {
                                        toasts.open(note);
                                    }
                                });
                        });
                });
        });
    !just_opened && area.response.clicked_elsewhere()
}

fn list_height(ctx: &egui::Context, anchor: Rect) -> f32 {
    (ctx.content_rect().bottom() - anchor.bottom() - 80.0).clamp(160.0, LIST_MAX_H)
}

fn history_row(ui: &mut egui::Ui, logged: &widgets::toast::Logged, now: f64) -> bool {
    let (title, body) = notification_text(&logged.note.code);
    let width = ui.available_width();
    let text_w = width - 20.0 - 40.0;
    let body_galley = ui
        .painter()
        .layout(body, theme::body_md(), theme::TEXT_DIM, text_w);
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(width, 24.0 + 3.0 + body_galley.size().y + 22.0 - 12.0),
        Sense::click(),
    );
    let p = ui.painter();
    if !logged.read {
        p.rect_filled(rect, theme::RADIUS_SM, theme::RAISED_BG);
    }
    widgets::toast::paint_severity_icon(
        p,
        Rect::from_min_size(rect.min + Vec2::new(10.0, 12.0), Vec2::splat(28.0)),
        logged.note.code.severity(),
    );
    let x = rect.left() + 50.0;
    let age = p.text(
        Pos2::new(rect.right() - 10.0, rect.top() + 12.0),
        Align2::RIGHT_TOP,
        format_age(now - logged.received),
        theme::body_sm(),
        theme::TEXT_FAINT,
    );
    p.with_clip_rect(Rect::from_min_max(
        rect.min,
        Pos2::new(age.left() - 8.0, rect.bottom()),
    ))
    .text(
        Pos2::new(x, rect.top() + 11.0),
        Align2::LEFT_TOP,
        title,
        if logged.read {
            theme::medium(14.0)
        } else {
            theme::semibold(14.0)
        },
        theme::TEXT,
    );
    p.galley(
        Pos2::new(x, rect.top() + 32.0),
        body_galley,
        theme::TEXT_DIM,
    );
    response.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_buckets_are_monotonic() {
        assert_eq!(format_age(-5.0), format_age(0.0));
        assert_eq!(format_age(59.0), t!("shell.age_now"));
        assert_eq!(format_age(120.0), t!("shell.age_min", n = 2));
        assert_eq!(format_age(7200.0), t!("shell.age_hour", n = 2));
        assert_eq!(format_age(3.0 * 86400.0), t!("shell.age_day", n = 3));
    }
}
