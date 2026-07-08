// SPDX-License-Identifier: GPL-3.0-or-later
//! Toast notifications: daemon-pushed messages stacked in the bottom-right
//! corner. Info/warning toasts auto-dismiss after a timeout; errors stay until
//! the user dismisses them (matching the GTK frontend's toast priorities).

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{Notification, NotificationSeverity};

use crate::domain::models::notifications::notification_text;
use crate::ui::theme::{self, a};

/// Auto-dismiss window per severity (egui seconds). `None` = sticky.
fn timeout_secs(severity: NotificationSeverity) -> Option<f64> {
    match severity {
        NotificationSeverity::Error => None,
        NotificationSeverity::Warning => Some(8.0),
        NotificationSeverity::Info => Some(5.0),
    }
}

/// Accent color for a toast's severity stripe + title.
fn severity_color(severity: NotificationSeverity) -> Color32 {
    match severity {
        NotificationSeverity::Error => theme::TRAFFIC_RED,
        NotificationSeverity::Warning => theme::TRAFFIC_YELLOW,
        NotificationSeverity::Info => theme::STAT_CYAN,
    }
}

struct Toast {
    note: Notification,
    /// egui time the toast appeared.
    spawned: f64,
}

impl Toast {
    fn expired(&self, now: f64) -> bool {
        timeout_secs(self.note.code.severity()).is_some_and(|to| now - self.spawned >= to)
    }
}

/// Most toasts shown at once; older ones are dropped so a burst can't fill the
/// screen.
const MAX_TOASTS: usize = 5;

#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    /// Queue freshly-received notifications, stamping each with `now`.
    pub fn ingest(&mut self, incoming: impl IntoIterator<Item = Notification>, now: f64) {
        for note in incoming {
            self.items.push(Toast { note, spawned: now });
        }
        if self.items.len() > MAX_TOASTS {
            let drop = self.items.len() - MAX_TOASTS;
            self.items.drain(0..drop);
        }
    }

    /// Drop timed-out toasts. Returns whether any remain (callers keep
    /// repainting while toasts are live so timeouts fire without input).
    fn prune(&mut self, now: f64) -> bool {
        self.items.retain(|t| !t.expired(now));
        !self.items.is_empty()
    }

    /// Render the toast stack and handle dismiss clicks. Call once per frame
    /// after the page content so toasts overlay everything.
    pub fn show(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        if !self.prune(now) {
            return;
        }
        // Live toasts need a steady repaint so timeouts advance without input.
        ctx.request_repaint();

        let screen = ctx.content_rect();
        const W: f32 = 320.0;
        const GAP: f32 = 10.0;
        let mut dismiss: Option<usize> = None;

        egui::Area::new(egui::Id::new("toasts"))
            .order(egui::Order::Foreground)
            .fixed_pos(Pos2::ZERO)
            .show(ctx, |ui| {
                let mut bottom = screen.bottom() - 16.0;
                // Newest at the bottom, stacking upward.
                for (i, toast) in self.items.iter().enumerate().rev() {
                    let h = toast_height(ui, &toast.note, W);
                    let rect = Rect::from_min_size(
                        Pos2::new(screen.right() - 16.0 - W, bottom - h),
                        Vec2::new(W, h),
                    );
                    if toast_card(ui, rect, &toast.note, i) {
                        dismiss = Some(i);
                    }
                    bottom -= h + GAP;
                }
            });

        if let Some(i) = dismiss {
            self.items.remove(i);
        }
    }
}

/// Lay out the message body once to size the card to its wrapped text.
fn message_galley(ui: &egui::Ui, note: &Notification, w: f32) -> std::sync::Arc<egui::Galley> {
    let (_, message) = notification_text(&note.code);
    ui.painter()
        .layout(message, theme::body(12.0), theme::TEXT_DIM, w - 28.0)
}

fn toast_height(ui: &egui::Ui, note: &Notification, w: f32) -> f32 {
    let msg_h = message_galley(ui, note, w).size().y;
    // top pad + title + gap + message + bottom pad.
    14.0 + 16.0 + 6.0 + msg_h + 14.0
}

