// SPDX-License-Identifier: GPL-3.0-or-later
//! Records observed device telemetry as authoritative retained state.

use crate::domain::events::ChangeSink as _;

use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::device::Device;
use halod_shared::types::Sensor;

const DEVICE_SAMPLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Observe cached hardware samples and update the host-owned dynamic sensor
/// records. Every sampled device is returned so its complete observed
/// projection (telemetry, controls, throughput, etc.) can be checked for a
/// change; the coordinator suppresses identical records.
pub(crate) async fn observe(app: &AppState) -> std::collections::HashSet<String> {
    let known = app.config.read().await.known_devices.clone();
    let devices = app.device_registry.read().await.clone();
    let eligible: Vec<_> = devices
        .into_iter()
        .filter(|device| {
            let disabled = known.get(device.id()).is_some_and(|record| {
                record.active_state == halod_shared::types::VisibilityState::Disabled
            });
            !disabled && device.is_live()
        })
        .collect();
    let sampled_ids = eligible
        .iter()
        .map(|device| device.id().to_owned())
        .collect::<std::collections::HashSet<_>>();

    // A slow or wedged device must not hold every device behind it. Calls for
    // one device remain sequential (important for shared HID transports), while
    // independent devices are sampled concurrently with a modest bound.
    let mut tasks = tokio::task::JoinSet::new();
    let mut sensors = Vec::new();
    for device in eligible {
        if tasks.len() >= 8 {
            if let Some(Ok(sampled)) = tasks.join_next().await {
                sensors.extend(sampled);
            }
        }
        tasks.spawn(sample_device(device));
    }
    while let Some(result) = tasks.join_next().await {
        if let Ok(sampled) = result {
            sensors.extend(sampled);
        }
    }

    let mut affected = app.data_bus.replace_host_sensors(sensors);
    affected.extend(sampled_ids);
    affected
}

async fn sample_device(device: Arc<dyn Device>) -> Vec<(String, Sensor)> {
    let mut sensors = Vec::new();
    if let Some(capability) = device.as_sensor_capability() {
        match tokio::time::timeout(DEVICE_SAMPLE_TIMEOUT, capability.get_sensors()).await {
            Ok(Ok(readings)) => {
                for sensor in readings {
                    sensors.push((device.id().to_owned(), sensor));
                }
            }
            Ok(Err(error)) => log::debug!("sensor read failed for {}: {error:#}", device.id()),
            Err(_) => log::warn!("sensor read timed out for {}", device.id()),
        }
    }
    match tokio::time::timeout(
        DEVICE_SAMPLE_TIMEOUT,
        crate::domain::device::fan_sensors(device.as_ref()),
    )
    .await
    {
        Ok(readings) => {
            for sensor in readings {
                sensors.push((device.id().to_owned(), sensor));
            }
        }
        Err(_) => log::warn!("fan sensor read timed out for {}", device.id()),
    }
    sensors
}

/// Refresh once and commit device records whose complete observed projection changed.
pub async fn refresh(app: &Arc<AppState>) {
    let changed = observe(app).await;
    if !changed.is_empty() {
        app.record_change(crate::domain::events::Change::SensorTelemetry(
            changed.into_iter().collect(),
        ))
        .await;
    }
}

/// Continuously refresh cached telemetry at a bounded cadence.
pub async fn run(app: Arc<AppState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        refresh(&app).await;
    }
}
