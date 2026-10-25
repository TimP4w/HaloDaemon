// SPDX-License-Identifier: GPL-3.0-or-later
use crate::domain::events::ChangeSink as _;

use std::collections::HashMap;

use anyhow::Result;
use std::sync::Arc;

use crate::domain::profiles::device_state::persist_device_state;
use crate::domain::registry::require_device_owned_id;
use crate::{application::state::AppState, lcd::engine::LcdEngine};
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
        crate::application::usecases::lcd::templates::validate_template(&def)?;
        crate::application::usecases::lcd::templates::validate_template_catalog(
            &def,
            &app.registry,
        )?;
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

    app.record_change(crate::domain::events::Change::LcdDevice(device_id))
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

    app.record_change(crate::domain::events::Change::LcdDevice(device_id))
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::state::AppState;
    use crate::domain::device::LcdCapability;
    use crate::test_support::{app_with, install_lcd_engine, MockDevice};
    use halod_shared::types::LcdMode;
    use std::sync::Arc;

    fn setup() -> (Arc<MockDevice>, Arc<AppState>) {
        let dev = Arc::new(MockDevice::new("dev0").with_lcd());
        (dev.clone(), app_with(vec![dev]))
    }

    #[tokio::test]
    async fn set_template_errors_on_unknown_template() {
        let (_, app) = setup();
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
        let app = app_with(vec![]);
        let err = set_template("ghost".into(), "custom".into(), HashMap::new(), app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[tokio::test]
    async fn set_template_stores_template_id_in_slot() {
        let (dev, app) = setup();
        set_template("dev0".into(), "custom".into(), HashMap::new(), app)
            .await
            .unwrap();
        assert_eq!(dev.lcd_state().lcd_template_id().as_deref(), Some("custom"));
    }

    #[tokio::test]
    async fn set_template_keeps_template_active_when_engine_is_installed() {
        let (dev, app) = setup();
        let engine = install_lcd_engine(&app);

        set_template("dev0".into(), "custom".into(), HashMap::new(), app)
            .await
            .unwrap();

        assert_eq!(dev.lcd_state().mode(), LcdMode::Engine);
        assert_eq!(dev.lcd_state().lcd_template_id().as_deref(), Some("custom"));
        assert!(engine.has_slot("dev0").await);
    }

    #[tokio::test]
    async fn deactivate_clears_template_id_in_slot() {
        let (dev, app) = setup();
        dev.lcd_state()
            .set_lcd_template_id(Some("system_stats".to_string()));
        deactivate("dev0".into(), app).await.unwrap();
        assert!(dev.lcd_state().lcd_template_id().is_none());
    }

    #[tokio::test]
    async fn deactivate_errors_on_missing_device() {
        let app = app_with(vec![]);
        let err = deactivate("ghost".into(), app).await.unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }
}