/// Paint one toast; returns `true` if its dismiss button was clicked.
fn toast_card(ui: &mut egui::Ui, rect: Rect, note: &Notification, idx: usize) -> bool {
    let (title, _) = notification_text(&note.code);
    let color = severity_color(note.code.severity());
    let p = ui.painter();
    theme::halo(p, rect, 12.0, a(Color32::BLACK, 0.5), 24.0);
    p.rect_filled(rect, 12.0, theme::CARD_BG);
    p.rect_stroke(
        rect,
        12.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Middle,
    );
    // Severity stripe down the left edge.
    let stripe = Rect::from_min_size(rect.left_top(), Vec2::new(3.0, rect.height()));
    p.rect_filled(stripe, 1.5, color);

    p.text(
        Pos2::new(rect.left() + 14.0, rect.top() + 14.0),
        Align2::LEFT_TOP,
        &title,
        theme::semibold(13.0),
        color,
    );
    let galley = message_galley(ui, note, rect.width());
    ui.painter().galley(
        Pos2::new(rect.left() + 14.0, rect.top() + 14.0 + 16.0 + 6.0),
        galley,
        theme::TEXT_DIM,
    );

    // Dismiss button (top-right ×).
    let x_rect = Rect::from_center_size(
        Pos2::new(rect.right() - 14.0, rect.top() + 16.0),
        Vec2::splat(18.0),
    );
    let resp = ui.interact(
        x_rect,
        ui.id().with(("toast_x", idx, note.timestamp_ms)),
        Sense::click(),
    );
    let x_col = if resp.hovered() {
        theme::TEXT
    } else {
        theme::TEXT_FAINT
    };
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    ui.painter().text(
        x_rect.center(),
        Align2::CENTER_CENTER,
        "×",
        theme::body(13.0),
        x_col,
    );
    resp.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A notification whose derived severity matches `severity` (used to drive
    /// the timeout/prune tests without depending on specific copy).
    fn note(severity: NotificationSeverity) -> Notification {
        use halod_shared::types::NotificationCode;
        let code = match severity {
            NotificationSeverity::Info => NotificationCode::ProfileSwitched {
                profile: "p".into(),
            },
            NotificationSeverity::Warning => NotificationCode::FanStalled { fan: "f".into() },
            NotificationSeverity::Error => NotificationCode::Generic {
                message: "m".into(),
            },
        };
        Notification {
            code,
            timestamp_ms: 0,
        }
    }

    #[test]
    fn errors_are_sticky_others_time_out() {
        assert_eq!(timeout_secs(NotificationSeverity::Error), None);
        assert!(timeout_secs(NotificationSeverity::Warning).is_some());
        assert!(timeout_secs(NotificationSeverity::Info).is_some());
    }

    #[test]
    fn info_expires_after_its_timeout_but_not_before() {
        let t = Toast {
            note: note(NotificationSeverity::Info),
            spawned: 100.0,
        };
        let to = timeout_secs(NotificationSeverity::Info).unwrap();
        assert!(!t.expired(100.0));
        assert!(!t.expired(100.0 + to - 0.01));
        assert!(t.expired(100.0 + to));
    }

    #[test]
    fn error_toast_never_expires() {
        let t = Toast {
            note: note(NotificationSeverity::Error),
            spawned: 0.0,
        };
        assert!(!t.expired(1e9));
    }

    #[test]
    fn prune_keeps_sticky_drops_expired() {
        let mut toasts = Toasts::default();
        toasts.ingest(
            [
                note(NotificationSeverity::Info),
                note(NotificationSeverity::Error),
            ],
            0.0,
        );
        // After the info timeout only the sticky error remains.
        let after = timeout_secs(NotificationSeverity::Info).unwrap() + 1.0;
        assert!(toasts.prune(after));
        assert_eq!(toasts.items.len(), 1);
        assert_eq!(
            toasts.items[0].note.code.severity(),
            NotificationSeverity::Error
        );
    }

    #[test]
    fn ingest_caps_at_max_keeping_newest() {
        use halod_shared::types::NotificationCode;
        let mut toasts = Toasts::default();
        let many: Vec<Notification> = (0..MAX_TOASTS + 3)
            .map(|i| Notification {
                code: NotificationCode::ProfileSwitched {
                    profile: i.to_string(),
                },
                timestamp_ms: i as u64,
            })
            .collect();
        toasts.ingest(many, 0.0);
        assert_eq!(toasts.items.len(), MAX_TOASTS);
        // Oldest (timestamps 0..2) were dropped; newest kept.
        assert_eq!(toasts.items[0].note.timestamp_ms, 3);
    }
}
