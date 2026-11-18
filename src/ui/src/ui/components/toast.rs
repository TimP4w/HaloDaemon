// SPDX-License-Identifier: GPL-3.0-or-later
//! Toast notifications: daemon-pushed messages stacked in the bottom-right
//! corner. Info/warning toasts auto-dismiss after a timeout; errors stay until
//! the user dismisses them (matching the GTK frontend's toast priorities).

use egui::{Color32, Pos2, Rect, Stroke, Vec2};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToastButton {
    /// Opens the Details modal for codes that carry detail text.
    Details,
    /// Opens the notification's target page.
    Open,
}

fn toast_button(code: &NotificationCode) -> Option<ToastButton> {
    if code.detail().is_some() {
        Some(ToastButton::Details)
    } else if matches!(code, NotificationCode::UpdateAvailable { .. }) {
        Some(ToastButton::Open)
    } else {
        None
    }
}

/// Height of the optional action-button row.
const ACTION_ROW_H: f32 = 38.0;

fn action_row_height(note: &Notification) -> f32 {
    if toast_button(&note.code).is_some() {
        ACTION_ROW_H
    } else {
        0.0
    }
}

#[derive(Debug, Clone)]
pub enum ToastEvent {
    Details(Notification),
    Open(Notification),
}

/// Accent color for a toast's severity indicator and title.
fn severity_color(severity: NotificationSeverity) -> Color32 {
    match severity {
        NotificationSeverity::Error => theme::TRAFFIC_RED,
        NotificationSeverity::Warning => theme::TRAFFIC_YELLOW,
        NotificationSeverity::Info => theme::CYAN,
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

/// Most notifications the bell keeps.
const MAX_HISTORY: usize = 50;

/// A notification kept for the bell's history.
pub struct Logged {
    pub note: Notification,
    /// egui time it arrived.
    pub received: f64,
    pub read: bool,
}

#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
    history: std::collections::VecDeque<Logged>,
    /// Whether the bell's history panel is open.
    pub panel_open: bool,
    opened: Option<Notification>,
}

impl Toasts {
    pub fn open(&mut self, note: Notification) {
        self.panel_open = false;
        self.opened = Some(note);
    }

    /// Queue freshly-received notifications, stamping each with `now`.
    pub fn ingest(&mut self, incoming: impl IntoIterator<Item = Notification>, now: f64) {
        for note in incoming {
            self.history.push_front(Logged {
                note: note.clone(),
                received: now,
                read: false,
            });
            self.items.push(Toast { note, spawned: now });
        }
        self.history.truncate(MAX_HISTORY);
        if self.items.len() > MAX_TOASTS {
            let drop = self.items.len() - MAX_TOASTS;
            self.items.drain(0..drop);
        }
    }

    /// Newest first.
    pub fn history(&self) -> impl Iterator<Item = &Logged> {
        self.history.iter()
    }

    pub fn unread(&self) -> usize {
        self.history.iter().filter(|l| !l.read).count()
    }

    pub fn mark_all_read(&mut self) {
        for logged in &mut self.history {
            logged.read = true;
        }
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
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
    pub fn show(&mut self, ctx: &egui::Context) -> Option<ToastEvent> {
        let now = ctx.input(|i| i.time);
        if let Some(note) = self.opened.take() {
            return Some(ToastEvent::Open(note));
        }
        if !self.prune(now) {
            return None;
        }
        // Live toasts need a steady repaint so timeouts advance without input.
        ctx.request_repaint();

        let screen = ctx.content_rect();
        const W: f32 = 380.0;
        const GAP: f32 = 10.0;
        let mut dismiss: Option<usize> = None;
        let mut event = None;

        egui::Area::new(egui::Id::unique("toasts"))
            .order(egui::Order::Foreground)
            .fixed_pos(Pos2::ZERO)
            .show(ctx, |ui| {
                let mut bottom = screen.bottom() - 20.0;
                // Newest at the bottom, stacking upward.
                for (i, toast) in self.items.iter().enumerate().rev() {
                    let h = toast_height(ui, &toast.note, W);
                    let rect = Rect::from_min_size(
                        Pos2::new(screen.right() - 20.0 - W, bottom - h),
                        Vec2::new(W, h),
                    );
                    let left = toast_timeout(&toast.note.code)
                        .map(|to| (1.0 - (now - toast.spawned) / to).clamp(0.0, 1.0) as f32);
                    let action = toast_card(ui, rect, &toast.note, i, left);
                    if action.dismiss {
                        dismiss = Some(i);
                    }
                    if action.open {
                        dismiss = Some(i);
                        event = Some(ToastEvent::Open(toast.note.clone()));
                    }
                    if action.details {
                        event = Some(ToastEvent::Details(toast.note.clone()));
                    }
                    bottom -= h + GAP;
                }
            });

        if let Some(i) = dismiss {
            self.items.remove(i);
        }
        event
    }
}

const PAD: f32 = 14.0;
const ICON: f32 = 28.0;
const TEXT_X: f32 = PAD + ICON + 12.0;

fn text_width(w: f32) -> f32 {
    w - TEXT_X - 36.0
}

fn title_galley(ui: &egui::Ui, note: &Notification, w: f32) -> std::sync::Arc<egui::Galley> {
    let (title, _) = notification_text(&note.code);
    ui.painter()
        .layout(title, theme::semibold(14.0), theme::TEXT, text_width(w))
}

fn message_galley(ui: &egui::Ui, note: &Notification, w: f32) -> std::sync::Arc<egui::Galley> {
    let (_, message) = notification_text(&note.code);
    ui.painter()
        .layout(message, theme::body_md(), theme::TEXT_DIM, text_width(w))
}

fn toast_height(ui: &egui::Ui, note: &Notification, w: f32) -> f32 {
    let title_h = title_galley(ui, note, w).size().y;
    let msg_h = message_galley(ui, note, w).size().y;
    PAD + title_h + 3.0 + msg_h + PAD + action_row_height(note)
}

/// What a rendered toast card's controls were clicked this frame.
#[derive(Default)]
struct ToastAction {
    dismiss: bool,
    details: bool,
    open: bool,
}

/// Severity tile: a tinted square with the severity glyph, shared by toasts
/// and the notification history.
pub fn paint_severity_icon(p: &egui::Painter, rect: Rect, severity: NotificationSeverity) {
    let color = severity_color(severity);
    p.rect_filled(rect, 7.0, a(color, 0.14));
    let c = rect.center();
    let stroke = Stroke::new(1.5, color);
    match severity {
        NotificationSeverity::Info => {
            p.circle_stroke(c, 5.5, stroke);
            p.line_segment([c + Vec2::new(0.0, -0.5), c + Vec2::new(0.0, 3.0)], stroke);
            p.circle_filled(c + Vec2::new(0.0, -2.8), 0.9, color);
        }
        NotificationSeverity::Warning => {
            p.add(egui::Shape::closed_line(
                vec![
                    c + Vec2::new(0.0, -5.5),
                    c + Vec2::new(5.8, 4.7),
                    c + Vec2::new(-5.8, 4.7),
                ],
                stroke,
            ));
            p.line_segment([c + Vec2::new(0.0, -1.5), c + Vec2::new(0.0, 1.5)], stroke);
            p.circle_filled(c + Vec2::new(0.0, 3.2), 0.9, color);
        }
        NotificationSeverity::Error => {
            p.circle_stroke(c, 5.5, stroke);
            p.line_segment([c + Vec2::new(0.0, -3.0), c + Vec2::new(0.0, 0.8)], stroke);
            p.circle_filled(c + Vec2::new(0.0, 2.8), 0.9, color);
        }
    }
}

/// Paint one toast, returning which of its controls were clicked. `left` is
/// the remaining fraction of an auto-dismissing toast's lifetime.
fn toast_card(
    ui: &mut egui::Ui,
    rect: Rect,
    note: &Notification,
    idx: usize,
    left: Option<f32>,
) -> ToastAction {
    let severity = note.code.severity();
    let body = ui.interact(
        rect,
        ui.scope_id().with(("toast_body", idx, note.timestamp_ms)),
        egui::Sense::click(),
    );
    let p = ui.painter();
    theme::halo(p, rect, theme::RADIUS_MD, a(Color32::BLACK, 0.4), 24.0);
    theme::paint_surface(
        p,
        rect,
        theme::RADIUS_MD,
        theme::RAISED_BG,
        theme::BORDER_INNER,
    );
    paint_severity_icon(
        p,
        Rect::from_min_size(rect.min + Vec2::splat(PAD), Vec2::splat(ICON)),
        severity,
    );
    let title = title_galley(ui, note, rect.width());
    let title_h = title.size().y;
    p.galley(
        Pos2::new(rect.left() + TEXT_X, rect.top() + PAD),
        title,
        theme::TEXT,
    );
    let galley = message_galley(ui, note, rect.width());
    ui.painter().galley(
        Pos2::new(rect.left() + TEXT_X, rect.top() + PAD + title_h + 3.0),
        galley,
        theme::TEXT_DIM,
    );
    if let Some(left) = left {
        let bar = Rect::from_min_size(
            Pos2::new(rect.left() + 1.0, rect.bottom() - 3.0),
            Vec2::new((rect.width() - 2.0) * left, 2.0),
        );
        ui.painter()
            .rect_filled(bar, 1.0, a(severity_color(severity), 0.6));
    }

    let x_rect = Rect::from_center_size(
        Pos2::new(rect.right() - 20.0, rect.top() + 20.0),
        Vec2::splat(24.0),
    );
    let resp = super::glyph_button_at(
        ui,
        x_rect,
        ui.scope_id().with(("toast_x", idx, note.timestamp_ms)),
        super::paint_close_glyph,
    );

    let mut action = ToastAction {
        dismiss: resp.clicked(),
        open: body.clicked() && !resp.clicked(),
        ..Default::default()
    };
    if let Some(button) = toast_button(&note.code) {
        let label = match button {
            ToastButton::Details => t!("plugins.issue_details"),
            ToastButton::Open => t!("shell.update_open_settings"),
        };
        let w = ui
            .painter()
            .layout_no_wrap(label.to_string(), theme::body_sm(), theme::TEXT)
            .size()
            .x
            + 24.0;
        let btn = Rect::from_min_size(
            Pos2::new(rect.left() + TEXT_X, rect.bottom() - PAD - 30.0),
            Vec2::new(w, 30.0),
        );
        let clicked = super::button_at(
            ui,
            btn,
            ui.scope_id().with(("toast_action", idx, note.timestamp_ms)),
            &label,
            super::ButtonKind::Secondary,
        )
        .clicked();
        match button {
            ToastButton::Details => action.details = clicked,
            ToastButton::Open => action.open |= clicked,
        }
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
            show_native: false,
            timestamp_ms: 0,
        }
    }

    fn plugin_error() -> Notification {
        use halod_shared::types::NotificationCode;
        Notification {
            code: NotificationCode::PluginRuntimeError {
                plugin: "wled".parse().unwrap(),
                detail: "boom".into(),
            },
            show_native: false,
            timestamp_ms: 0,
        }
    }

    fn device_write_error() -> Notification {
        Notification {
            code: NotificationCode::DeviceWriteFailed {
                device_id: "kb-1".parse().unwrap(),
                device: "keyboard".into(),
                detail: "HID++ rejected write".into(),
            },
            show_native: false,
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
            assert_eq!(toast_button(&n.code), Some(ToastButton::Details));
            assert_eq!(
                toast_timeout(&n.code),
                None,
                "detail toasts stay until dismissed"
            );
            assert_eq!(action_row_height(&n), ACTION_ROW_H);
        }
        // A no-detail warning is neither sticky nor gets a Details row.
        let w = note(NotificationSeverity::Warning);
        assert_eq!(toast_button(&w.code), None);
        assert!(toast_timeout(&w.code).is_some());
        assert_eq!(action_row_height(&w), 0.0);
    }

    #[test]
    fn update_toast_offers_open_settings_and_times_out() {
        let code = NotificationCode::UpdateAvailable {
            version: "0.14.0".into(),
        };
        assert_eq!(toast_button(&code), Some(ToastButton::Open));
        assert!(toast_timeout(&code).is_some());
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
    fn opening_a_notification_emits_open_once_and_closes_the_panel() {
        let ctx = egui::Context::default();
        let mut toasts = Toasts {
            panel_open: true,
            ..Default::default()
        };
        toasts.open(note(NotificationSeverity::Info));
        assert!(!toasts.panel_open);
        let mut events = Vec::new();
        for _ in 0..2 {
            crate::ui::run_headless(&ctx, egui::RawInput::default(), |ui| {
                events.push(
                    toasts
                        .show(ui.ctx())
                        .map(|event| matches!(event, ToastEvent::Open(_))),
                );
            });
        }
        assert_eq!(events, [Some(true), None]);
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
                show_native: false,
                timestamp_ms: i as u64,
            })
            .collect();
        toasts.ingest(many, 0.0);
        assert_eq!(toasts.items.len(), MAX_TOASTS);
        // Oldest (timestamps 0..2) were dropped; newest kept.
        assert_eq!(toasts.items[0].note.timestamp_ms, 3);
    }

    #[test]
    fn history_keeps_newest_first_and_tracks_unread() {
        let mut toasts = Toasts::default();
        toasts.ingest([note(NotificationSeverity::Info)], 1.0);
        toasts.ingest([note(NotificationSeverity::Error)], 2.0);
        let received: Vec<f64> = toasts.history().map(|l| l.received).collect();
        assert_eq!(received, vec![2.0, 1.0]);
        assert_eq!(toasts.unread(), 2);
        toasts.mark_all_read();
        assert_eq!(toasts.unread(), 0);
        toasts.clear_history();
        assert_eq!(toasts.history().count(), 0);
    }

    #[test]
    fn long_titles_wrap_inside_the_card() {
        const W: f32 = 380.0;
        let ctx = egui::Context::default();
        let long = Notification {
            code: NotificationCode::PluginUpdateNeedsPermissions {
                plugin: "nuvoton_lpcio".parse().unwrap(),
                name: "Nuvoton Super I/O Hardware Monitor".into(),
            },
            show_native: false,
            timestamp_ms: 0,
        };
        let short = note(NotificationSeverity::Info);
        crate::ui::run_headless(&ctx, egui::RawInput::default(), |ui| {
            assert!(title_galley(ui, &long, W).size().x <= text_width(W));
            assert!(title_galley(ui, &long, W).size().y > title_galley(ui, &short, W).size().y);
        });
    }

    #[test]
    fn history_is_bounded() {
        let mut toasts = Toasts::default();
        toasts.ingest(
            (0..MAX_HISTORY + 5).map(|_| note(NotificationSeverity::Info)),
            0.0,
        );
        assert_eq!(toasts.history().count(), MAX_HISTORY);
    }
}
