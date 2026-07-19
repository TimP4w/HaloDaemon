// SPDX-License-Identifier: GPL-3.0-or-later
use crate::config::Config;
use crate::domain::lighting::model::ProfileLighting;
use halod_shared::types::DEFAULT_PROFILE_NAME;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub device_states: HashMap<String, Value>,

    #[serde(default)]
    pub lighting: ProfileLighting,
}

impl Config {
    pub fn active_profile_data(&self) -> &Profile {
        static FALLBACK: std::sync::OnceLock<Profile> = std::sync::OnceLock::new();
        self.profiles
            .get(&self.active_profile)
            .or_else(|| self.profiles.get(DEFAULT_PROFILE_NAME))
            .or_else(|| self.profiles.values().next())
            .unwrap_or_else(|| FALLBACK.get_or_init(Profile::default))
    }

    pub fn active_profile_data_mut(&mut self) -> &mut Profile {
        let key = self.active_profile.clone();
        self.profiles.entry(key).or_default()
    }

    pub fn profile_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.profiles.keys().cloned().collect();
        names.sort();
        names
    }

    /// Which device capabilities and the canvas the active (non-default)
    /// profile overrides, for the GUI's override badges. Empty when the active
    /// profile is `default` (nothing is overridable there).
    pub fn profile_overrides(&self) -> halod_shared::types::ProfileOverrides {
        use halod_shared::types::ProfileOverrides;
        let mut ov = ProfileOverrides {
            active_is_default: self.active_profile == DEFAULT_PROFILE_NAME,
            ..Default::default()
        };
        if ov.active_is_default {
            return ov;
        }
        if let Some(p) = self.profiles.get(&self.active_profile) {
            for (device_id, state) in &p.device_states {
                if let Some(obj) = state.as_object() {
                    let keys: Vec<String> = obj.keys().cloned().collect();
                    if !keys.is_empty() {
                        ov.device_capabilities.insert(device_id.clone(), keys);
                    }
                }
            }
            ov.canvas = p.lighting.canvas.is_some();
        }
        ov
    }

    /// Effective per-device capability state: the default profile's object
    /// overlaid with the active profile's overrides (active wins per key).
    /// Returns `Value::Null` when neither profile has any state for the device.
    pub fn effective_device_state(&self, device_id: &str) -> Value {
        let default_obj = self
            .profiles
            .get(DEFAULT_PROFILE_NAME)
            .and_then(|p| p.device_states.get(device_id));
        let active_obj = if self.active_profile == DEFAULT_PROFILE_NAME {
            None
        } else {
            self.profiles
                .get(&self.active_profile)
                .and_then(|p| p.device_states.get(device_id))
        };
        match (default_obj, active_obj) {
            (None, None) => Value::Null,
            (Some(d), None) => d.clone(),
            (None, Some(a)) => a.clone(),
            (Some(d), Some(a)) => {
                let mut merged = d.as_object().cloned().unwrap_or_default();
                if let Some(a_obj) = a.as_object() {
                    for (k, v) in a_obj {
                        merged.insert(k.clone(), v.clone());
                    }
                }
                Value::Object(merged)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_profile_data_falls_back_to_default_then_any_then_static() {
        let mut cfg = Config::default();
        cfg.profiles.clear();
        cfg.active_profile = "ghost".to_string();

        // No profiles at all: falls back to the static default.
        assert_eq!(cfg.active_profile_data().device_states.len(), 0);

        // Only a non-default, non-active profile exists: falls back to it.
        let mut other = Profile::default();
        other.device_states.insert("dev1".into(), Value::Bool(true));
        cfg.profiles.insert("other".into(), other);
        assert!(cfg.active_profile_data().device_states.contains_key("dev1"));

        // Default profile exists alongside: default wins over "any".
        let mut default_profile = Profile::default();
        default_profile
            .device_states
            .insert("dev2".into(), Value::Bool(true));
        cfg.profiles
            .insert(DEFAULT_PROFILE_NAME.to_string(), default_profile);
        assert!(cfg.active_profile_data().device_states.contains_key("dev2"));

        // Active profile exists: it wins over default.
        let mut active_profile = Profile::default();
        active_profile
            .device_states
            .insert("dev3".into(), Value::Bool(true));
        cfg.profiles.insert("ghost".into(), active_profile);
        assert!(cfg.active_profile_data().device_states.contains_key("dev3"));
    }

    #[test]
    fn active_profile_data_mut_creates_entry_when_missing() {
        let mut cfg = Config {
            active_profile: "new_profile".to_string(),
            ..Config::default()
        };
        // Confirm the profile doesn't exist yet.
        assert!(!cfg.profiles.contains_key("new_profile"));

        let profile = cfg.active_profile_data_mut();
        assert!(profile.device_states.is_empty());

        // Now it should exist.
        assert!(cfg.profiles.contains_key("new_profile"));
    }

    #[test]
    fn effective_device_state_overlays_active_over_default() {
        let mut cfg = Config::default();
        cfg.profiles
            .get_mut(DEFAULT_PROFILE_NAME)
            .unwrap()
            .device_states
            .insert(
                "dev1".into(),
                serde_json::json!({ "rgb": {"mode": "static"}, "fan_curve": {"a": 1} }),
            );
        let mut gaming = Profile::default();
        gaming
            .device_states
            .insert("dev1".into(), serde_json::json!({ "fan_curve": {"a": 2} }));
        cfg.profiles.insert("gaming".into(), gaming);
        cfg.active_profile = "gaming".into();

        let eff = cfg.effective_device_state("dev1");
        // rgb inherited from default, fan_curve overridden by gaming
        assert_eq!(eff["rgb"]["mode"], "static");
        assert_eq!(eff["fan_curve"]["a"], 2);
    }

    #[test]
    fn profile_overrides_maps_active_profile() {
        let mut cfg = Config::default();
        let mut g = Profile::default();
        g.device_states
            .insert("dev1".into(), serde_json::json!({ "fan_curve": {"a": 1} }));
        g.lighting.canvas = Some(Default::default());
        cfg.profiles.insert("Gaming".into(), g);
        cfg.active_profile = "Gaming".into();

        let ov = cfg.profile_overrides();
        assert!(!ov.active_is_default);
        assert_eq!(
            ov.device_capabilities["dev1"],
            vec!["fan_curve".to_string()]
        );
        assert!(ov.canvas);
    }

    #[test]
    fn profile_overrides_empty_on_default_profile() {
        let cfg = Config::default();
        let ov = cfg.profile_overrides();
        assert!(ov.active_is_default);
        assert!(ov.device_capabilities.is_empty());
    }
}
