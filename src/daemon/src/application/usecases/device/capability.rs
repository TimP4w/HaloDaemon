// SPDX-License-Identifier: GPL-3.0-or-later
//! Device capability commands.
use crate::domain::events::ChangeSink as _;

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::device::Device;
use crate::domain::profiles::device_state::persist_device_state;
use crate::domain::registry::require_device_owned_id;

/// Wire-level parameters for the "look up a device, apply a value to one
/// capability, maybe persist/broadcast" family of usecases. One variant per
/// `DaemonCommand` setter; the differences between them (validation, whether
/// the result persists or broadcasts) live in [`apply`].
pub enum CapabilityParam {
    Boolean { key: String, value: bool },
    Range { key: String, value: i32 },
    Choice { key: String, selected: usize },
    Action { key: String },
    DpiSteps { steps: Vec<u32> },
    EqPreset { preset_index: usize },
    EqBands { values: Vec<f32> },
}

struct Effects {
    persist: bool,
}

pub async fn set_capability_param(
    id: String,
    param: CapabilityParam,
    app: Arc<AppState>,
) -> Result<()> {
    let dev = require_device_owned_id(&id, &app).await?;
    let effects = apply(dev.as_ref(), &param).await?;
    if effects.persist {
        persist_device_state(&app, dev.as_ref()).await;
    }
    app.record_change(crate::domain::events::Change::Device(id))
        .await;
    Ok(())
}

