// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{anyhow, Result};
use std::sync::Arc;

use crate::ipc::{broadcast_delta, Domain};
use crate::profiles::device_state::persist_device_state;
use crate::registry::config::ensure_record;
use crate::state::AppState;
use halod_shared::types::VisibilityState;

pub async fn set_device_visibility(
    device_id: String,
    new_state: VisibilityState,
    app: Arc<AppState>,
) -> Result<()> {
    let device = {
        let devices = app.devices.read().await;
        devices.iter().find(|d| d.id() == device_id).cloned()
    };

    // Capture the previous state before mutating anything.
    let prev_state = device.as_ref().map(|d| d.active_state());

    if let Some(device) = &device {
        // Gate engines and command lookups before awaiting close, so no new
        // hardware work can start during the teardown window.
        if new_state == VisibilityState::Disabled {
            device.set_active_state(VisibilityState::Disabled);
        }

        // Clear slots before state change (gates check active_state == Visible).
        if new_state != VisibilityState::Visible {
            crate::registry::usecases::registration::clear_engine_slots(device);
        }

        if new_state == VisibilityState::Disabled {
            super::registration::close_device(&app, device).await;
        }

        if new_state != VisibilityState::Disabled {
            device.set_active_state(new_state.clone());
        }

        // Persist device state for Visible/Hidden transitions only — a disabled
        // device has no live hardware state worth saving.
        if new_state != VisibilityState::Disabled {
            persist_device_state(&app, device.as_ref()).await;
        }
    }

    {
        let mut cfg = app.config.write().await;
        let record = ensure_record(&mut cfg.known_devices, &device_id, device.as_deref());
        record.active_state = new_state.clone();
        drop(cfg);
        app.request_config_save();
    }

    // Re-discover to get a fresh initialize()
    let enabling_from_disabled =
        new_state == VisibilityState::Visible && prev_state == Some(VisibilityState::Disabled);
    if enabling_from_disabled {
        app.devices.write().await.retain(|d| d.id() != device_id);
        crate::plugin::usecases::plugins::reconcile_full(&app).await;
        return Ok(());
    }

    broadcast_delta(&app, &[Domain::Devices]).await;
    Ok(())
}

