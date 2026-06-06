use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

use super::require_device_owned_id;
use crate::state::AppState;

#[derive(Deserialize)]
struct SetChoiceReq {
    id: String,
    key: String,
    selected: usize,
}

pub async fn set_choice(msg: Value, app: Arc<AppState>) -> Result<()> {
    let r: SetChoiceReq =
        serde_json::from_value(msg).context("invalid set_choice request")?;
    let dev = require_device_owned_id(&r.id, &app).await?;
    dev.as_choice()
        .context("device does not support choice control")?
        .set_choice(&r.key, r.selected)
        .await?;
    super::persist_device_state(&app, dev.as_ref()).await;
    Ok(())
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
    async fn set_choice_calls_capability() {
        let dev = Arc::new(MockDevice::new("dev1").with_choice());
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.lock().await.push(dev.clone() as Arc<dyn Device>);

        let msg = json!({"id": "dev1", "key": "nc_mode", "selected": 2});
        set_choice(msg, app).await.unwrap();

        let last = dev.choice_last_set.as_ref().unwrap().lock().unwrap().clone().unwrap();
        assert_eq!(last, ("nc_mode".to_string(), 2));
    }
}
