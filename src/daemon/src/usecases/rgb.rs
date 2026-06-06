use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use super::require_device_owned;
use crate::state::AppState;
use halod_protocol::types::RgbState;
use halod_protocol::zone_transform::ZoneContentTransform;

pub async fn rgb_apply(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let rgb = device
        .as_rgb()
        .ok_or_else(|| anyhow::anyhow!("device does not support RGB"))?;
    let state: RgbState = serde_json::from_value(
        msg.get("state")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing state field"))?,
    )?;
    rgb.apply(state).await?;
    super::persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

/// Set a zone's LED-content transform. The transform is a persistent per-device
/// setting, applied to all daemon-driven output for that zone.
pub async fn set_zone_transform(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let rgb = device
        .as_rgb()
        .ok_or_else(|| anyhow::anyhow!("device does not support RGB"))?;
    let zone_id = msg["zone_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing zone_id"))?
        .to_string();
    let transform: ZoneContentTransform = serde_json::from_value(
        msg["transform"].clone(),
    )
    .map_err(|_| anyhow::anyhow!("invalid transform"))?;

    rgb.set_zone_transform(zone_id.clone(), transform);

    let _cfg_snap = {
        let mut cfg = app.config.write().await;
        cfg.device_transforms
            .entry(device.id())
            .or_default()
            .insert(zone_id, transform);
        cfg.clone()
    };
    #[cfg(not(test))]
    app.request_config_save(_cfg_snap);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::{CapabilityRef, Device, RgbCapability, RgbStateSlot};
    use async_trait::async_trait;
    use halod_protocol::types::{RgbDescriptor, RgbState};
    use serde_json::json;

    struct MockRgbDevice {
        id: &'static str,
        transforms: std::sync::Mutex<std::collections::HashMap<String, ZoneContentTransform>>,
        rgb: RgbStateSlot,
    }

    #[async_trait]
    impl Device for MockRgbDevice {
        fn id(&self) -> String { self.id.to_string() }
        fn name(&self) -> &str { "mock" }
        fn vendor(&self) -> &str { "mock" }
        fn model(&self) -> &str { "mock" }
        async fn initialize(&self) -> anyhow::Result<bool> { Ok(true) }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> { vec![CapabilityRef::Rgb(self)] }
    }

    #[async_trait]
    impl RgbCapability for MockRgbDevice {
        fn rgb_state(&self) -> &RgbStateSlot { &self.rgb }
        fn descriptor(&self) -> &RgbDescriptor {
            use std::sync::OnceLock;
            static DESC: OnceLock<RgbDescriptor> = OnceLock::new();
            DESC.get_or_init(|| RgbDescriptor { zones: vec![], native_effects: vec![] })
        }
        async fn apply(&self, _state: RgbState) -> anyhow::Result<()> { Ok(()) }
        fn current_state(&self) -> Option<RgbState> { None }
        async fn write_frame(&self, _zone_id: &str, _colors: &[halod_protocol::types::RgbColor]) -> anyhow::Result<()> { Ok(()) }
        fn zone_transforms(&self) -> std::collections::HashMap<String, ZoneContentTransform> {
            self.transforms.lock().unwrap().clone()
        }
        fn transform_for(&self, zone_id: &str) -> ZoneContentTransform {
            self.transforms.lock().unwrap().get(zone_id).copied().unwrap_or_default()
        }
        fn set_zone_transform(&self, zone_id: String, transform: ZoneContentTransform) {
            self.transforms.lock().unwrap().insert(zone_id, transform);
        }
        fn set_zone_transforms(&self, m: std::collections::HashMap<String, ZoneContentTransform>) {
            *self.transforms.lock().unwrap() = m;
        }
    }

    #[tokio::test]
    async fn rgb_apply_errors_on_missing_state_field() {
        let dev = Arc::new(MockRgbDevice { id: "dev1", transforms: Default::default(), rgb: RgbStateSlot::default() });
        let app = Arc::new(AppState::new(Config::default()));
        {
            let mut devices = app.devices.lock().await;
            devices.push(dev as Arc<dyn crate::drivers::Device>);
        }
        let err = rgb_apply(json!({"id": "dev1"}), app).await.unwrap_err();
        assert!(err.to_string().contains("missing state field"), "got: {err}");
    }

    #[tokio::test]
    async fn set_zone_transform_stores_transform_in_config() {
        let dev = Arc::new(MockRgbDevice { id: "dev1", transforms: Default::default(), rgb: RgbStateSlot::default() });
        let app = Arc::new(AppState::new(Config::default()));
        app.devices
            .lock()
            .await
            .push(dev.clone() as Arc<dyn crate::drivers::Device>);

        set_zone_transform(
            json!({
                "id": "dev1",
                "zone_id": "ring",
                "transform": {"reverse": true, "led_offset": 4, "flip_h": false, "flip_v": false, "swap_rings": false}
            }),
            app.clone(),
        )
        .await
        .unwrap();

        let cfg = app.config.read().await;
        let t = cfg
            .device_transforms
            .get("dev1")
            .and_then(|m| m.get("ring"))
            .copied()
            .expect("transform should be stored in config");
        assert!(t.reverse);
        assert_eq!(t.led_offset, 4);
        assert!(!t.flip_h && !t.flip_v);

        // Verify device's set_zone_transform was invoked
        let device_transforms = dev.zone_transforms();
        let device_t = device_transforms
            .get("ring")
            .copied()
            .expect("transform should be stored in device");
        assert!(device_t.reverse);
        assert_eq!(device_t.led_offset, 4);
    }

    #[tokio::test]
    async fn set_zone_transform_errors_on_missing_zone_id() {
        let dev = Arc::new(MockRgbDevice { id: "dev1", transforms: Default::default(), rgb: RgbStateSlot::default() });
        let app = Arc::new(AppState::new(Config::default()));
        app.devices
            .lock()
            .await
            .push(dev as Arc<dyn crate::drivers::Device>);
        let err = set_zone_transform(json!({"id": "dev1"}), app).await.unwrap_err();
        assert!(err.to_string().contains("zone_id"), "got: {err}");
    }
}
