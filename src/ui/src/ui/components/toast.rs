// SPDX-License-Identifier: GPL-3.0-or-later
//! Toast notifications: daemon-pushed messages stacked in the bottom-right
//! corner. Info/warning toasts auto-dismiss after a timeout; errors stay until
//! the user dismisses them (matching the GTK frontend's toast priorities).

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{Notification, NotificationCode, NotificationSeverity};

use crate::domain::models::notifications::notification_text;
use crate::ui::theme::{self, a};

/// Auto-dismiss window for a code (egui seconds). `None` = sticky. Codes that
/// back a Details modal stay until dismissed so there's always time to open it;
/// everything else follows its severity timeout.
fn toast_timeout(code: &NotificationCode) -> Option<f64> {
    if code.detail().is_some() {
        return None;
    }
    match code.severity() {
        NotificationSeverity::Error => None,
        NotificationSeverity::Warning => Some(8.0),
        NotificationSeverity::Info => Some(5.0),
    }
}

/// A toast shows a "Details" button when its code carries modal detail text.
fn shows_details(code: &NotificationCode) -> bool {
    code.detail().is_some()
}

/// Height of the Details button row.
const DETAILS_ROW_H: f32 = 30.0;

/// Extra card height for the Details row (0 when the code has no detail).
fn details_extra_height(note: &Notification) -> f32 {
    if shows_details(&note.code) {
        DETAILS_ROW_H
    } else {
        0.0
    }
}

/// Accent color for a toast's severity indicator and title.
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
        toast_timeout(&self.note.code).is_some_and(|to| now - self.spawned >= to)
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

    /// Render the toast stack, handling dismiss clicks. Returns the notification
    /// whose "Details" button was clicked, if any, so the caller can open its
    /// modal. Call once per frame after the page content so toasts overlay it.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<Notification> {
        let now = ctx.input(|i| i.time);
        if !self.prune(now) {
            return None;
        }
        // Live toasts need a steady repaint so timeouts advance without input.
        ctx.request_repaint();

        let screen = ctx.content_rect();
        const W: f32 = 320.0;
        const GAP: f32 = 10.0;
        let mut dismiss: Option<usize> = None;
        let mut details: Option<Notification> = None;

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
                    let action = toast_card(ui, rect, &toast.note, i);
                    if action.dismiss {
                        dismiss = Some(i);
                    }
                    if action.details {
                        details = Some(toast.note.clone());
                    }
                    bottom -= h + GAP;
                }
            });

        if let Some(i) = dismiss {
            self.items.remove(i);
        }
        details
    }
}

/// Lay out the message body once to size the card to its wrapped text.
fn message_galley(ui: &egui::Ui, note: &Notification, w: f32) -> std::sync::Arc<egui::Galley> {
    let (_, message) = notification_text(&note.code);
    ui.painter()
        .layout(message, theme::body_md(), theme::TEXT_DIM, w - 28.0)
}

fn toast_height(ui: &egui::Ui, note: &Notification, w: f32) -> f32 {
    let msg_h = message_galley(ui, note, w).size().y;
    // top pad + title + gap + message + bottom pad.
    14.0 + 16.0 + 6.0 + msg_h + 14.0 + details_extra_height(note)
}

/// What a rendered toast card's controls were clicked this frame.
#[derive(Default)]
struct ToastAction {
    dismiss: bool,
    details: bool,
}

