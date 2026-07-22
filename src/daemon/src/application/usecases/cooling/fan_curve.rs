// SPDX-License-Identifier: GPL-3.0-or-later
use crate::domain::events::ChangeSink as _;

use anyhow::{anyhow, Result};
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::cooling::engine::fan_curve::preset_curves;
use crate::domain::cooling::model::FanCurveRecord;
use crate::domain::profiles::device_state::persist_device_state;
use crate::domain::registry::require_device_owned_id;

pub async fn set_cooling_curve_points(
    device_id: String,
    channel_id: String,
    points: Vec<[f32; 2]>,
    sensor_id: Option<String>,
    app: Arc<AppState>,
) -> Result<()> {
    let points = points.into_iter().map(|p| (p[0], p[1])).collect();
    let record = FanCurveRecord { sensor_id, points };
    record.validate()?;
    require_temperature_sensor(&record.sensor_id, &app).await?;
    let device = require_device_owned_id(&device_id, &app).await?;
    let cooling = device
        .as_cooling()
        .ok_or_else(|| anyhow!("device does not support cooling: {device_id}"))?;
    let channel = cooling
        .cooling_channels()
        .iter()
        .find(|channel| channel.id == channel_id)
        .cloned()
        .ok_or_else(|| anyhow!("unknown cooling channel '{channel_id}' on device '{device_id}'"))?;
    anyhow::ensure!(
        channel.controllable,
        "cooling channel '{channel_id}' on device '{device_id}' is not controllable"
    );
    cooling.set_curve(channel_id, record);
    persist_device_state(&app, device.as_ref()).await;
    app.record_change(crate::domain::events::Change::CoolingDevice(device_id))
        .await;
    Ok(())
}

pub async fn set_cooling_curve_preset(
    device_id: String,
    channel_id: String,
    preset: String,
    sensor_id: Option<String>,
    app: Arc<AppState>,
) -> Result<()> {
    let points = preset_curves()
        .iter()
        .find(|p| p.id == preset)
        .ok_or_else(|| anyhow!("unknown preset: {preset}"))?
        .points
        .to_vec();
    let record = FanCurveRecord { sensor_id, points };
    record.validate()?;
    require_temperature_sensor(&record.sensor_id, &app).await?;
    set_cooling_curve_points(
        device_id,
        channel_id,
        record.points.iter().map(|&(t, d)| [t, d]).collect(),
        record.sensor_id,
        app,
    )
    .await
}

pub async fn remove_cooling_curve(
    device_id: String,
    channel_id: String,
    app: Arc<AppState>,
) -> Result<()> {
    let device = require_device_owned_id(&device_id, &app).await?;
    let cooling = device
        .as_cooling()
        .ok_or_else(|| anyhow!("device does not support cooling: {device_id}"))?;
    anyhow::ensure!(
        cooling
            .cooling_channels()
            .iter()
            .any(|channel| channel.id == channel_id),
        "unknown cooling channel '{channel_id}' on device '{device_id}'"
    );
    cooling.clear_curve(&channel_id);
    persist_device_state(&app, device.as_ref()).await;
    app.record_change(crate::domain::events::Change::CoolingDevice(device_id))
        .await;
    Ok(())
}

/// `Ok` unless `sensor_id` is `Some` and names something other than a
/// temperature sensor the system currently exposes.
async fn require_temperature_sensor(sensor_id: &Option<String>, app: &Arc<AppState>) -> Result<()> {
    let Some(id) = sensor_id else {
        return Ok(());
    };
    let sensors = app.data_bus.sensors();
    match sensors.get(id) {
        Some(s) if s.sensor_type == halod_shared::types::SensorType::Temperature => Ok(()),
        Some(_) => Err(anyhow!("sensor '{id}' is not a temperature source")),
        None => Err(anyhow!("unknown sensor '{id}'")),
    }
}

