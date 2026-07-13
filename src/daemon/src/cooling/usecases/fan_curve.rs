// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{anyhow, Result};
use std::sync::Arc;

use crate::cooling::config::FanCurveRecord;
use crate::cooling::fan_curve::preset_curves;
use crate::profiles::device_state::persist_device_state;
use crate::registry::require_device_owned_id;
use crate::state::AppState;

/// Look up a controllable fan by id, run `f` against its capability, then persist.
async fn with_fan<F>(fan_id: &str, app: &Arc<AppState>, f: F) -> Result<()>
where
    F: FnOnce(&dyn crate::drivers::FanCapability),
{
    let device = require_device_owned_id(fan_id, app).await?;
    let fan = device.as_fan().ok_or_else(|| {
        log::warn!("[fan_curve] {fan_id} has no engine slot (not controllable)");
        anyhow!("device does not support fan curve: {fan_id}")
    })?;
    f(fan);
    persist_device_state(app, device.as_ref()).await;
    Ok(())
}

pub async fn set_fan_curve_points(
    fan_id: String,
    points: Vec<[f32; 2]>,
    sensor_id: Option<String>,
    app: Arc<AppState>,
) -> Result<()> {
    let points: Vec<(f32, f32)> = points.iter().map(|p| (p[0], p[1])).collect();
    let record = FanCurveRecord { sensor_id, points };
    record.validate()?;
    require_temperature_sensor(&record.sensor_id, &app).await?;

    with_fan(&fan_id, &app, |fan| fan.set_fan_curve(record)).await
}

pub async fn set_fan_curve_preset(
    fan_id: String,
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
    with_fan(&fan_id, &app, |fan| fan.set_fan_curve(record)).await
}

/// `Ok` unless `sensor_id` is `Some` and names something other than a
/// temperature sensor the system currently exposes.
async fn require_temperature_sensor(sensor_id: &Option<String>, app: &Arc<AppState>) -> Result<()> {
    let Some(id) = sensor_id else {
        return Ok(());
    };
    let sensors = app.snapshot_sensors().await;
    match sensors.get(id) {
        Some(s) if s.sensor_type == halod_shared::types::SensorType::Temperature => Ok(()),
        Some(_) => Err(anyhow!("sensor '{id}' is not a temperature source")),
        None => Err(anyhow!("unknown sensor '{id}'")),
    }
}

pub async fn remove_fan_curve(fan_id: String, app: Arc<AppState>) -> Result<()> {
    with_fan(&fan_id, &app, |fan| fan.clear_fan_curve()).await
}

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
    use crate::config::Config;
    use crate::state::AppState;
    use crate::test_support::MockDevice;

    use std::sync::Arc;

    fn fan_curve(device: &MockDevice) -> Option<crate::cooling::config::FanCurveRecord> {
        device.fan.as_ref().unwrap().fan_curve()
    }

    fn make_app_with_fan(id: &str) -> (Arc<AppState>, Arc<MockDevice>) {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let device = Arc::new(MockDevice::new(id).with_fan());
        {
            let mut devices = app.devices.try_write().unwrap();
            devices.push(device.clone() as Arc<dyn crate::drivers::Device>);
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
            visibility: Default::default(),
        }
    }

    fn make_app_with_fan_and_sensor(
        fan_id: &str,
        sensor_id: &str,
    ) -> (Arc<AppState>, Arc<MockDevice>) {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let device = Arc::new(MockDevice::new(fan_id).with_fan().with_sensor(vec![sensor(
            sensor_id,
            halod_shared::types::SensorType::Temperature,
        )]));
        {
            let mut devices = app.devices.try_write().unwrap();
            devices.push(device.clone() as Arc<dyn crate::drivers::Device>);
        }
        (app, device)
    }

    #[tokio::test]
    async fn set_fan_curve_preset_stores_points() {
        let (app, device) = make_app_with_fan_and_sensor("fan_0", "sensor_0");
        let preset = preset_curves().iter().find(|p| p.id == "balanced").unwrap();
        set_fan_curve_preset(
            "fan_0".into(),
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
        set_fan_curve_points(
            "fan_0".into(),
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
        set_fan_curve_points(
            "fan_0".into(),
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
        let err = set_fan_curve_points(
            "fan_0".into(),
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
            let mut devices = app.devices.try_write().unwrap();
            devices.push(device.clone() as Arc<dyn crate::drivers::Device>);
        }
        let err = set_fan_curve_points(
            "fan_0".into(),
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
        set_fan_curve_points(
            "fan_0".into(),
            vec![[30.0f32, 20.0], [80.0f32, 100.0]],
            None,
            app.clone(),
        )
        .await
        .unwrap();
        assert!(fan_curve(&device).is_some());

        remove_fan_curve("fan_0".into(), app).await.unwrap();
        assert!(fan_curve(&device).is_none());
    }

    #[tokio::test]
    async fn remove_fan_curve_errors_on_missing_device() {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let err = remove_fan_curve("nonexistent".into(), app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[tokio::test]
    async fn set_fan_curve_points_errors_on_missing_device() {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let err = set_fan_curve_points(
            "ghost".into(),
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
        let err = set_fan_curve_preset("ghost".into(), "balanced".into(), None, app)
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
