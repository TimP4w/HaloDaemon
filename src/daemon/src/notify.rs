//! User-visible notifications.
//!
//! Two responsibilities, intentionally bundled in one helper:
//!   1. Log the event at the matching `log::` level (so it still lands in the
//!      log buffer the UI streams via state broadcasts).
//!   2. Push a `{"type":"notification", ...}` JSON frame to every connected
//!      client so the UI can render a toast.
//!
//! Notifications are not buffered daemon-side. A client that connects after a
//! notification fires will not see it — historical context lives in
//! `log_entries` (already broadcast every 250 ms in the state frame).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::state::AppState;
use halod_protocol::types::{Notification, NotificationSeverity};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn broadcast(app: &Arc<AppState>, n: Notification) {
    let msg = json!({ "type": "notification", "data": n });
    let clients = app.clients.lock().await;
    for client in clients.iter() {
        client.send_json(&msg);
    }
}

/// Emit a user-visible info notification.
#[allow(dead_code)]
pub async fn info(app: &Arc<AppState>, title: impl Into<String>, message: impl Into<String>) {
    let n = Notification {
        severity: NotificationSeverity::Info,
        title: title.into(),
        message: message.into(),
        timestamp_ms: now_ms(),
    };
    log::info!("[{}] {}", n.title, n.message);
    broadcast(app, n).await;
}

/// Emit a user-visible warning notification.
pub async fn warn(app: &Arc<AppState>, title: impl Into<String>, message: impl Into<String>) {
    let n = Notification {
        severity: NotificationSeverity::Warning,
        title: title.into(),
        message: message.into(),
        timestamp_ms: now_ms(),
    };
    log::warn!("[{}] {}", n.title, n.message);
    broadcast(app, n).await;
}

/// Emit a user-visible error notification.
pub async fn error(app: &Arc<AppState>, title: impl Into<String>, message: impl Into<String>) {
    let n = Notification {
        severity: NotificationSeverity::Error,
        title: title.into(),
        message: message.into(),
        timestamp_ms: now_ms(),
    };
    log::error!("[{}] {}", n.title, n.message);
    broadcast(app, n).await;
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tokio::sync::mpsc;

    async fn register_test_client(app: &Arc<AppState>) -> mpsc::UnboundedReceiver<Vec<u8>> {
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        app.clients
            .lock()
            .await
            .push(crate::ipc::ClientHandle { tx });
        rx
    }

    fn decode_json_frame(frame: &[u8]) -> serde_json::Value {
        // Frame layout: 1 byte type + 4 byte BE length + payload.
        assert!(frame.len() >= 5);
        serde_json::from_slice(&frame[5..]).expect("valid json payload")
    }

    #[tokio::test]
    async fn error_broadcasts_notification_frame() {
        let app = Arc::new(AppState::new(Config::default()));
        let mut rx = register_test_client(&app).await;

        error(&app, "Boom", "thing exploded").await;

        let frame = rx.try_recv().expect("frame was pushed");
        let msg = decode_json_frame(&frame);
        assert_eq!(msg["type"], "notification");
        assert_eq!(msg["data"]["severity"], "error");
        assert_eq!(msg["data"]["title"], "Boom");
        assert_eq!(msg["data"]["message"], "thing exploded");
        assert!(msg["data"]["timestamp_ms"].as_u64().is_some());
    }

    #[tokio::test]
    async fn warn_uses_warning_severity() {
        let app = Arc::new(AppState::new(Config::default()));
        let mut rx = register_test_client(&app).await;

        warn(&app, "Heads up", "something is fishy").await;

        let msg = decode_json_frame(&rx.try_recv().unwrap());
        assert_eq!(msg["data"]["severity"], "warning");
    }

    #[tokio::test]
    async fn info_uses_info_severity() {
        let app = Arc::new(AppState::new(Config::default()));
        let mut rx = register_test_client(&app).await;

        info(&app, "FYI", "thing happened").await;

        let msg = decode_json_frame(&rx.try_recv().unwrap());
        assert_eq!(msg["data"]["severity"], "info");
    }

    #[tokio::test]
    async fn no_clients_means_no_panic() {
        // Helpers must not crash when nobody is connected — the normal state
        // during early startup discovery, before the UI attaches.
        let app = Arc::new(AppState::new(Config::default()));
        error(&app, "no audience", "noop").await;
    }
}
