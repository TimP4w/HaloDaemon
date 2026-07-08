use std::sync::Arc;

use super::AppState;

pub fn start_config_save_worker(app: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    let mut rx = app.persistence.save_tx.subscribe();
    tokio::spawn(async move {
        loop {
            if rx.changed().await.is_err() {
                break;
            }
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(250);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    result = rx.changed() => {
                        if result.is_err() { return; }
                    }
                }
            }
            let cfg = app.config.read().await.clone();
            let result = tokio::task::spawn_blocking(move || crate::config::save(&cfg)).await;
            match result {
                Err(e) => {
                    log::warn!("[Config] Save worker panicked: {e}");
                    // Re-queue the save so the pending change isn't lost
                    // just because the blocker thread panicked.
                    let _ = app.persistence.save_tx.send(());
                }
                Ok(Err(e)) => {
                    log::warn!("[Config] Save failed, will retry on next change: {e}");
                }
                Ok(Ok(())) => {}
            }
        }
    })
}

/// Spawn a background task that calls
/// [`crate::profiles::device_state::persist_device_state`] for every
/// registered device whenever an engine signals via
/// [`Persistence::notify`](super::Persistence). This keeps engines decoupled
/// from the usecase layer.
pub fn start_persist_worker(app: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            app.persistence.notify.notified().await;
            let devices = app.devices.read().await.clone();
            for device in &devices {
                crate::profiles::device_state::persist_device_state(&app, device.as_ref()).await;
            }
        }
    })
}

pub async fn shutdown(app: Arc<AppState>) {
    log::info!("Gracefully shutting down...");
    let devices = app.devices.read().await.clone();
    for device in devices {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), device.close()).await;
    }
}
