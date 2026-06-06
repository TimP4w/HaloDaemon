use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

use super::require_device_owned_id;
use crate::state::AppState;

#[derive(Deserialize)]
struct TriggerActionReq {
    id: String,
    key: String,
}

pub async fn trigger_action(msg: Value, app: Arc<AppState>) -> Result<()> {
    let r: TriggerActionReq =
        serde_json::from_value(msg).context("invalid trigger_action request")?;
    let dev = require_device_owned_id(&r.id, &app).await?;
    dev.as_action()
        .context("device does not support actions")?
        .trigger_action(&r.key)
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
    async fn trigger_action_calls_capability() {
        let dev = Arc::new(MockDevice::new("dev1").with_action());
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.lock().await.push(dev.clone() as Arc<dyn Device>);

        let msg = json!({"id": "dev1", "key": "pixel_refresh"});
        trigger_action(msg, app).await.unwrap();

        assert_eq!(dev.action_last_key.as_ref().unwrap().lock().unwrap().as_deref(), Some("pixel_refresh"));
    }
}