/// Paint one toast, returning which of its controls were clicked.
fn toast_card(ui: &mut egui::Ui, rect: Rect, note: &Notification, idx: usize) -> ToastAction {
    let (title, _) = notification_text(&note.code);
    let color = severity_color(note.code.severity());
    let p = ui.painter();
    theme::halo(p, rect, 12.0, a(Color32::BLACK, 0.5), 24.0);
    p.rect_filled(rect, 12.0, theme::CARD_BG);
    p.rect_stroke(
        rect,
        12.0,
        Stroke::new(1.0, a(color, 0.34)),
        egui::StrokeKind::Middle,
    );

    // A compact severity dot keeps the color hint local to the title without
    // splitting the card with a persistent vertical bar.
    p.circle_filled(Pos2::new(rect.left() + 17.0, rect.top() + 21.0), 3.0, color);

    p.text(
        Pos2::new(rect.left() + 28.0, rect.top() + 14.0),
        Align2::LEFT_TOP,
        &title,
        theme::heading(),
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
        theme::body_lg(),
        x_col,
    );

    let mut action = ToastAction {
        dismiss: resp.clicked(),
        details: false,
    };

    // Details action: a compact pill in the bottom-right, separated from the
    // message copy so it reads as an intentional button rather than a loose
    // link at the edge of the card.
    if shows_details(&note.code) {
        let label = t!("plugins.issue_details");
        const DETAILS_W: f32 = 78.0;
        let d_rect = Rect::from_min_size(
            Pos2::new(
                rect.right() - 14.0 - DETAILS_W,
                rect.bottom() - DETAILS_ROW_H + 4.0,
            ),
            Vec2::new(DETAILS_W, 20.0),
        );
        let d_resp = ui.interact(
            d_rect,
            ui.id().with(("toast_details", idx, note.timestamp_ms)),
            Sense::click(),
        );
        let d_col = if d_resp.hovered() {
            color
        } else {
            theme::TEXT_DIM
        };
        if d_resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let button_fill = if d_resp.hovered() {
            a(color, 0.18)
        } else {
            a(color, 0.06)
        };
        ui.painter().rect_filled(d_rect, 8.0, button_fill);
        ui.painter().rect_stroke(
            d_rect,
            8.0,
            Stroke::new(1.0, a(color, if d_resp.hovered() { 0.55 } else { 0.28 })),
            egui::StrokeKind::Middle,
        );
        ui.painter().text(
            d_rect.center(),
            Align2::CENTER_CENTER,
            &label,
            theme::subhead(),
            d_col,
        );
        action.details = d_resp.clicked();
    }

    action
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

    fn plugin_error() -> Notification {
        use halod_shared::types::NotificationCode;
        Notification {
            code: NotificationCode::PluginRuntimeError {
                plugin: "wled".into(),
                detail: "boom".into(),
            },
            timestamp_ms: 0,
        }
    }

    fn device_write_error() -> Notification {
        Notification {
            code: NotificationCode::DeviceWriteFailed {
                device: "keyboard".into(),
                detail: "HID++ rejected write".into(),
            },
            timestamp_ms: 0,
        }
    }

    #[test]
    fn errors_are_sticky_others_time_out() {
        assert_eq!(toast_timeout(&note(NotificationSeverity::Error).code), None);
        assert!(toast_timeout(&note(NotificationSeverity::Warning).code).is_some());
        assert!(toast_timeout(&note(NotificationSeverity::Info).code).is_some());
    }

    #[test]
    fn detail_codes_are_sticky_and_show_details() {
        for n in [plugin_error(), device_write_error()] {
            assert!(shows_details(&n.code));
            assert_eq!(
                toast_timeout(&n.code),
                None,
                "detail toasts stay until dismissed"
            );
            assert_eq!(details_extra_height(&n), DETAILS_ROW_H);
        }
        // A no-detail warning is neither sticky nor gets a Details row.
        let w = note(NotificationSeverity::Warning);
        assert!(!shows_details(&w.code));
        assert!(toast_timeout(&w.code).is_some());
        assert_eq!(details_extra_height(&w), 0.0);
    }

    #[test]
    fn info_expires_after_its_timeout_but_not_before() {
        let t = Toast {
            note: note(NotificationSeverity::Info),
            spawned: 100.0,
        };
        let to = toast_timeout(&note(NotificationSeverity::Info).code).unwrap();
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
        let after = toast_timeout(&note(NotificationSeverity::Info).code).unwrap() + 1.0;
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