pub async fn set_sensor_visibility(
    sensor_id: String,
    state: VisibilityState,
    app: Arc<AppState>,
) -> Result<()> {
    app.data_bus
        .sensor_owner(&sensor_id)
        .ok_or_else(|| anyhow!("sensor not found: {sensor_id}"))?;

    {
        let mut cfg = app.config.write().await;
        if state == VisibilityState::Visible {
            cfg.sensor_visibility.remove(&sensor_id);
        } else {
            cfg.sensor_visibility.insert(sensor_id, state);
        }
        drop(cfg);
        app.request_config_save();
    }

    broadcast_delta(&app, &[Domain::Devices]).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::test_support::MockDevice;
    use halod_shared::types::VisibilityState;
    use std::sync::Arc;

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    fn push_device(app: &Arc<AppState>, device: Arc<dyn crate::drivers::Device>) {
        app.devices.try_write().unwrap().push(device);
    }

    #[tokio::test]
    async fn set_device_visibility_to_hidden_updates_config_record() {
        let app = make_app();
        let device = Arc::new(MockDevice::new("dev1").with_rgb().with_fan());
        push_device(&app, device.clone());

        set_device_visibility("dev1".into(), VisibilityState::Hidden, app.clone())
            .await
            .unwrap();

        let cfg = app.config.read().await;
        let record = cfg.known_devices.get("dev1").unwrap();
        assert_eq!(record.active_state, VisibilityState::Hidden);
    }

    #[tokio::test]
    async fn set_device_visibility_to_hidden_clears_engine_slots() {
        let app = make_app();
        let device = Arc::new(MockDevice::new("dev1").with_rgb().with_fan());
        if let Some(cooling) = (device.as_ref() as &dyn crate::drivers::Device).as_cooling() {
            cooling.set_curve(
                "default".to_string(),
                crate::cooling::config::FanCurveRecord {
                    sensor_id: None,
                    points: vec![(30.0, 50.0), (80.0, 100.0)],
                },
            );
        }
        push_device(&app, device.clone());

        set_device_visibility("dev1".into(), VisibilityState::Hidden, app.clone())
            .await
            .unwrap();

        assert!(device.fan.as_ref().unwrap().curve("default").is_none());
    }

    #[tokio::test]
    async fn recovers_from_crash_window_between_slot_clear_and_config_save() {
        // Simulates the documented crash window: engine slots were cleared (as
        // if a Hidden transition ran) but the config record still reads Visible
        // (as if the process died before the save completed). A later action
        // on the device must not panic, and re-registering it must work.
        let app = make_app();
        let device = Arc::new(MockDevice::new("dev1").with_rgb().with_fan());
        device.fan.as_ref().unwrap().set_curve(
            "default".to_string(),
            crate::cooling::config::FanCurveRecord {
                sensor_id: None,
                points: vec![(30.0, 50.0), (80.0, 100.0)],
            },
        );
        push_device(&app, device.clone());
        crate::registry::usecases::registration::clear_engine_slots(
            &(device.clone() as Arc<dyn crate::drivers::Device>),
        );
        app.config.write().await.known_devices.insert(
            "dev1".into(),
            crate::registry::config::DeviceRecord {
                name: String::new(),
                vendor: String::new(),
                model: String::new(),
                active_state: VisibilityState::Visible,
            },
        );

        assert!(device.fan.as_ref().unwrap().curve("default").is_none());

        set_device_visibility("dev1".into(), VisibilityState::Hidden, app.clone())
            .await
            .unwrap();
        let cfg = app.config.read().await;
        assert_eq!(
            cfg.known_devices.get("dev1").unwrap().active_state,
            VisibilityState::Hidden
        );
    }

    #[tokio::test]
    async fn set_device_visibility_for_unknown_device_still_writes_config() {
        let app = make_app();

        set_device_visibility("ghost".into(), VisibilityState::Hidden, app.clone())
            .await
            .unwrap();

        let cfg = app.config.read().await;
        let record = cfg.known_devices.get("ghost").unwrap();
        assert_eq!(record.active_state, VisibilityState::Hidden);
    }

    #[tokio::test]
    async fn set_sensor_visibility_accepts_synthesized_fan_sensor_id() {
        let app = make_app();
        let device = Arc::new(MockDevice::new("fan0").with_fan());
        push_device(&app, device.clone());
        app.refresh_sensor_bus().await;

        set_sensor_visibility(
            "cooling_fan0_default_duty".into(),
            VisibilityState::Hidden,
            app.clone(),
        )
        .await
        .unwrap();

        let cfg = app.config.read().await;
        assert_eq!(
            cfg.sensor_visibility.get("cooling_fan0_default_duty"),
            Some(&VisibilityState::Hidden)
        );
    }

    #[tokio::test]
    async fn set_sensor_visibility_returns_error_for_unknown_sensor() {
        let app = make_app();
        let err = set_sensor_visibility("no_such_sensor".into(), VisibilityState::Hidden, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("sensor not found"));
    }

    #[tokio::test]
    async fn set_sensor_visibility_hidden_adds_to_config() {
        let app = make_app();
        let device = std::sync::Arc::new(MockDevice::new("dev1").with_sensor(vec![
            halod_shared::types::Sensor {
                id: "temp1".into(),
                name: "CPU".into(),
                value: 45.0,
                unit: halod_shared::types::SensorUnit::Celsius,
                sensor_type: halod_shared::types::SensorType::Temperature,
                visibility: VisibilityState::Visible,
            },
        ]));
        push_device(&app, device);
        app.refresh_sensor_bus().await;

        set_sensor_visibility("temp1".into(), VisibilityState::Hidden, app.clone())
            .await
            .unwrap();

        let cfg = app.config.read().await;
        assert_eq!(
            cfg.sensor_visibility.get("temp1"),
            Some(&VisibilityState::Hidden)
        );
    }

    #[tokio::test]
    async fn set_sensor_visibility_visible_removes_from_config() {
        let app = make_app();
        let device = std::sync::Arc::new(MockDevice::new("dev1").with_sensor(vec![
            halod_shared::types::Sensor {
                id: "temp1".into(),
                name: "CPU".into(),
                value: 45.0,
                unit: halod_shared::types::SensorUnit::Celsius,
                sensor_type: halod_shared::types::SensorType::Temperature,
                visibility: VisibilityState::Hidden,
            },
        ]));
        push_device(&app, device);
        app.refresh_sensor_bus().await;

        set_sensor_visibility("temp1".into(), VisibilityState::Hidden, app.clone())
            .await
            .unwrap();
        assert!(app
            .config
            .read()
            .await
            .sensor_visibility
            .contains_key("temp1"));

        set_sensor_visibility("temp1".into(), VisibilityState::Visible, app.clone())
            .await
            .unwrap();

        assert!(!app
            .config
            .read()
            .await
            .sensor_visibility
            .contains_key("temp1"));
    }
}
