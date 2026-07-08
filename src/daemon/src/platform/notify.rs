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

use serde_json::json;

use crate::state::AppState;
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
        timestamp_ms: now_ms(),
    };
    let msg = json!({ "type": "notification", "data": n });
    let clients = app.clients.lock().await;
    for client in clients.iter() {
        client.send_json(&msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tokio::sync::mpsc;

    async fn register_test_client(app: &Arc<AppState>) -> mpsc::Receiver<std::sync::Arc<Vec<u8>>> {
        let (tx, rx) = mpsc::channel::<std::sync::Arc<Vec<u8>>>(16);
        app.clients.lock().await.push(crate::ipc::ClientHandle {
            id: 0,
            tx,
            subs: std::sync::Arc::default(),
        });
        rx
    }

    fn decode_json_frame(frame: &[u8]) -> serde_json::Value {
        // Frame layout: 1 byte type + 4 byte BE length + payload.
        assert!(frame.len() >= 5);
        serde_json::from_slice(&frame[5..]).expect("valid json payload")
    }

    #[tokio::test]
    async fn send_broadcasts_flattened_code_frame() {
        let app = Arc::new(AppState::new(Config::default()));
        let mut rx = register_test_client(&app).await;

        send(
            &app,
            NotificationCode::DeviceInitFailed {
                device: "Kraken".into(),
                detail: "thing exploded".into(),
            },
        )
        .await;

        let frame = rx.try_recv().expect("frame was pushed");
        let msg = decode_json_frame(&frame);
        assert_eq!(msg["type"], "notification");
        // Code tag + params flatten into `data`; no human-readable prose crosses.
        assert_eq!(msg["data"]["code"], "device_init_failed");
        assert_eq!(msg["data"]["device"], "Kraken");
        assert_eq!(msg["data"]["detail"], "thing exploded");
        assert!(msg["data"]["timestamp_ms"].as_u64().is_some());
        assert!(msg["data"]["title"].is_null());
        assert!(msg["data"]["message"].is_null());
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
