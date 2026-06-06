use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use super::require_device_owned;
use crate::state::AppState;

pub async fn set_eq_preset(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let cap = device
        .as_equalizer()
        .ok_or_else(|| anyhow::anyhow!("device does not support equalizer control"))?;
    let preset_index = msg["preset_index"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing or invalid preset_index"))? as usize;
    cap.set_eq_preset(preset_index).await?;
    super::persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

pub async fn set_eq_bands(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let cap = device
        .as_equalizer()
        .ok_or_else(|| anyhow::anyhow!("device does not support equalizer control"))?;
    let values_raw = msg["values"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing or invalid values array"))?;
    let values: Vec<f32> = values_raw
        .iter()
        .map(|v| v.as_f64().unwrap_or(0.0) as f32)
        .collect();
    cap.set_eq_bands(&values).await?;
    super::persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::{CapabilityRef, EqualizerCapability, Device};
    use async_trait::async_trait;
    use halod_protocol::types::{EqBand, Equalizer};
    use serde_json::json;
    use std::sync::Mutex;

    struct MockEqDevice {
        id: &'static str,
        last_preset: Mutex<Option<usize>>,
        last_bands: Mutex<Option<Vec<f32>>>,
        saved_preset: Mutex<Option<usize>>,
    }

    impl MockEqDevice {
        fn new(id: &'static str) -> Self {
            Self {
                id,
                last_preset: Mutex::new(None),
                last_bands: Mutex::new(None),
                saved_preset: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl Device for MockEqDevice {
        fn id(&self) -> String { self.id.to_string() }
        fn name(&self) -> &str { "mock" }
        fn vendor(&self) -> &str { "mock" }
        fn model(&self) -> &str { "mock" }
        async fn initialize(&self) -> anyhow::Result<bool> { Ok(true) }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::Equalizer(self)]
        }
    }

    #[async_trait]
    impl EqualizerCapability for MockEqDevice {
        async fn get_equalizer(&self) -> anyhow::Result<halod_protocol::types::Equalizer> {
            unimplemented!()
        }
        async fn set_eq_preset(&self, preset_index: usize) -> anyhow::Result<()> {
            *self.last_preset.lock().unwrap() = Some(preset_index);
            *self.saved_preset.lock().unwrap() = Some(preset_index);
            Ok(())
        }
        async fn set_eq_bands(&self, values: &[f32]) -> anyhow::Result<()> {
            *self.last_bands.lock().unwrap() = Some(values.to_vec());
            Ok(())
        }
        fn current_state(&self) -> Option<Equalizer> {
            let preset = (*self.saved_preset.lock().unwrap())?;
            let band_values = self.last_bands.lock().unwrap()
                .as_ref()
                .cloned()
                .unwrap_or_else(|| vec![0.0; 10]);
            Some(Equalizer {
                presets: vec![],
                selected_preset: preset,
                bands: band_values.into_iter().enumerate().map(|(i, v)| EqBand {
                    index: i,
                    label: String::new(),
                    min: -10.0,
                    max: 10.0,
                    step: 0.5,
                    value: v,
                }).collect(),
            })
        }
    }

    #[tokio::test]
    async fn set_eq_preset_calls_capability() {
        let dev = Arc::new(MockEqDevice::new("dev1"));
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.lock().await.push(dev.clone() as Arc<dyn Device>);

        set_eq_preset(json!({"id": "dev1", "preset_index": 2}), app).await.unwrap();

        assert_eq!(*dev.last_preset.lock().unwrap(), Some(2));
    }

    #[tokio::test]
    async fn set_eq_preset_persists_state() {
        let dev = Arc::new(MockEqDevice::new("dev1"));
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.lock().await.push(dev.clone() as Arc<dyn Device>);

        set_eq_preset(json!({"id": "dev1", "preset_index": 1}), app.clone()).await.unwrap();

        let cfg = app.config.read().await;
        let device_state = cfg
            .active_profile_data()
            .device_states
            .get("dev1")
            .expect("state must be saved after set_eq_preset");
        assert_eq!(device_state["equalizer"]["preset"].as_u64().unwrap(), 1);
    }

    #[tokio::test]
    async fn set_eq_bands_calls_capability() {
        let dev = Arc::new(MockEqDevice::new("dev2"));
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.lock().await.push(dev.clone() as Arc<dyn Device>);

        let values = vec![1.0f64; 10];
        set_eq_bands(json!({"id": "dev2", "values": values}), app).await.unwrap();

        assert!(dev.last_bands.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn set_eq_bands_persists_state() {
        let dev = Arc::new(MockEqDevice::new("dev2"));
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.lock().await.push(dev.clone() as Arc<dyn Device>);

        // set a preset first so save_state is non-null
        set_eq_preset(json!({"id": "dev2", "preset_index": 4}), app.clone()).await.unwrap();
        let values = vec![0.5f64; 10];
        set_eq_bands(json!({"id": "dev2", "values": values}), app.clone()).await.unwrap();

        let cfg = app.config.read().await;
        assert!(
            cfg.active_profile_data().device_states.contains_key("dev2"),
            "state must be saved after set_eq_bands"
        );
    }
}
