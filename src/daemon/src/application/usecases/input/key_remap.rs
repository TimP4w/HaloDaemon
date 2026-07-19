// SPDX-License-Identifier: GPL-3.0-or-later
use crate::domain::events::ChangeSink as _;

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::input::validate::{validate_button_mapping, validate_macro};
use crate::domain::profiles::device_state::persist_device_state;
use crate::domain::registry::require_device_owned_id;
use halod_shared::types::{ButtonMapping, MacroStep};

pub async fn set_button_mapping(
    id: String,
    mapping: ButtonMapping,
    app: Arc<AppState>,
) -> Result<()> {
    validate_button_mapping(&mapping)?;
    let device = require_device_owned_id(&id, &app).await?;
    let cap = device
        .as_key_remap()
        .context("device does not support key remapping")?;

    cap.set_button_mapping(mapping).await?;
    persist_device_state(&app, device.as_ref()).await;
    app.record_change(crate::domain::events::Change::Device(id))
        .await;
    Ok(())
}

pub async fn reset_button_mapping(id: String, cid: u16, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let cap = device
        .as_key_remap()
        .context("device does not support key remapping")?;

    cap.reset_button_mapping(cid).await?;
    persist_device_state(&app, device.as_ref()).await;
    app.record_change(crate::domain::events::Change::Device(id))
        .await;
    Ok(())
}

pub async fn reset_all_button_mappings(id: String, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let cap = device
        .as_key_remap()
        .context("device does not support key remapping")?;

    cap.reset_all_button_mappings().await?;
    persist_device_state(&app, device.as_ref()).await;
    app.record_change(crate::domain::events::Change::Device(id))
        .await;
    Ok(())
}

pub async fn play_macro(steps: Vec<MacroStep>, app: Arc<AppState>) -> Result<()> {
    validate_macro(&steps)?;
    let exec = app
        .input
        .executor()
        .context("input injection unavailable, action executor failed to initialize")?;
    anyhow::ensure!(exec.play_macro(steps), "a macro is already playing");
    Ok(())
}

