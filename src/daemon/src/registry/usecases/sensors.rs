// SPDX-License-Identifier: GPL-3.0-or-later
//! Observe cached device sensor samples and publish their effective device records.

use std::sync::Arc;

use crate::state::AppState;

/// Observe cached hardware samples and update the host-owned dynamic sensor
/// records. The returned ids identify effective device records that changed.
pub(crate) async fn observe(app: &AppState) -> std::collections::HashSet<String> {
    let known = app.config.read().await.known_devices.clone();
    let visibility = app.config.read().await.sensor_visibility.clone();
    let devices = app.device_registry.read().await.clone();
    let mut sensors = Vec::new();
    for device in &devices {
        let disabled = known.get(device.id()).is_some_and(|record| {
            record.active_state == halod_shared::types::VisibilityState::Disabled
        });
        if disabled || !device.is_live() {
            continue;
        }
        if let Some(capability) = device.as_sensor_capability() {
            if let Ok(readings) = capability.get_sensors().await {
                for mut sensor in readings {
                    if let Some(state) = visibility.get(&sensor.id) {
                        sensor.visibility = state.clone();
                    }
                    sensors.push((device.id().to_owned(), sensor));
                }
            }
        }
        for mut sensor in crate::drivers::fan_sensors(device.as_ref()).await {
            if let Some(state) = visibility.get(&sensor.id) {
                sensor.visibility = state.clone();
            }
            sensors.push((device.id().to_owned(), sensor));
        }
    }

    app.data_bus.replace_host_sensors(sensors)
}

/// Refresh once and commit only device records whose effective telemetry changed.
pub async fn refresh(app: &Arc<AppState>) {
    let changed = observe(app).await;
    if !changed.is_empty() {
        app.record_change(crate::services::effective_state::Change::SensorTelemetry(
            changed.into_iter().collect(),
        ))
        .await;
    }
}

/// Run the bounded-cadence sensor observation use case continuously.
pub async fn run(app: Arc<AppState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        refresh(&app).await;
    }
}
