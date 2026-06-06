pub mod action;
pub mod app_rules;
pub mod running_apps;
pub mod boolean;
pub mod settings;
pub mod canvas;
pub mod chain;
pub mod choice;
pub mod debug;
pub mod dpi;
pub mod equalizer;
pub mod fan;
pub mod fan_curve;
pub mod key_remap;
pub mod lcd;
pub mod lcd_engine;
pub mod onboard_profiles;
pub mod profiles;
pub mod range;
pub mod rename;
pub mod rgb;
pub mod visibility;
pub mod registration;

use anyhow::Result;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

use crate::config::DeviceRecord;
use crate::discovery::discover_devices;
use crate::drivers::Device;
use crate::state::AppState;
use halod_protocol::types::EffectParamValue;

/// Parse the `params` field of a JSON message into an effect/template params map.
/// Entries that fail to deserialize into an `EffectParamValue` are skipped.
pub(crate) fn parse_params(v: &Value) -> HashMap<String, EffectParamValue> {
    let Some(obj) = v.as_object() else {
        return HashMap::new();
    };
    obj.iter()
        .filter_map(|(k, v)| {
            let param = serde_json::from_value::<EffectParamValue>(v.clone()).ok()?;
            Some((k.clone(), param))
        })
        .collect()
}

/// Looks up a device by `msg["id"]`, returns an error if the id is missing or not found.
pub fn require_device<'a>(
    msg: &Value,
    devices: &'a [Arc<dyn Device>],
) -> Result<&'a Arc<dyn Device>> {
    let id = msg["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing id"))?;
    devices
        .iter()
        .find(|d| d.id() == id)
        .ok_or_else(|| anyhow::anyhow!("device not found: {id}"))
}

/// Looks up the device named by `msg["id"]` and returns an owned clone,
/// holding the `devices` lock only for the lookup.
pub async fn require_device_owned(msg: &Value, app: &AppState) -> Result<Arc<dyn Device>> {
    let devices = app.devices.lock().await;
    Ok(require_device(msg, &devices)?.clone())
}

/// Look up the device named by `id` and return an owned clone.
/// Error if the device is not found.
pub async fn require_device_owned_id(id: &str, app: &AppState) -> Result<Arc<dyn Device>> {
    let msg = serde_json::json!({ "id": id });
    require_device_owned(&msg, app).await
}

/// Get or create the `DeviceRecord` for `device_id`. When the device is
/// currently registered we seed the record from its `name/vendor/model`; when
/// it isn't (offline / never-seen) we leave a `DeviceRecord::default()`.
pub fn ensure_record<'a>(
    known: &'a mut HashMap<String, DeviceRecord>,
    device_id: &str,
    device: Option<&dyn Device>,
) -> &'a mut DeviceRecord {
    known.entry(device_id.to_string()).or_insert_with(|| match device {
        Some(d) => DeviceRecord {
            name: d.name().to_string(),
            vendor: d.vendor().to_string(),
            model: d.model().to_string(),
            active_state: Default::default(),
        },
        None => DeviceRecord::default(),
    })
}

pub async fn persist_device_state(app: &Arc<AppState>, device: &dyn Device) {
    let state = device.save_state().await;
    if state.is_null() {
        return;
    }
    log::debug!("[{}] persisting state", device.id());
    let _cfg_snap = {
        let mut cfg = app.config.write().await;
        cfg.active_profile_data_mut()
            .device_states
            .insert(device.id(), state);
        cfg.clone()
    };
    #[cfg(not(test))]
    app.request_config_save(_cfg_snap);
}

pub async fn seed_known_devices(app: Arc<AppState>) {
    let devices = app.devices.lock().await.clone();
    let mut cfg = app.config.write().await;
    for device in &devices {
        ensure_record(&mut cfg.known_devices, &device.id(), Some(device.as_ref()));
    }
}

pub async fn initialize_app_state(app: Arc<AppState>) {
    discover_devices(app.clone()).await;
    seed_known_devices(app.clone()).await;
    chain::restore_saved_chains(app.clone()).await;
    profiles::load_active_profile(app.clone()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    struct StubDevice {
        id: &'static str,
    }

    #[async_trait]
    impl Device for StubDevice {
        fn id(&self) -> String {
            self.id.to_string()
        }
        fn name(&self) -> &str {
            "stub"
        }
        fn vendor(&self) -> &str {
            "stub"
        }
        fn model(&self) -> &str {
            "stub"
        }
        async fn initialize(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<crate::drivers::CapabilityRef<'_>> { vec![] }
    }

    fn devices(ids: &[&'static str]) -> Vec<Arc<dyn Device>> {
        ids.iter()
            .map(|id| Arc::new(StubDevice { id }) as Arc<dyn Device>)
            .collect()
    }

    #[test]
    fn require_device_returns_matching_device() {
        let devs = devices(&["dev_a", "dev_b"]);
        let msg = json!({"id": "dev_b"});
        let found = require_device(&msg, &devs).unwrap();
        assert_eq!(found.id(), "dev_b");
    }

    #[test]
    fn require_device_errors_when_id_missing() {
        let devs = devices(&["dev_a"]);
        let msg = json!({});
        assert!(require_device(&msg, &devs).is_err());
    }

    #[test]
    fn require_device_errors_when_not_found() {
        let devs = devices(&["dev_a"]);
        let msg = json!({"id": "dev_z"});
        let err = require_device(&msg, &devs).err().expect("expected error");
        assert!(err.to_string().contains("dev_z"));
    }
}
