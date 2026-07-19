// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;

use anyhow::Result;
use std::sync::Arc;

use crate::profiles::device_state::persist_device_state;
use crate::registry::require_device_owned_id;
use crate::{lcd::engine::LcdEngine, state::AppState};
use halod_shared::lcd_custom::{CustomTemplateDef, WIDGETS_JSON_PARAM};
use halod_shared::types::{EffectParamValue, LcdHealth};

pub async fn set_template(
    device_id: String,
    template_id: String,
    params: HashMap<String, EffectParamValue>,
    app: Arc<AppState>,
) -> Result<()> {
    if !LcdEngine::template_exists(&template_id) {
        anyhow::bail!("unknown template: {template_id}");
    }
    if template_id == "custom" {
        let raw = match params.get(WIDGETS_JSON_PARAM) {
            Some(EffectParamValue::Str(raw)) => raw,
            Some(_) => anyhow::bail!("widgets_json must be text"),
            None => "",
        };
        let def = if raw.is_empty() {
            CustomTemplateDef::default()
        } else {
            serde_json::from_str(raw)?
        };
        crate::lcd::usecases::templates::validate_template(&def)?;
        crate::lcd::usecases::templates::validate_template_catalog(&def, &app.registry)?;
    }

    let device = require_device_owned_id(&device_id, &app).await?;
    let slot = device
        .as_lcd()
        .ok_or_else(|| anyhow::anyhow!("device does not support LCD engine: {device_id}"))?;
    slot.lcd_state().set_health(LcdHealth::Starting);
    slot.set_lcd_template_id(Some(template_id.clone()));
    slot.set_lcd_template_params(params.clone());
    persist_device_state(&app, device.as_ref()).await;

    if let Some(video) = app.lcd.video() {
        video.stop(&device_id).await;
    }
    if let Some(lcd_engine) = app.lcd.engine() {
        lcd_engine
            .set_template_active(&device_id, &template_id, &params)
            .await;
    }
    slot.lcd_state().set_health(LcdHealth::Stable);

    app.record_change(crate::services::effective_state::Change::LcdDevice(
        device_id,
    ))
    .await;
    Ok(())
}

pub async fn deactivate(device_id: String, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&device_id, &app).await?;
    let slot = device
        .as_lcd()
        .ok_or_else(|| anyhow::anyhow!("device does not support LCD engine: {device_id}"))?;
    slot.lcd_state().set_health(LcdHealth::Stopping);
    slot.set_lcd_template_id(None);
    slot.set_lcd_template_params(HashMap::new());
    persist_device_state(&app, device.as_ref()).await;

    if let Some(lcd_engine) = app.lcd.engine() {
        lcd_engine.remove_device(&device_id).await;
    }
    slot.lcd_state().set_health(LcdHealth::Stable);

    app.record_change(crate::services::effective_state::Change::LcdDevice(
        device_id,
    ))
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::{CapabilityRef, LcdCapability, LcdStateSlot};
    use crate::state::AppState;
    use async_trait::async_trait;
    use halod_shared::types::{LcdDescriptor, LcdMode, LcdStatus, ScreenShape};
    use std::sync::Arc;

    struct MockLcdDevice {
        id: String,
        lcd: LcdStateSlot,
    }

    impl MockLcdDevice {
        fn new(id: &str) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_string(),
                lcd: LcdStateSlot::default(),
            })
        }
    }

    #[async_trait]
    impl crate::drivers::Device for MockLcdDevice {
        fn id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            "mock"
        }
        fn vendor(&self) -> &str {
            "mock"
        }
        fn model(&self) -> &str {
            "mock"
        }
        async fn initialize(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
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
            latches_last_frame: false,
        }
    }

    #[async_trait]
    impl LcdCapability for MockLcdDevice {
        fn lcd_descriptor(&self) -> LcdDescriptor {
            mock_lcd_descriptor()
        }
        fn current_state(&self) -> LcdStatus {
            LcdStatus {
                descriptor: mock_lcd_descriptor(),
                brightness: 50,
                rotation: halod_shared::types::ScreenRotation::R0,
                mode: LcdMode::Image,
                active_image: None,
                raw_streaming: false,
                video_path: None,
                health: Default::default(),
            }
        }
        async fn set_image(&self, _data: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }
        async fn set_rotation(&self, _degrees: u32) -> anyhow::Result<()> {
            Ok(())
        }
        async fn set_brightness(&self, _brightness: u8) -> anyhow::Result<()> {
            Ok(())
        }
        async fn reset_to_default(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn set_active_image_filename(&self, _filename: Option<String>) {}
        fn lcd_state(&self) -> &LcdStateSlot {
            &self.lcd
        }
    }

    fn make_app(device: Arc<MockLcdDevice>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .try_write()
            .unwrap()
            .push(device as Arc<dyn crate::drivers::Device>);
        app
    }

    #[tokio::test]
    async fn set_template_errors_on_unknown_template() {
        let dev = MockLcdDevice::new("dev0");
        let app = make_app(dev);
        let err = set_template(
            "dev0".into(),
            "not_a_real_template".into(),
            HashMap::new(),
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown template"));
    }

    #[tokio::test]
    async fn set_template_errors_on_missing_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let err = set_template("ghost".into(), "custom".into(), HashMap::new(), app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[tokio::test]
    async fn set_template_stores_template_id_in_slot() {
        let dev = MockLcdDevice::new("dev0");
        let app = make_app(dev.clone());
        set_template("dev0".into(), "custom".into(), HashMap::new(), app)
            .await
            .unwrap();
        assert_eq!(dev.lcd.lcd_template_id().as_deref(), Some("custom"));
    }

    #[tokio::test]
    async fn set_template_keeps_template_active_when_engine_is_installed() {
        let dev = MockLcdDevice::new("dev0");
        let app = make_app(dev.clone());
        let engine = LcdEngine::new(Arc::clone(&app));
        app.lcd.set_engine(
            Arc::clone(&engine),
            crate::lcd::engine::video::VideoEngine::new(Arc::clone(&app), engine.frame_sender()),
        );

        set_template("dev0".into(), "custom".into(), HashMap::new(), app)
            .await
            .unwrap();

        assert_eq!(dev.lcd.mode(), LcdMode::Engine);
        assert_eq!(dev.lcd.lcd_template_id().as_deref(), Some("custom"));
        assert!(engine.has_slot("dev0").await);
    }

    #[tokio::test]
    async fn deactivate_clears_template_id_in_slot() {
        let dev = MockLcdDevice::new("dev0");
        let app = make_app(dev.clone());
        dev.lcd
            .set_lcd_template_id(Some("system_stats".to_string()));
        deactivate("dev0".into(), app).await.unwrap();
        assert!(dev.lcd.lcd_template_id().is_none());
    }

    #[tokio::test]
    async fn deactivate_errors_on_missing_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let err = deactivate("ghost".into(), app).await.unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }
}
