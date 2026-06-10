use anyhow::{anyhow, Result};
use std::sync::Arc;

use crate::ipc::broadcast_state;
use crate::state::AppState;
use crate::usecases::{ensure_record, persist_device_state};
use halod_protocol::types::VisibilityState;

pub async fn set_device_visibility(
    device_id: String,
    new_state: VisibilityState,
    app: Arc<AppState>,
) -> Result<()> {
    let device = {
        let devices = app.devices.lock().await;
        devices.iter().find(|d| d.id() == device_id).cloned()
    };

    // Capture the previous state before mutating anything.
    let prev_state = device.as_ref().map(|d| d.active_state());

    if let Some(device) = &device {
        // Clear engine participation before updating active_state, while the
        // capability gates still return Some (they check active_state == Visible).
        //
        // Trade-off: there is a narrow crash window between slot clearing below
        // and the config save at the end of this function. If the process dies
        // mid-way, engine slots are empty but config still reads Visible. On
        // restart the device is live but has no engine assignments — safe and
        // recoverable, but the user may see blank settings until they re-assign.
        // A full two-phase write is not worth the complexity for this edge case.
        if new_state != VisibilityState::Visible {
            if let Some(s) = device.as_rgb() {
                s.set_canvas_zones(vec![]);
            }
            if let Some(s) = device.as_fan() {
                s.clear_fan_curve();
            }
            if let Some(s) = device.as_lcd() {
                s.set_lcd_template_id(None);
            }
        }

        if new_state == VisibilityState::Disabled {
            device.close().await;
        }

        device.set_active_state(new_state.clone());

        // Persist device state for Visible/Hidden transitions only — a disabled
        // device has no live hardware state worth saving.
        if new_state != VisibilityState::Disabled {
            persist_device_state(&app, device.as_ref()).await;
        }
    }

    // Write the new active_state to the config record and save.
    {
        let mut cfg = app.config.write().await;
        let record = ensure_record(&mut cfg.known_devices, &device_id, device.as_deref());
        record.active_state = new_state.clone();
        let _snap = cfg.clone();
        drop(cfg);
        #[cfg(not(test))]
        app.request_config_save(_snap);
    }

    // Re-enabling a previously-disabled device requires a full rediscover so the
    // hardware gets a fresh initialize() call. Remove the stale closed object first
    // (unique ID guarantees no false removals), then let discover_devices add a
    // fresh initialized entry. discover_devices broadcasts state at the end.
    let enabling_from_disabled = new_state == VisibilityState::Visible
        && prev_state == Some(VisibilityState::Disabled);
    if enabling_from_disabled {
        app.devices.lock().await.retain(|d| d.id() != device_id);
        crate::discovery::discover_devices(app).await;
        return Ok(());
    }

    broadcast_state(app).await;
    Ok(())
}

pub async fn set_sensor_visibility(
    sensor_id: String,
    state: VisibilityState,
    app: Arc<AppState>,
) -> Result<()> {
    let owning_device = {
        let devices = app.devices.lock().await.clone();
        let mut found: Option<Arc<dyn crate::drivers::Device>> = None;
        for device in &devices {
            if let Some(cap) = device.as_sensor_capability() {
                if let Ok(sensors) = cap.get_sensors().await {
                    if sensors.iter().any(|s| s.id == sensor_id) {
                        found = Some(device.clone());
                        break;
                    }
                }
            }
        }
        found
    };

    let device = owning_device.ok_or_else(|| anyhow!("sensor not found: {sensor_id}"))?;
    device.set_sensor_visibility(&sensor_id, state.clone());

    {
        let mut cfg = app.config.write().await;
        if state == VisibilityState::Visible {
            cfg.sensor_visibility.remove(&sensor_id);
        } else {
            cfg.sensor_visibility.insert(sensor_id, state);
        }
        let _snap = cfg.clone();
        drop(cfg);
        #[cfg(not(test))]
        app.request_config_save(_snap);
    }

    broadcast_state(app).await;
    Ok(())
}
