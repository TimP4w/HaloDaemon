use anyhow::Result;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

use super::parse_params;
use crate::{engines::lcd::LcdEngine, ipc, state::AppState};

pub async fn set_template(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device_id = msg["device_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing device_id"))?
        .to_string();
    let template_id = msg["template_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing template_id"))?
        .to_string();
    let params = parse_params(&msg["params"]);

    if !LcdEngine::template_exists(&template_id) {
        anyhow::bail!("unknown template: {template_id}");
    }

    let device = {
        let devices = app.devices.lock().await;
        devices
            .iter()
            .find(|d| d.id() == device_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("device not found: {device_id}"))?
    };
    let slot = device
        .as_lcd()
        .ok_or_else(|| anyhow::anyhow!("device does not support LCD engine: {device_id}"))?;
    slot.set_lcd_template_id(Some(template_id.clone()));
    slot.set_lcd_template_params(params.clone());
    super::persist_device_state(&app, device.as_ref()).await;

    if let Some(engines) = app.engines.get() {
        engines.lcd.set_template_active(&device_id, &template_id, &params).await;
    }

    ipc::broadcast_state(app).await;
    Ok(())
}

pub async fn deactivate(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device_id = msg["device_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing device_id"))?
        .to_string();

    let device = {
        let devices = app.devices.lock().await;
        devices
            .iter()
            .find(|d| d.id() == device_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("device not found: {device_id}"))?
    };
    let slot = device
        .as_lcd()
        .ok_or_else(|| anyhow::anyhow!("device does not support LCD engine: {device_id}"))?;
    slot.set_lcd_template_id(None);
    slot.set_lcd_template_params(HashMap::new());
    super::persist_device_state(&app, device.as_ref()).await;

    if let Some(engines) = app.engines.get() {
        engines.lcd.remove_device(&device_id).await;
    }

    ipc::broadcast_state(app).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::{CapabilityRef, LcdCapability, LcdStateSlot};
    use crate::state::AppState;
    use async_trait::async_trait;
    use halod_protocol::types::{LcdDescriptor, LcdMode, LcdStatus, ScreenShape};
    use serde_json::json;
    use std::sync::Arc;

    struct MockLcdDevice {
        id: String,
        lcd: LcdStateSlot,
    }

    impl MockLcdDevice {
        fn new(id: &str) -> Arc<Self> {
            Arc::new(Self { id: id.to_string(), lcd: LcdStateSlot::default() })
        }
    }

    #[async_trait]
    impl crate::drivers::Device for MockLcdDevice {
        fn id(&self) -> String { self.id.clone() }
        fn name(&self) -> &str { "mock" }
        fn vendor(&self) -> &str { "mock" }
        fn model(&self) -> &str { "mock" }
        async fn initialize(&self) -> anyhow::Result<bool> { Ok(true) }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::Lcd(self)]
        }
    }

    fn mock_lcd_descriptor() -> LcdDescriptor {
        LcdDescriptor {
            shape: ScreenShape::Circle,
            width: 0,
            height: 0,
            supported_rotations: vec![],
            supported_image_types: vec![],
        }
    }

    #[async_trait]
    impl LcdCapability for MockLcdDevice {
        fn lcd_descriptor(&self) -> LcdDescriptor { mock_lcd_descriptor() }
        fn current_state(&self) -> LcdStatus {
            LcdStatus {
                descriptor: mock_lcd_descriptor(),
                brightness: 50,
                rotation: 0,
                mode: LcdMode::Image,
                active_image: None,
            }
        }
        async fn set_image(&self, _data: &[u8]) -> anyhow::Result<()> { Ok(()) }
        async fn set_rotation(&self, _degrees: u32) -> anyhow::Result<()> { Ok(()) }
        async fn set_brightness(&self, _brightness: u8) -> anyhow::Result<()> { Ok(()) }
        async fn reset_to_default(&self) -> anyhow::Result<()> { Ok(()) }
        async fn set_active_image_filename(&self, _filename: Option<String>) {}
        fn lcd_state(&self) -> &LcdStateSlot { &self.lcd }
    }

    fn make_app(device: Arc<MockLcdDevice>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.try_lock().unwrap().push(device as Arc<dyn crate::drivers::Device>);
        app
    }

    #[tokio::test]
    async fn set_template_errors_on_unknown_template() {
        let dev = MockLcdDevice::new("dev0");
        let app = make_app(dev);
        let msg = json!({"device_id": "dev0", "template_id": "not_a_real_template"});
        let err = set_template(msg, app).await.unwrap_err();
        assert!(err.to_string().contains("unknown template"));
    }

    #[tokio::test]
    async fn set_template_errors_on_missing_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let msg = json!({"device_id": "ghost", "template_id": "frame_counter"});
        let err = set_template(msg, app).await.unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[tokio::test]
    async fn set_template_stores_template_id_in_slot() {
        let dev = MockLcdDevice::new("dev0");
        let app = make_app(dev.clone());
        let msg = json!({"device_id": "dev0", "template_id": "frame_counter"});
        set_template(msg, app).await.unwrap();
        assert_eq!(dev.lcd.lcd_template_id().as_deref(), Some("frame_counter"));
    }

    #[tokio::test]
    async fn deactivate_clears_template_id_in_slot() {
        let dev = MockLcdDevice::new("dev0");
        let app = make_app(dev.clone());
        dev.lcd.set_lcd_template_id(Some("system_stats".to_string()));
        let msg = json!({"device_id": "dev0"});
        deactivate(msg, app).await.unwrap();
        assert!(dev.lcd.lcd_template_id().is_none());
    }

    #[tokio::test]
    async fn deactivate_errors_on_missing_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let msg = json!({"device_id": "ghost"});
        let err = deactivate(msg, app).await.unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }
}
