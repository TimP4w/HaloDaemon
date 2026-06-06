use anyhow::{anyhow, Result};
use serde_json::Value;
use std::sync::Arc;

use crate::config::FanCurveRecord;
use crate::engines::fan_curve::preset_curves;
use crate::state::AppState;

pub async fn set_fan_curve_points(msg: Value, app: Arc<AppState>) -> Result<()> {
    let fan_id = msg["fan_id"]
        .as_str()
        .ok_or_else(|| anyhow!("missing fan_id"))?
        .to_string();
    let sensor_id: Option<String> = msg["sensor_id"].as_str().map(str::to_owned);
    let points = parse_points(&msg)?;
    validate_points(&points)?;

    let record = FanCurveRecord { sensor_id, points };

    let device = {
        let devices = app.devices.lock().await;
        devices
            .iter()
            .find(|d| d.id() == fan_id)
            .cloned()
            .ok_or_else(|| anyhow!("device not found: {fan_id}"))?
    };
    let slot = device.as_fan().ok_or_else(|| {
        log::warn!("[fan_curve] {fan_id} has no engine slot (not controllable)");
        anyhow!("device does not support fan curve: {fan_id}")
    })?;
    slot.set_fan_curve(record);
    super::persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

pub async fn set_fan_curve_preset(msg: Value, app: Arc<AppState>) -> Result<()> {
    let fan_id = msg["fan_id"]
        .as_str()
        .ok_or_else(|| anyhow!("missing fan_id"))?
        .to_string();
    let sensor_id: Option<String> = msg["sensor_id"].as_str().map(str::to_owned);
    let preset_id = msg["preset"]
        .as_str()
        .ok_or_else(|| anyhow!("missing preset"))?;
    let points = preset_curves()
        .iter()
        .find(|p| p.id == preset_id)
        .ok_or_else(|| anyhow!("unknown preset: {preset_id}"))?
        .points
        .to_vec();

    let record = FanCurveRecord { sensor_id, points };

    let device = {
        let devices = app.devices.lock().await;
        devices
            .iter()
            .find(|d| d.id() == fan_id)
            .cloned()
            .ok_or_else(|| anyhow!("device not found: {fan_id}"))?
    };
    let slot = device.as_fan().ok_or_else(|| {
        log::warn!("[fan_curve] {fan_id} has no engine slot (not controllable)");
        anyhow!("device does not support fan curve: {fan_id}")
    })?;
    slot.set_fan_curve(record);
    super::persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

pub async fn remove_fan_curve(msg: Value, app: Arc<AppState>) -> Result<()> {
    let fan_id = msg["fan_id"]
        .as_str()
        .ok_or_else(|| anyhow!("missing fan_id"))?
        .to_string();

    let device = {
        let devices = app.devices.lock().await;
        devices
            .iter()
            .find(|d| d.id() == fan_id)
            .cloned()
            .ok_or_else(|| anyhow!("device not found: {fan_id}"))?
    };
    let slot = device.as_fan().ok_or_else(|| {
        log::warn!("[fan_curve] {fan_id} has no engine slot (not controllable)");
        anyhow!("device does not support fan curve: {fan_id}")
    })?;
    slot.clear_fan_curve();
    super::persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

fn parse_points(msg: &Value) -> Result<Vec<(f32, f32)>> {
    let arr = msg["points"]
        .as_array()
        .ok_or_else(|| anyhow!("missing points array"))?;
    arr.iter()
        .map(|p| {
            let x = p[0].as_f64().ok_or_else(|| anyhow!("invalid point x"))? as f32;
            let y = p[1].as_f64().ok_or_else(|| anyhow!("invalid point y"))? as f32;
            Ok((x, y))
        })
        .collect::<Result<Vec<_>>>()
}

fn validate_points(points: &[(f32, f32)]) -> Result<()> {
    if points.len() < 2 {
        anyhow::bail!("fan curve must have at least 2 points");
    }
    for &(_, duty) in points {
        if !(0.0..=100.0).contains(&duty) {
            anyhow::bail!("duty must be between 0 and 100, got {duty}");
        }
    }
    for window in points.windows(2) {
        if window[0].0 >= window[1].0 {
            anyhow::bail!("temperature points must be strictly ascending");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::{CapabilityRef, FanCapability, FanStateSlot};
    use crate::state::AppState;
    use async_trait::async_trait;

    use serde_json::json;
    use std::sync::Arc;

    // ---------------------------------------------------------------------------
    // MockFanDevice
    // ---------------------------------------------------------------------------

    struct MockFanDevice {
        id: String,
        fan: FanStateSlot,
    }

    impl MockFanDevice {
        fn new(id: &str) -> Self {
            Self { id: id.to_string(), fan: FanStateSlot::default() }
        }
    }

    #[async_trait]
    impl crate::drivers::Device for MockFanDevice {
        fn id(&self) -> String {
            self.id.clone()
        }
        fn name(&self) -> &str {
            "Mock Fan"
        }
        fn vendor(&self) -> &str {
            "Mock"
        }
        fn model(&self) -> &str {
            "Fan"
        }
        async fn initialize(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::Fan(self)]
        }
    }

    #[async_trait]
    impl FanCapability for MockFanDevice {
        async fn get_duty(&self) -> anyhow::Result<u8> { Ok(0) }
        async fn set_duty(&self, _: u8) -> anyhow::Result<()> { Ok(()) }
        async fn get_rpm(&self) -> Option<u32> { None }
        fn fan_state(&self) -> &FanStateSlot { &self.fan }
    }

    fn make_app_with_fan(id: &str) -> (Arc<AppState>, Arc<MockFanDevice>) {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let device = Arc::new(MockFanDevice::new(id));
        {
            let mut devices = app.devices.try_lock().unwrap();
            devices.push(device.clone() as Arc<dyn crate::drivers::Device>);
        }
        (app, device)
    }

    // ---------------------------------------------------------------------------
    // Tests
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn set_fan_curve_preset_stores_points() {
        let (app, device) = make_app_with_fan("fan_0");
        let preset = preset_curves().iter().find(|p| p.id == "balanced").unwrap();
        let msg = json!({
            "fan_id": "fan_0",
            "preset": "balanced",
            "sensor_id": "sensor_0"
        });
        set_fan_curve_preset(msg, app).await.unwrap();
        let curve = device.fan.fan_curve().expect("curve should be set");
        assert_eq!(curve.sensor_id, Some("sensor_0".to_string()));
        assert_eq!(curve.points, preset.points.to_vec());
    }

    #[tokio::test]
    async fn set_fan_curve_points_stores_custom_points() {
        let (app, device) = make_app_with_fan("fan_0");
        let msg = json!({"fan_id": "fan_0", "points": [[30, 20], [80, 100]]});
        set_fan_curve_points(msg, app).await.unwrap();
        let curve = device.fan.fan_curve().expect("curve should be set");
        assert_eq!(curve.points, vec![(30.0, 20.0), (80.0, 100.0)]);
    }

    #[test]
    fn set_fan_curve_points_errors_on_non_ascending_temps() {
        let msg = json!({"points": [[80, 20], [30, 100]]});
        let points = parse_points(&msg).unwrap();
        let err = validate_points(&points).unwrap_err();
        assert!(err.to_string().contains("ascending"));
    }

    #[test]
    fn set_fan_curve_points_errors_on_single_point() {
        let msg = json!({"points": [[50, 75]]});
        let points = parse_points(&msg).unwrap();
        let err = validate_points(&points).unwrap_err();
        assert!(err.to_string().contains("at least 2"));
    }

    #[tokio::test]
    async fn remove_fan_curve_removes_entry() {
        let (app, device) = make_app_with_fan("fan_0");
        // First set a curve.
        let set_msg = json!({"fan_id": "fan_0", "points": [[30, 20], [80, 100]]});
        set_fan_curve_points(set_msg, app.clone()).await.unwrap();
        assert!(device.fan.fan_curve().is_some());

        // Now remove it.
        let remove_msg = json!({"fan_id": "fan_0"});
        remove_fan_curve(remove_msg, app).await.unwrap();
        assert!(device.fan.fan_curve().is_none());
    }

    #[tokio::test]
    async fn remove_fan_curve_errors_on_missing_device() {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let msg = json!({"fan_id": "nonexistent"});
        let err = remove_fan_curve(msg, app).await.unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[tokio::test]
    async fn set_fan_curve_points_errors_on_missing_device() {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let msg = json!({"fan_id": "ghost", "points": [[30, 20], [80, 100]]});
        let err = set_fan_curve_points(msg, app).await.unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[tokio::test]
    async fn set_fan_curve_preset_errors_on_missing_device() {
        let cfg = Config::default();
        let app = Arc::new(AppState::new(cfg));
        let msg = json!({"fan_id": "ghost", "preset": "balanced"});
        let err = set_fan_curve_preset(msg, app).await.unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }
}