pub async fn set_software_dpi_steps(id: String, steps: Vec<u32>, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let cap = device
        .as_dpi()
        .context("device does not support software DPI")?;

    let steps: Vec<u16> = steps
        .into_iter()
        .map(|s| {
            u16::try_from(s)
                .map_err(|_| anyhow::anyhow!("DPI step {s} exceeds maximum value 65535"))
        })
        .collect::<Result<Vec<u16>>>()?;

    cap.set_dpi_steps(steps).await?;
    persist_device_state(&app, device.as_ref()).await;
    app.record_change(crate::domain::events::Change::Device(id))
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::domain::device::Device;
    use crate::test_support::MockDevice;
    use halod_shared::types::{ButtonAction, MACRO_MAX_DELAY_MS, MACRO_MAX_STEPS};
    use std::sync::Arc;

    fn make_app(dev: Arc<MockDevice>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        let devices: Vec<Arc<dyn Device>> = vec![dev as Arc<dyn Device>];
        *app.device_registry.try_write().unwrap() = devices;
        app
    }

    #[tokio::test]
    async fn set_button_mapping_happy_path() {
        let dev = Arc::new(MockDevice::new("dev1").with_key_remap());
        let app = make_app(dev.clone());
        let mapping = ButtonMapping {
            cid: 1,
            base: ButtonAction::Native,
            shifted: ButtonAction::Native,
        };
        set_button_mapping("dev1".into(), mapping, app)
            .await
            .unwrap();
        let last = dev
            .key_remap_last_mapping
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        assert_eq!(last.cid, 1);
    }

    #[tokio::test]
    async fn set_button_mapping_error_on_missing_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let err = set_button_mapping(
            "no-such-device".into(),
            ButtonMapping {
                cid: 0,
                base: ButtonAction::Native,
                shifted: ButtonAction::Native,
            },
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[tokio::test]
    async fn set_button_mapping_error_when_capability_absent() {
        let dev = Arc::new(MockDevice::new("dev1")); // no key_remap capability
        let app = make_app(dev.clone());
        let mapping = ButtonMapping {
            cid: 1,
            base: ButtonAction::Native,
            shifted: ButtonAction::Native,
        };
        let err = set_button_mapping("dev1".into(), mapping, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("key remapping"), "got: {err}");
    }

    #[tokio::test]
    async fn reset_button_mapping_happy_path() {
        let dev = Arc::new(MockDevice::new("dev1").with_key_remap());
        let app = make_app(dev.clone());
        reset_button_mapping("dev1".into(), 1, app).await.unwrap();
    }

    #[tokio::test]
    async fn reset_button_mapping_error_when_capability_absent() {
        let dev = Arc::new(MockDevice::new("dev1"));
        let app = make_app(dev.clone());
        let err = reset_button_mapping("dev1".into(), 1, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("key remapping"), "got: {err}");
    }

    #[tokio::test]
    async fn reset_all_button_mappings_happy_path() {
        let dev = Arc::new(MockDevice::new("dev1").with_key_remap());
        let app = make_app(dev.clone());
        reset_all_button_mappings("dev1".into(), app).await.unwrap();
    }

    #[tokio::test]
    async fn reset_all_button_mappings_error_when_capability_absent() {
        let dev = Arc::new(MockDevice::new("dev1"));
        let app = make_app(dev.clone());
        let err = reset_all_button_mappings("dev1".into(), app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("key remapping"), "got: {err}");
    }

    #[tokio::test]
    async fn set_software_dpi_steps_happy_path() {
        let dev = Arc::new(MockDevice::new("dev1").with_dpi());
        let app = make_app(dev.clone());
        set_software_dpi_steps("dev1".into(), vec![800, 1600], app)
            .await
            .unwrap();
        let steps = dev
            .dpi_last_steps
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        assert_eq!(steps, vec![800u16, 1600u16]);
    }

    #[tokio::test]
    async fn play_macro_rejects_empty_steps() {
        let app = Arc::new(AppState::new(Config::default()));
        let err = play_macro(vec![], app).await.unwrap_err();
        assert!(err.to_string().contains("no steps"), "got: {err}");
    }

    #[tokio::test]
    async fn play_macro_rejects_too_many_steps() {
        let app = Arc::new(AppState::new(Config::default()));
        let steps = vec![
            halod_shared::types::MacroStep {
                kind: halod_shared::types::MacroAtom::Delay,
                delay_after_ms: 1,
            };
            MACRO_MAX_STEPS + 1
        ];
        let err = play_macro(steps, app).await.unwrap_err();
        assert!(err.to_string().contains("exceeds"), "got: {err}");
    }

    #[tokio::test]
    async fn play_macro_rejects_excessive_delay() {
        let app = Arc::new(AppState::new(Config::default()));
        let steps = vec![halod_shared::types::MacroStep {
            kind: halod_shared::types::MacroAtom::Delay,
            delay_after_ms: MACRO_MAX_DELAY_MS + 1,
        }];
        let err = play_macro(steps, app).await.unwrap_err();
        assert!(err.to_string().contains("delay exceeds"), "got: {err}");
    }

    #[tokio::test]
    async fn play_macro_errors_without_executor() {
        let app = Arc::new(AppState::new(Config::default()));
        let steps = vec![halod_shared::types::MacroStep {
            kind: halod_shared::types::MacroAtom::Delay,
            delay_after_ms: 100,
        }];
        let err = play_macro(steps, app).await.unwrap_err();
        assert!(err.to_string().contains("unavailable"), "got: {err}");
    }

    #[tokio::test]
    async fn set_software_dpi_steps_error_when_capability_absent() {
        let dev = Arc::new(MockDevice::new("dev1")); // no dpi capability
        let app = make_app(dev.clone());
        let err = set_software_dpi_steps("dev1".into(), vec![800], app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("software DPI"), "got: {err}");
    }
}
