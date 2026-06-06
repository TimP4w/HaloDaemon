use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

use super::require_device_owned_id;
use crate::state::AppState;

#[derive(Deserialize)]
struct SetDpiStepsReq {
    id: String,
    steps: Vec<u16>,
}

pub async fn set_dpi_steps(msg: Value, app: Arc<AppState>) -> Result<()> {
    let r: SetDpiStepsReq =
        serde_json::from_value(msg).context("invalid set_dpi_steps request")?;
    let dev = require_device_owned_id(&r.id, &app).await?;
    dev.as_dpi()
        .context("device does not support DPI control")?
        .set_dpi_steps(r.steps)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::Device;
    use crate::test_support::MockDevice;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn set_dpi_steps_calls_capability() {
        let dev = Arc::new(MockDevice::new("dev1").with_dpi());
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.lock().await.push(dev.clone() as Arc<dyn Device>);

        let msg = json!({"id": "dev1", "steps": [400, 800, 1600]});
        set_dpi_steps(msg, app).await.unwrap();

        let recorded = dev.dpi_last_steps.as_ref().unwrap().lock().unwrap().clone();
        assert_eq!(recorded, Some(vec![400u16, 800, 1600]));
    }

    #[tokio::test]
    async fn set_dpi_steps_errors_without_dpi_capability() {
        let dev = Arc::new(MockDevice::new("dev1")); // no .with_dpi()
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.lock().await.push(dev.clone() as Arc<dyn Device>);

        let msg = json!({"id": "dev1", "steps": [400, 800]});
        let err = set_dpi_steps(msg, app).await.unwrap_err();
        assert!(err.to_string().contains("DPI"));
    }

    #[tokio::test]
    async fn set_dpi_steps_does_not_persist_device_state() {
        // dpi.rs never calls persist_device_state — verify by confirming save_state
        // is not invoked (MockDevice.load_called tracks load, not save, so we
        // simply assert the call succeeds without side-effects on config).
        let dev = Arc::new(MockDevice::new("dev1").with_dpi());
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.lock().await.push(dev.clone() as Arc<dyn Device>);

        let msg = json!({"id": "dev1", "steps": [1200]});
        set_dpi_steps(msg, app.clone()).await.unwrap();

        // Config device_states should remain empty — no persistence happened.
        let cfg = app.config.read().await;
        assert!(cfg.active_profile_data().device_states.get("dev1").is_none());
    }
}
