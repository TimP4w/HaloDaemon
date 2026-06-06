use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

use super::require_device_owned_id;
use crate::state::AppState;

#[derive(Deserialize)]
struct SetRangeReq {
    id: String,
    key: String,
    value: i32,
}

pub async fn set_range(msg: Value, app: Arc<AppState>) -> Result<()> {
    let r: SetRangeReq =
        serde_json::from_value(msg).context("invalid set_range request")?;
    let dev = require_device_owned_id(&r.id, &app).await?;
    dev.as_range()
        .context("device does not support range control")?
        .set_range(&r.key, r.value)
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
    async fn set_range_calls_capability() {
        let dev = Arc::new(MockDevice::new("dev1").with_range());
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.lock().await.push(dev.clone() as Arc<dyn Device>);

        let msg = json!({"id": "dev1", "key": "nc_level", "value": 50});
        set_range(msg, app).await.unwrap();

        let last = dev.range_last_set.as_ref().unwrap().lock().unwrap().clone().unwrap();
        assert_eq!(last, ("nc_level".to_string(), 50));
    }
}