/// Test-only convenience wrapper for exercising the same record validation as
/// the command ingress path without fabricating a sensor assignment.
#[cfg(test)]
fn validate_points(points: &[(f32, f32)]) -> Result<()> {
    FanCurveRecord {
        sensor_id: None,
        points: points.to_vec(),
    }
    .validate()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::state::AppState;
    use crate::config::Config;
    use crate::test_support::MockDevice;

    use std::sync::Arc;

    fn fan_curve(device: &MockDevice) -> Option<crate::domain::cooling::model::FanCurveRecord> {
        device.fan.as_ref().unwrap().curve("default")
    }

    fn make_app_with_fan(id: &str) -> (Arc<AppState>, Arc<MockDevice>) {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let device = Arc::new(MockDevice::new(id).with_fan());
        {
            let mut devices = app.device_registry.try_write().unwrap();
            devices.push(device.clone() as Arc<dyn crate::domain::device::Device>);
        }
        (app, device)
    }

    fn sensor(id: &str, kind: halod_shared::types::SensorType) -> halod_shared::types::Sensor {
        halod_shared::types::Sensor {
            id: id.to_string(),
            name: id.to_string(),
            value: 40.0,
            unit: halod_shared::types::SensorUnit::Celsius,
            sensor_type: kind,
        }
    }

    fn make_app_with_fan_and_sensor(
        fan_id: &str,
        sensor_id: &str,
    ) -> (Arc<AppState>, Arc<MockDevice>) {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let reading = sensor(sensor_id, halod_shared::types::SensorType::Temperature);
        let device = Arc::new(
            MockDevice::new(fan_id)
                .with_fan()
                .with_sensor(vec![reading.clone()]),
        );
        {
            let mut devices = app.device_registry.try_write().unwrap();
            devices.push(device.clone() as Arc<dyn crate::domain::device::Device>);
        }
        app.data_bus
            .replace_host_sensors(vec![(fan_id.to_owned(), reading)]);
        (app, device)
    }

    #[tokio::test]
    async fn set_fan_curve_preset_stores_points() {
        let (app, device) = make_app_with_fan_and_sensor("fan_0", "sensor_0");
        let preset = preset_curves().iter().find(|p| p.id == "balanced").unwrap();
        set_cooling_curve_preset(
            "fan_0".into(),
            "default".into(),
            "balanced".into(),
            Some("sensor_0".into()),
            app,
        )
        .await
        .unwrap();
        let curve = fan_curve(&device).expect("curve should be set");
        assert_eq!(curve.sensor_id, Some("sensor_0".to_string()));
        assert_eq!(curve.points, preset.points.to_vec());
    }

    #[tokio::test]
    async fn set_fan_curve_points_stores_custom_points() {
        let (app, device) = make_app_with_fan("fan_0");
        set_cooling_curve_points(
            "fan_0".into(),
            "default".into(),
            vec![[30.0f32, 20.0], [80.0f32, 100.0]],
            None,
            app,
        )
        .await
        .unwrap();
        let curve = fan_curve(&device).expect("curve should be set");
        assert_eq!(curve.points, vec![(30.0f32, 20.0), (80.0f32, 100.0)]);
    }

    #[test]
    fn set_fan_curve_points_errors_on_non_ascending_temps() {
        let points: Vec<(f32, f32)> = vec![(80.0, 20.0), (30.0, 100.0)];
        let err = validate_points(&points).unwrap_err();
        assert!(err.to_string().contains("ascending"));
    }

    #[test]
    fn set_fan_curve_points_errors_on_single_point() {
        let points: Vec<(f32, f32)> = vec![(50.0, 75.0)];
        let err = validate_points(&points).unwrap_err();
        assert!(err.to_string().contains("at least 2"));
    }

    #[test]
    fn validate_points_errors_on_too_many_points() {
        let points: Vec<(f32, f32)> = (0..FanCurveRecord::MAX_POINTS as i32 + 1)
            .map(|i| (i as f32, 50.0))
            .collect();
        let err = validate_points(&points).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn set_fan_curve_points_accepts_owned_temperature_sensor() {
        let (app, device) = make_app_with_fan_and_sensor("fan_0", "cpu_temp");
        set_cooling_curve_points(
            "fan_0".into(),
            "default".into(),
            vec![[30.0f32, 20.0], [80.0f32, 100.0]],
            Some("cpu_temp".into()),
            app,
        )
        .await
        .unwrap();
        assert_eq!(
            fan_curve(&device).unwrap().sensor_id,
            Some("cpu_temp".to_string())
        );
    }

    #[tokio::test]
    async fn set_fan_curve_points_rejects_unknown_sensor() {
        let (app, _device) = make_app_with_fan("fan_0");
        let err = set_cooling_curve_points(
            "fan_0".into(),
            "default".into(),
            vec![[30.0f32, 20.0], [80.0f32, 100.0]],
            Some("ghost_sensor".into()),
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown sensor"), "{err}");
    }

    #[tokio::test]
    async fn set_fan_curve_points_rejects_non_temperature_sensor() {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let device = Arc::new(
            MockDevice::new("fan_0")
                .with_fan()
                .with_sensor(vec![sensor("load0", halod_shared::types::SensorType::Load)]),
        );
        {
            let mut devices = app.device_registry.try_write().unwrap();
            devices.push(device.clone() as Arc<dyn crate::domain::device::Device>);
        }
        crate::application::usecases::device::telemetry::observe(&app).await;
        let err = set_cooling_curve_points(
            "fan_0".into(),
            "default".into(),
            vec![[30.0f32, 20.0], [80.0f32, 100.0]],
            Some("load0".into()),
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not a temperature"), "{err}");
    }

    #[tokio::test]
    async fn remove_fan_curve_removes_entry() {
        let (app, device) = make_app_with_fan("fan_0");
        set_cooling_curve_points(
            "fan_0".into(),
            "default".into(),
            vec![[30.0f32, 20.0], [80.0f32, 100.0]],
            None,
            app.clone(),
        )
        .await
        .unwrap();
        assert!(fan_curve(&device).is_some());

        remove_cooling_curve("fan_0".into(), "default".into(), app)
            .await
            .unwrap();
        assert!(fan_curve(&device).is_none());
    }

    #[tokio::test]
    async fn remove_fan_curve_errors_on_missing_device() {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let err = remove_cooling_curve("nonexistent".into(), "default".into(), app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[tokio::test]
    async fn set_and_remove_fan_curve_error_on_unknown_channel() {
        let (app, _device) = make_app_with_fan("fan_0");
        let set_err = set_cooling_curve_points(
            "fan_0".into(),
            "missing".into(),
            vec![[30.0, 20.0], [80.0, 100.0]],
            None,
            app.clone(),
        )
        .await
        .unwrap_err();
        assert!(set_err.to_string().contains("unknown cooling channel"));

        let remove_err = remove_cooling_curve("fan_0".into(), "missing".into(), app)
            .await
            .unwrap_err();
        assert!(remove_err.to_string().contains("unknown cooling channel"));
    }

    #[tokio::test]
    async fn set_fan_curve_points_errors_on_missing_device() {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let err = set_cooling_curve_points(
            "ghost".into(),
            "default".into(),
            vec![[30.0f32, 20.0], [80.0f32, 100.0]],
            None,
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[tokio::test]
    async fn set_fan_curve_preset_errors_on_missing_device() {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let err = set_cooling_curve_preset(
            "ghost".into(),
            "default".into(),
            "balanced".into(),
            None,
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn validate_points_errors_on_duty_above_100() {
        let err = validate_points(&[(30.0, 20.0), (50.0, 101.0)]).unwrap_err();
        assert!(err.to_string().contains("duty"));
    }

    #[test]
    fn validate_points_errors_on_negative_duty() {
        let err = validate_points(&[(30.0, 20.0), (50.0, -1.0)]).unwrap_err();
        assert!(err.to_string().contains("duty"));
    }
}
