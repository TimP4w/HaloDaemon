// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::Arc;

use super::persistence::ConfigSaveState;
use super::AppState;

const SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);
const SAVE_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

pub fn start_config_save_worker(app: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    let mut rx = app.config.persistence().save_tx.subscribe();
    let mut shutdown_rx = app.config.persistence().shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut saved_version = *rx.borrow();
        loop {
            tokio::select! {
                changed = rx.changed() => if changed.is_err() { break },
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() { break; }
                    continue;
                }
            }
            app.config
                .persistence()
                .save_state
                .send_replace(ConfigSaveState::Debouncing);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(SAVE_DEBOUNCE) => break,
                    result = rx.changed() => if result.is_err() { return },
                    result = shutdown_rx.changed() => {
                        if result.is_err() || *shutdown_rx.borrow() { break; }
                    }
                }
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            if *shutdown_rx.borrow() {
                break;
            }

            loop {
                let version = *rx.borrow_and_update();
                app.config
                    .persistence()
                    .save_state
                    .send_replace(ConfigSaveState::Saving(version));
                let cfg = app.config.read().await.clone();
                let result = tokio::task::spawn_blocking(move || crate::config::save(&cfg)).await;
                match result {
                    Ok(Ok(())) => {
                        saved_version = version;
                        if *rx.borrow() == saved_version {
                            app.config
                                .persistence()
                                .save_state
                                .send_replace(ConfigSaveState::Clean);
                            break;
                        }
                        app.config
                            .persistence()
                            .save_state
                            .send_replace(ConfigSaveState::DirtyWhileSaving);
                    }
                    Ok(Err(error)) => {
                        log::warn!("[Config] Save failed, retrying: {error}");
                        app.config
                            .persistence()
                            .save_state
                            .send_replace(ConfigSaveState::Failed(error.to_string()));
                        tokio::select! {
                            _ = tokio::time::sleep(SAVE_RETRY) => {},
                            _ = rx.changed() => {},
                            _ = shutdown_rx.changed() => {},
                        }
                    }
                    Err(error) => {
                        log::warn!("[Config] Save worker panicked, retrying: {error}");
                        app.config
                            .persistence()
                            .save_state
                            .send_replace(ConfigSaveState::Failed(error.to_string()));
                        tokio::time::sleep(SAVE_RETRY).await;
                    }
                }
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }

        app.config
            .persistence()
            .save_state
            .send_replace(ConfigSaveState::Stopping);
        let latest = *rx.borrow();
        if latest != saved_version {
            let cfg = app.config.read().await.clone();
            match tokio::task::spawn_blocking(move || crate::config::save(&cfg)).await {
                Ok(Ok(())) => {
                    app.config
                        .persistence()
                        .save_state
                        .send_replace(ConfigSaveState::Clean);
                }
                Ok(Err(error)) => {
                    app.config
                        .persistence()
                        .save_state
                        .send_replace(ConfigSaveState::Failed(error.to_string()));
                }
                Err(error) => {
                    app.config
                        .persistence()
                        .save_state
                        .send_replace(ConfigSaveState::Failed(error.to_string()));
                }
            }
        } else {
            app.config
                .persistence()
                .save_state
                .send_replace(ConfigSaveState::Clean);
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
            app.config.persistence().notify.notified().await;
            let devices = app.device_registry.read().await.clone();
            for device in &devices {
                crate::profiles::device_state::persist_device_state(&app, device.as_ref()).await;
            }
        }
    })
}

pub async fn shutdown(app: Arc<AppState>) {
    log::info!("Gracefully shutting down...");
    let devices = app.device_registry.read().await.clone();
    for device in devices {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), device.close()).await;
    }
}