async fn apply(dev: &dyn Device, param: &CapabilityParam) -> Result<Effects> {
    match param {
        CapabilityParam::Boolean { key, value } => {
            let cap = dev
                .as_boolean()
                .context("device does not support boolean control")?;
            let booleans = cap.get_booleans().await?;
            if let Some(b) = booleans.iter().find(|b| &b.key == key) {
                if b.read_only {
                    anyhow::bail!("boolean '{key}' is read-only");
                }
            }
            cap.set_boolean(key, *value).await?;
            Ok(Effects { persist: true })
        }
        CapabilityParam::Range { key, value } => {
            dev.as_range()
                .context("device does not support range control")?
                .set_range(key, *value)
                .await?;
            Ok(Effects { persist: true })
        }
        CapabilityParam::Choice { key, selected } => {
            dev.as_choice()
                .context("device does not support choice control")?
                .set_choice(key, *selected)
                .await?;
            Ok(Effects { persist: true })
        }
        CapabilityParam::Action { key } => {
            dev.as_action()
                .context("device does not support actions")?
                .trigger_action(key)
                .await?;
            Ok(Effects { persist: false })
        }
        CapabilityParam::DpiSteps { steps } => {
            let steps16 = steps
                .iter()
                .map(|&s| {
                    u16::try_from(s)
                        .map_err(|_| anyhow::anyhow!("DPI step {s} exceeds maximum value 65535"))
                })
                .collect::<Result<Vec<u16>>>()?;
            dev.as_dpi()
                .context("device does not support DPI control")?
                .set_dpi_steps(steps16)
                .await?;
            Ok(Effects { persist: false })
        }
        CapabilityParam::EqPreset { preset_index } => {
            dev.as_equalizer()
                .context("device does not support equalizer control")?
                .set_eq_preset(*preset_index)
                .await?;
            Ok(Effects { persist: true })
        }
        CapabilityParam::EqBands { values } => {
            let cap = dev
                .as_equalizer()
                .context("device does not support equalizer control")?;
            let eq = cap.get_equalizer().await?;
            anyhow::ensure!(
                values.len() == eq.bands.len(),
                "expected {} EQ bands, got {}",
                eq.bands.len(),
                values.len()
            );
            anyhow::ensure!(
                values.iter().all(|v| v.is_finite()),
                "EQ band values must be finite"
            );
            cap.set_eq_bands(values).await?;
            Ok(Effects { persist: true })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::test_support::MockDevice;
    use halod_shared::types::{Boolean, EqBand, Equalizer};

    fn make_app(devices: Vec<Arc<dyn Device>>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        *app.device_registry.try_write().unwrap() = devices;
        app
    }

    // ── Boolean ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_boolean_calls_capability_for_writable_key() {
        let dev = Arc::new(MockDevice::new("dev1").with_booleans(vec![Boolean {
            key: "sidetone".into(),
            label: "Sidetone".into(),
            value: false,
            read_only: false,
            category: String::new(),
            visible_when: None,
        }]));
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);
        set_capability_param(
            "dev1".into(),
            CapabilityParam::Boolean {
                key: "sidetone".into(),
                value: true,
            },
            app,
        )
        .await
        .unwrap();
        let last = dev
            .bool_last_set
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        assert_eq!(last, ("sidetone".to_string(), true));
    }

    #[tokio::test]
    async fn set_boolean_rejects_read_only_key() {
        let dev = Arc::new(MockDevice::new("dev1").with_booleans(vec![Boolean {
            key: "muted".into(),
            label: "Muted".into(),
            value: true,
            read_only: true,
            category: String::new(),
            visible_when: None,
        }]));
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);
        let err = set_capability_param(
            "dev1".into(),
            CapabilityParam::Boolean {
                key: "muted".into(),
                value: false,
            },
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("read-only"), "got: {err}");
        assert!(
            dev.bool_last_set
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .is_none(),
            "set_boolean should not have been called"
        );
    }

    // ── Range ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_range_calls_capability() {
        let dev = Arc::new(MockDevice::new("dev1").with_range());
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::Range {
                key: "nc_level".into(),
                value: 50,
            },
            app,
        )
        .await
        .unwrap();

        let last = dev
            .range_last_set
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        assert_eq!(last, ("nc_level".to_string(), 50));
    }

    #[tokio::test]
    async fn set_range_publishes_new_profile_override() {
        let dev = Arc::new(MockDevice::new("dev1").with_range());
        dev.range.as_ref().unwrap().record("nc_level", 0);
        let app = make_app(vec![dev as Arc<dyn Device>]);
        {
            let mut cfg = app.config.write().await;
            cfg.active_profile_data_mut().device_states.insert(
                "dev1".into(),
                serde_json::json!({ "range": { "nc_level": 0 } }),
            );
            cfg.profiles.insert("Gaming".into(), Default::default());
            cfg.active_profile = "Gaming".into();
        }
        let mut transactions = app.data_bus.subscribe_transactions();

        set_capability_param(
            "dev1".into(),
            CapabilityParam::Range {
                key: "nc_level".into(),
                value: 50,
            },
            app,
        )
        .await
        .unwrap();

        let transaction = transactions.recv().await.unwrap();
        let profiles = transaction
            .upserts
            .into_iter()
            .find_map(|record| match record.value {
                halod_shared::bus::BusValue::Profiles(profiles) => Some(profiles),
                _ => None,
            })
            .expect("persisted capability mutation must publish profile overrides");
        assert_eq!(
            profiles.overrides.device_capabilities["dev1"],
            vec!["range".to_string()]
        );
    }

    // ── Choice ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_choice_calls_capability() {
        let dev = Arc::new(MockDevice::new("dev1").with_choice());
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::Choice {
                key: "nc_mode".into(),
                selected: 2,
            },
            app,
        )
        .await
        .unwrap();

        let last = dev
            .choice_last_set
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        assert_eq!(last, ("nc_mode".to_string(), 2));
    }

    #[tokio::test]
    async fn set_choice_publishes_new_profile_override() {
        let dev = Arc::new(MockDevice::new("dev1").with_choice());
        dev.choice.as_ref().unwrap().record("nc_mode", 0);
        let app = make_app(vec![dev as Arc<dyn Device>]);
        {
            let mut cfg = app.config.write().await;
            cfg.active_profile_data_mut().device_states.insert(
                "dev1".into(),
                serde_json::json!({ "choice": { "nc_mode": 0 } }),
            );
            cfg.profiles.insert("Gaming".into(), Default::default());
            cfg.active_profile = "Gaming".into();
        }
        let mut transactions = app.data_bus.subscribe_transactions();

        set_capability_param(
            "dev1".into(),
            CapabilityParam::Choice {
                key: "nc_mode".into(),
                selected: 2,
            },
            app,
        )
        .await
        .unwrap();

        let transaction = transactions.recv().await.unwrap();
        let profiles = transaction
            .upserts
            .into_iter()
            .find_map(|record| match record.value {
                halod_shared::bus::BusValue::Profiles(profiles) => Some(profiles),
                _ => None,
            })
            .expect("persisted capability mutation must publish profile overrides");
        assert_eq!(
            profiles.overrides.device_capabilities["dev1"],
            vec!["choice".to_string()]
        );
    }

    // ── Action ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn trigger_action_calls_capability() {
        let dev = Arc::new(MockDevice::new("dev1").with_action());
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::Action {
                key: "pixel_refresh".into(),
            },
            app,
        )
        .await
        .unwrap();

        assert_eq!(
            dev.action_last_key
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .as_deref(),
            Some("pixel_refresh")
        );
    }

    // ── DpiSteps ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_dpi_steps_calls_capability() {
        let dev = Arc::new(MockDevice::new("dev1").with_dpi());
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::DpiSteps {
                steps: vec![400, 800, 1600],
            },
            app,
        )
        .await
        .unwrap();

        let recorded = dev.dpi_last_steps.as_ref().unwrap().lock().unwrap().clone();
        assert_eq!(recorded, Some(vec![400u16, 800, 1600]));
    }

    #[tokio::test]
    async fn set_dpi_steps_errors_without_dpi_capability() {
        let dev = Arc::new(MockDevice::new("dev1")); // no .with_dpi()
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        let err = set_capability_param(
            "dev1".into(),
            CapabilityParam::DpiSteps {
                steps: vec![400, 800],
            },
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("DPI"));
    }

    #[tokio::test]
    async fn set_dpi_steps_does_not_persist_device_state() {
        let dev = Arc::new(MockDevice::new("dev1").with_dpi());
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::DpiSteps { steps: vec![1200] },
            app.clone(),
        )
        .await
        .unwrap();

        let cfg = app.config.read().await;
        assert!(!cfg.active_profile_data().device_states.contains_key("dev1"));
    }

    // ── Equalizer ────────────────────────────────────────────────────────

    fn make_eq(band_count: usize) -> Equalizer {
        Equalizer {
            presets: vec![],
            selected_preset: 0,
            editable: true,
            bands: (0..band_count)
                .map(|i| EqBand {
                    index: i,
                    label: String::new(),
                    min: -10.0,
                    max: 10.0,
                    step: 0.5,
                    value: 0.0,
                })
                .collect(),
        }
    }

    fn eq_dev(id: &str) -> Arc<MockDevice> {
        Arc::new(MockDevice::new(id).with_equalizer(make_eq(10)))
    }

    #[tokio::test]
    async fn set_eq_preset_calls_capability() {
        let dev = eq_dev("dev1");
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::EqPreset { preset_index: 2 },
            app,
        )
        .await
        .unwrap();

        assert_eq!(
            *dev.eq_last_preset.as_ref().unwrap().lock().unwrap(),
            Some(2)
        );
    }

    #[tokio::test]
    async fn set_eq_preset_publishes_updated_device_record() {
        let dev = eq_dev("arctis");
        let app = make_app(vec![dev as Arc<dyn Device>]);
        let mut transactions = app.data_bus.subscribe_transactions();

        set_capability_param(
            "arctis".into(),
            CapabilityParam::EqPreset { preset_index: 2 },
            app,
        )
        .await
        .unwrap();

        let transaction = transactions.recv().await.unwrap();
        let device = transaction
            .upserts
            .into_iter()
            .find_map(|record| match record.value {
                halod_shared::bus::BusValue::Device(device) => Some(device),
                _ => None,
            })
            .expect("capability mutation must publish its device record");
        let equalizer = device
            .capabilities
            .into_iter()
            .find_map(|capability| match capability {
                halod_shared::types::DeviceCapability::Equalizer(equalizer) => Some(equalizer),
                _ => None,
            })
            .expect("updated device record must contain equalizer state");
        assert_eq!(equalizer.selected_preset, 2);
    }

    #[tokio::test]
    async fn set_eq_preset_persists_state() {
        let dev = eq_dev("dev1");
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::EqPreset { preset_index: 1 },
            app.clone(),
        )
        .await
        .unwrap();

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
        let dev = eq_dev("dev2");
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        let values = vec![1.0f32; 10];
        set_capability_param("dev2".into(), CapabilityParam::EqBands { values }, app)
            .await
            .unwrap();

        assert!(dev
            .eq_last_bands
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn set_eq_bands_persists_state() {
        let dev = eq_dev("dev2");
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        // set a preset first so current_state is non-null
        set_capability_param(
            "dev2".into(),
            CapabilityParam::EqPreset { preset_index: 4 },
            app.clone(),
        )
        .await
        .unwrap();
        let values = vec![0.5f32; 10];
        set_capability_param(
            "dev2".into(),
            CapabilityParam::EqBands { values },
            app.clone(),
        )
        .await
        .unwrap();

        let cfg = app.config.read().await;
        assert!(
            cfg.active_profile_data().device_states.contains_key("dev2"),
            "state must be saved after set_eq_bands"
        );
    }

    #[tokio::test]
    async fn set_eq_bands_rejects_wrong_count() {
        let dev = eq_dev("dev3");
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        // device reports 10 bands; sending 5 must fail
        let result = set_capability_param(
            "dev3".into(),
            CapabilityParam::EqBands {
                values: vec![0.0f32; 5],
            },
            app,
        )
        .await;
        assert!(result.is_err(), "mismatched band count must be rejected");
    }

    #[tokio::test]
    async fn set_eq_bands_rejects_non_finite() {
        let dev = eq_dev("dev4");
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        let mut values = vec![0.0f32; 10];
        values[3] = f32::NAN;
        let result =
            set_capability_param("dev4".into(), CapabilityParam::EqBands { values }, app).await;
        assert!(result.is_err(), "NaN band value must be rejected");
    }
}
