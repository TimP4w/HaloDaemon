// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::Arc;

use super::persistence::ConfigSaveState;
use super::AppState;

const SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);
const SAVE_MAX_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);
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
            let debounce_deadline = tokio::time::Instant::now() + SAVE_MAX_DEBOUNCE;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(SAVE_DEBOUNCE) => break,
                    _ = tokio::time::sleep_until(debounce_deadline) => break,
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
/// [`crate::domain::profiles::device_state::persist_device_state`] for every
/// registered device whenever an engine signals via
/// [`Persistence::notify`](super::Persistence). This keeps engines decoupled
/// from the usecase layer.
pub fn start_persist_worker(app: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    let mut shutdown_rx = app.config.persistence().shutdown_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = app.config.persistence().notify.notified() => {}
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() { break; }
                    continue;
                }
            }
            let devices = app.device_registry.read().await.clone();
            for device in &devices {
                crate::domain::profiles::device_state::persist_device_state(&app, device.as_ref())
                    .await;
            }
            // Engine-originated persistence does not pass through a command
            // use case, so explicitly refresh the profile projection here.
            // The publisher suppresses the transaction when no profile state
            // actually changed.
            app.effective_state
                .record(&app, crate::domain::events::Change::Profiles)
                .await;
        }
    })
}

fn order_devices_for_shutdown(
    devices: &mut [Arc<dyn crate::domain::device::Device>],
    child_ids: &std::collections::HashSet<String>,
) {
    devices.sort_by_key(|device| !child_ids.contains(device.id()));
}

pub async fn shutdown(app: Arc<AppState>) {
    log::info!("Gracefully shutting down...");
    if let Some(video) = app.lcd.video() {
        video.stop_all().await;
    }
    let mut devices = app.device_registry.read().await.clone();
    let child_ids: std::collections::HashSet<String> = app
        .device_registry
        .children
        .lock()
        .await
        .values()
        .flatten()
        .cloned()
        .collect();
    // Dynamic children share their root's Lua worker. Match scoped-unload
    // ordering so root close cannot terminate the worker first.
    order_devices_for_shutdown(&mut devices, &child_ids);
    for device in devices {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), device.close()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        domain::{device::Device, events::Change},
        test_support::MockDevice,
    };

    #[tokio::test]
    async fn persist_worker_publishes_engine_originated_profile_override() {
        let device = Arc::new(MockDevice::new("dev1").with_choice());
        device.choice.as_ref().unwrap().record("nc_mode", 0);
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .write()
            .await
            .push(device.clone() as Arc<dyn Device>);
        {
            let mut config = app.config.write().await;
            config.active_profile_data_mut().device_states.insert(
                "dev1".into(),
                serde_json::json!({ "choice": { "nc_mode": 0 } }),
            );
            config.profiles.insert("Gaming".into(), Default::default());
            config.active_profile = "Gaming".into();
        }

        // Seed the bus with the pre-mutation projection so the transaction
        // below can only be caused by the newly persisted override.
        app.effective_state.record(&app, Change::Profiles).await;
        let mut transactions = app.data_bus.subscribe_transactions();
        let worker = start_persist_worker(app.clone());

        device.choice.as_ref().unwrap().record("nc_mode", 2);
        app.config.persistence().notify.notify_one();

        let transaction =
            tokio::time::timeout(std::time::Duration::from_secs(1), transactions.recv())
                .await
                .expect("persist worker must publish the changed profile projection")
                .unwrap();
        let profiles = transaction
            .upserts
            .into_iter()
            .find_map(|record| match record.value {
                halod_shared::bus::BusValue::Profiles(profiles) => Some(profiles),
                _ => None,
            })
            .expect("engine-originated persistence must publish profile overrides");
        assert_eq!(
            profiles.overrides.device_capabilities["dev1"],
            vec!["choice".to_string()]
        );

        app.config.persistence().shutdown_tx.send_replace(true);
        worker.await.unwrap();
    }

    #[test]
    fn shutdown_orders_shared_worker_children_before_roots() {
        let root = Arc::new(MockDevice::new("root")) as Arc<dyn Device>;
        let child = Arc::new(MockDevice::new("child")) as Arc<dyn Device>;
        let independent = Arc::new(MockDevice::new("independent")) as Arc<dyn Device>;
        let mut devices = vec![root, independent, child];
        let children = std::collections::HashSet::from(["child".to_owned()]);

        order_devices_for_shutdown(&mut devices, &children);

        assert_eq!(devices[0].id(), "child");
        assert!(devices[1..].iter().any(|device| device.id() == "root"));
    }
}
