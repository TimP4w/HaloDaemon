// SPDX-License-Identifier: GPL-3.0-or-later
//! Application-level user-visible notification publishing.
//!
//! Two responsibilities, intentionally bundled in one helper:
//!   1. Log the event at the matching `log::` level.
//!   2. Publish a retained session event for native delivery and GUI toasts.
//!
//! The bounded bus event ring lets a reconnecting GUI replay notifications
//! emitted while it was closed or resident only in the tray.

use std::sync::Arc;

use crate::application::state::AppState;
use crate::util::time::now_ms;
use halod_shared::types::{Notification, NotificationCode, NotificationSeverity};

/// Emit a user-visible notification. The daemon carries only the stable code
/// and its runtime params; the GUI owns all human-readable copy. Severity is
/// derived from the code and used only for the daemon's own log line.
pub async fn send(app: &Arc<AppState>, code: NotificationCode) {
    match code.severity() {
        NotificationSeverity::Info => log::info!("{code:?}"),
        NotificationSeverity::Warning => log::warn!("{code:?}"),
        NotificationSeverity::Error => log::error!("{code:?}"),
    }
    let n = Notification {
        code,
        show_native: true,
        timestamp_ms: now_ms(),
    };
    app.data_bus
        .publish_event(halod_shared::bus::BusEventPayload::Notification(n));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    #[tokio::test]
    async fn send_publishes_typed_replayable_event() {
        let app = Arc::new(AppState::new(Config::default()));

        send(
            &app,
            NotificationCode::DeviceInitFailed {
                device: "Kraken".into(),
                detail: "thing exploded".into(),
            },
        )
        .await;

        let replay = app.data_bus.replay_events(None);
        let event = replay.events.last().expect("event was retained");
        let halod_shared::bus::BusEventPayload::Notification(notification) = &event.payload;
        assert!(notification.show_native);
        assert!(notification.timestamp_ms > 0);
        assert!(matches!(
            &notification.code,
            NotificationCode::DeviceInitFailed { device, detail }
                if device == "Kraken" && detail == "thing exploded"
        ));
    }

    #[tokio::test]
    async fn no_clients_means_no_panic() {
        let app = Arc::new(AppState::new(Config::default()));
        send(
            &app,
            NotificationCode::Generic {
                message: "noop".into(),
            },
        )
        .await;
    }
}
