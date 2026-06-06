use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

use super::require_device_owned_id;
use crate::state::AppState;

#[derive(Deserialize)]
struct SetBooleanReq {
    id: String,
    key: String,
    value: bool,
}

pub async fn set_boolean(msg: Value, app: Arc<AppState>) -> Result<()> {
    let r: SetBooleanReq =
        serde_json::from_value(msg).context("invalid set_boolean request")?;
    let dev = require_device_owned_id(&r.id, &app).await?;
    let cap = dev
        .as_boolean()
        .context("device does not support boolean control")?;
    // Check read_only before sending to the device
    let booleans = cap.get_booleans().await?;
    if let Some(b) = booleans.iter().find(|b| b.key == r.key) {
        if b.read_only {
            anyhow::bail!("boolean '{}' is read-only", r.key);
        }
    }
    cap.set_boolean(&r.key, r.value).await?;
    // Persist so toggles (e.g. host mode) survive reconnects and profile loads.
    super::persist_device_state(&app, dev.as_ref()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::Device;
    use crate::test_support::MockDevice;
    use halod_protocol::types::Boolean;
    use serde_json::json;
    use std::sync::Arc;

    fn make_app(dev: Arc<MockDevice>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        let devices: Vec<Arc<dyn Device>> = vec![dev as Arc<dyn Device>];
        *app.devices.try_lock().unwrap() = devices;
        app
    }

    #[tokio::test]
    async fn set_boolean_calls_capability_for_writable_key() {
        let dev = Arc::new(MockDevice::new("dev1").with_booleans(vec![
            Boolean { key: "sidetone".into(), label: "Sidetone".into(), value: false, read_only: false, category: String::new() },
        ]));
        let app = make_app(dev.clone());
        set_boolean(json!({"id": "dev1", "key": "sidetone", "value": true}), app).await.unwrap();
        let last = dev.bool_last_set.as_ref().unwrap().lock().unwrap().clone().unwrap();
        assert_eq!(last, ("sidetone".to_string(), true));
    }

    #[tokio::test]
    async fn set_boolean_rejects_read_only_key() {
        let dev = Arc::new(MockDevice::new("dev1").with_booleans(vec![
            Boolean { key: "muted".into(), label: "Muted".into(), value: true, read_only: true, category: String::new() },
        ]));
        let app = make_app(dev.clone());
        let err = set_boolean(json!({"id": "dev1", "key": "muted", "value": false}), app).await.unwrap_err();
        assert!(err.to_string().contains("read-only"), "got: {err}");
        assert!(dev.bool_last_set.as_ref().unwrap().lock().unwrap().is_none(), "set_boolean should not have been called");
    }
}
