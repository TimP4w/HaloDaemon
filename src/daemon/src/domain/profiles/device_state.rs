// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::Arc;

use crate::application::state::AppState;
use crate::infrastructure::drivers::Device;
use halod_shared::types::DEFAULT_PROFILE_NAME;

/// Merge into `entry` only the keys of `new_state` that differ from `effective`.
/// Only ADDS overrides — un-tracking is explicit via `remove_profile_override`,
/// so editing a value back to the default's never silently drops the override.
pub(crate) fn record_changed_overrides(
    entry: &mut serde_json::Value,
    new_state: &serde_json::Value,
    effective: &serde_json::Value,
) {
    let Some(new_obj) = new_state.as_object() else {
        return;
    };
    if !entry.is_object() {
        log::warn!("device_states entry was not a JSON object; resetting to empty object");
        *entry = serde_json::Value::Object(serde_json::Map::new());
    }
    let obj = entry
        .as_object_mut()
        .expect("entry coerced to a JSON object above");
    for (k, v) in new_obj {
        if effective.get(k) != Some(v) {
            obj.insert(k.clone(), v.clone());
        }
    }
}

/// Copy into `default` any capability in `state` it doesn't already track.
/// Existing keys are never overwritten — only the first value seen is kept.
fn seed_missing_default_baselines(
    cfg: &mut crate::config::Config,
    device_id: &str,
    state: &serde_json::Value,
) {
    let Some(state_obj) = state.as_object() else {
        return;
    };
    let default = cfg
        .profiles
        .entry(DEFAULT_PROFILE_NAME.to_string())
        .or_default();
    let entry = default
        .device_states
        .entry(device_id.to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(entry_obj) = entry.as_object_mut() else {
        return;
    };
    for (k, v) in state_obj {
        entry_obj.entry(k.clone()).or_insert_with(|| v.clone());
    }
}

pub async fn persist_device_state(app: &Arc<AppState>, device: &dyn Device) {
    let state = device.save_state().await;
    if state.is_null() {
        log::warn!(
            "[{}] persist_device_state: save_state() returned null, state will not be saved",
            device.id()
        );
        return;
    }
    let device_id = device.id().to_owned();
    let mut validation_profile = crate::domain::profiles::model::Profile::default();
    validation_profile
        .device_states
        .insert(device_id.clone(), state.clone());
    if let Err(e) =
        crate::domain::profiles::validate::validate_profile("persisted state", &validation_profile)
    {
        log::warn!("[{device_id}] refusing to persist invalid device state: {e:#}");
        return;
    }
    log::debug!("[{device_id}] persisting state");
    {
        let mut cfg = app.config.write().await;
        if cfg.active_profile == DEFAULT_PROFILE_NAME {
            cfg.active_profile_data_mut()
                .device_states
                .insert(device_id, state);
        } else {
            // Ensure `default` has a baseline so overrides always have a fallback.
            seed_missing_default_baselines(&mut cfg, &device_id, &state);

            // Record only the capabilities that changed vs. the effective state.
            let effective = cfg.effective_device_state(&device_id);
            let profile = cfg.active_profile_data_mut();
            let entry = profile
                .device_states
                .entry(device_id.clone())
                .or_insert_with(|| serde_json::json!({}));
            record_changed_overrides(entry, &state, &effective);
            let empty = entry.as_object().map(|o| o.is_empty()).unwrap_or(true);
            if empty {
                profile.device_states.remove(&device_id);
            }
        }
    }
    app.request_config_save();
}

#[cfg(test)]
mod override_diff_tests {
    use super::{record_changed_overrides, seed_missing_default_baselines};
    use crate::config::Config;
    use halod_shared::types::DEFAULT_PROFILE_NAME;
    use serde_json::json;

    #[test]
    fn seeds_missing_keys_into_default() {
        let mut cfg = Config::default();
        seed_missing_default_baselines(
            &mut cfg,
            "dev1",
            &json!({ "rgb": {"m": "static"}, "fan_curve": {"a": 1} }),
        );
        let dflt = &cfg.profiles[DEFAULT_PROFILE_NAME].device_states["dev1"];
        assert_eq!(dflt["rgb"]["m"], "static");
        assert_eq!(dflt["fan_curve"]["a"], 1);
    }

    #[test]
    fn never_overwrites_existing_default_baseline() {
        let mut cfg = Config::default();
        cfg.profiles
            .get_mut(DEFAULT_PROFILE_NAME)
            .unwrap()
            .device_states
            .insert("dev1".into(), json!({ "rgb": {"m": "wave"} }));
        seed_missing_default_baselines(
            &mut cfg,
            "dev1",
            &json!({ "rgb": {"m": "static"}, "dpi": {"s": [800]} }),
        );
        let dflt = &cfg.profiles[DEFAULT_PROFILE_NAME].device_states["dev1"];
        assert_eq!(dflt["rgb"]["m"], "wave", "existing baseline preserved");
        assert_eq!(dflt["dpi"]["s"][0], 800, "missing key seeded");
    }

    #[test]
    fn records_only_changed_keys() {
        let effective = json!({ "rgb": {"m": "static"}, "fan_curve": {"a": 1} });
        let new_state = json!({ "rgb": {"m": "static"}, "fan_curve": {"a": 2} });
        let mut entry = json!({});
        record_changed_overrides(&mut entry, &new_state, &effective);
        assert!(
            entry.get("rgb").is_none(),
            "unchanged rgb must not be tracked"
        );
        assert_eq!(entry["fan_curve"]["a"], 2);
    }

    #[test]
    fn records_new_capability_absent_from_effective() {
        let effective = json!(null);
        let new_state = json!({ "fan_curve": {"a": 9} });
        let mut entry = json!({});
        record_changed_overrides(&mut entry, &new_state, &effective);
        assert_eq!(entry["fan_curve"]["a"], 9);
    }

    #[test]
    fn non_object_entry_is_reset_then_recorded() {
        let effective = json!(null);
        let new_state = json!({ "fan_curve": {"a": 9} });
        let mut entry = json!("garbage");
        record_changed_overrides(&mut entry, &new_state, &effective);
        assert!(
            entry.is_object(),
            "non-object entry must be reset to object"
        );
        assert_eq!(entry["fan_curve"]["a"], 9);
    }

    #[tokio::test]
    async fn persist_device_state_partial_override_falls_back_on_profile_switch() {
        use crate::application::state::AppState;
        use crate::infrastructure::drivers::Device;
        use crate::test_support::MockDevice;
        use std::sync::Arc;

        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_choice().with_fan());
        dev.choice.as_ref().unwrap().record("mode", 1);
        dev.fan.as_ref().unwrap().set_curve(
            "default".to_string(),
            crate::domain::cooling::model::FanCurveRecord {
                sensor_id: None,
                points: vec![(20.0, 25.0), (80.0, 100.0)],
            },
        );
        app.device_registry
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        // Baseline saved while on the default profile.
        super::persist_device_state(&app, dev.as_ref()).await;

        // Switch to a non-default profile and override only the fan curve.
        {
            let mut cfg = app.config.write().await;
            cfg.profiles.insert(
                "Gaming".into(),
                crate::domain::profiles::model::Profile::default(),
            );
            cfg.active_profile = "Gaming".into();
        }
        dev.fan.as_ref().unwrap().set_curve(
            "default".to_string(),
            crate::domain::cooling::model::FanCurveRecord {
                sensor_id: None,
                points: vec![(30.0, 50.0)],
            },
        );
        super::persist_device_state(&app, dev.as_ref()).await;

        let cfg = app.config.read().await;
        let effective = cfg.effective_device_state("dev1");
        assert_eq!(
            effective["fan_curve"]["default"]["points"][0][1], 50.0,
            "gaming override must win for the changed capability"
        );
        assert_eq!(
            effective["choice"]["mode"], 1,
            "capability untouched in gaming must fall back to the default baseline"
        );
    }
}
