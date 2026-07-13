// SPDX-License-Identifier: GPL-3.0-or-later
//! Domain validation for a persisted profile, applied on config load so a
//! hand-edited or corrupt profile can't push oversized state into memory.

use anyhow::{ensure, Result};

use super::config::Profile;

const MAX_DEVICE_STATES: usize = 512;
const MAX_PROFILE_NAME_LEN: usize = 256;
const MAX_DEVICE_ID_LEN: usize = 256;
/// A device state is a flat capability-keyed object.  Keep this independently
/// bounded from the generic JSON node cap so a shallow hand-edited object
/// cannot contain an unbounded number of capability payloads.
const MAX_STATE_CAPABILITIES: usize = 128;
const MAX_STATE_KEY_LEN: usize = 128;
const STATE_MAX_DEPTH: usize = 32;
const STATE_MAX_NODES: usize = 50_000;

pub fn validate_profile(name: &str, profile: &Profile) -> Result<()> {
    ensure!(!name.is_empty(), "profile name must not be empty");
    ensure!(
        name.len() <= MAX_PROFILE_NAME_LEN,
        "profile name exceeds {MAX_PROFILE_NAME_LEN} bytes"
    );
    ensure!(!name.contains('\0'), "profile name contains a NUL byte");
    ensure!(
        profile.device_states.len() <= MAX_DEVICE_STATES,
        "profile '{name}' has too many device states"
    );
    for (device_id, state) in &profile.device_states {
        ensure!(
            !device_id.is_empty(),
            "profile '{name}' has an empty device id"
        );
        ensure!(
            device_id.len() <= MAX_DEVICE_ID_LEN,
            "profile '{name}' has an over-long device id"
        );
        ensure!(
            !device_id.contains('\0'),
            "profile '{name}' device id contains a NUL byte"
        );
        let state_object = state.as_object().ok_or_else(|| {
            anyhow::anyhow!("profile '{name}' device '{device_id}' state must be a JSON object")
        })?;
        ensure!(
            state_object.len() <= MAX_STATE_CAPABILITIES,
            "profile '{name}' device '{device_id}' has too many capability states"
        );
        for capability in state_object.keys() {
            ensure!(
                !capability.is_empty(),
                "profile '{name}' device '{device_id}' has an empty capability key"
            );
            ensure!(
                capability.len() <= MAX_STATE_KEY_LEN,
                "profile '{name}' device '{device_id}' has an over-long capability key"
            );
            ensure!(
                !capability.contains('\0'),
                "profile '{name}' device '{device_id}' capability key contains a NUL byte"
            );
        }
        crate::util::json::check_bounds(state, STATE_MAX_DEPTH, STATE_MAX_NODES)
            .map_err(|e| anyhow::anyhow!("profile '{name}' device '{device_id}': {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_too_many_device_states() {
        let mut p = Profile::default();
        for i in 0..MAX_DEVICE_STATES + 1 {
            p.device_states.insert(format!("d{i}"), json!({}));
        }
        assert!(validate_profile("p", &p).is_err());
    }

    #[test]
    fn rejects_deeply_nested_state() {
        let mut deep = json!(0);
        for _ in 0..STATE_MAX_DEPTH + 2 {
            deep = json!([deep]);
        }
        let mut p = Profile::default();
        p.device_states.insert("d".into(), deep);
        assert!(validate_profile("p", &p).is_err());
    }

    #[test]
    fn rejects_non_object_device_state() {
        let mut p = Profile::default();
        p.device_states.insert("d".into(), json!(true));
        assert!(validate_profile("p", &p).is_err());
    }

    #[test]
    fn rejects_too_many_or_invalid_capability_keys() {
        let mut p = Profile::default();
        let mut state = serde_json::Map::new();
        for i in 0..=MAX_STATE_CAPABILITIES {
            state.insert(format!("c{i}"), json!(null));
        }
        p.device_states
            .insert("d".into(), serde_json::Value::Object(state));
        assert!(validate_profile("p", &p).is_err());

        p.device_states.insert("d".into(), json!({"": null}));
        assert!(validate_profile("p", &p).is_err());
    }

    #[test]
    fn rejects_invalid_profile_and_device_ids() {
        let p = Profile::default();
        assert!(validate_profile("", &p).is_err());
        assert!(validate_profile("\0", &p).is_err());

        let mut p = Profile::default();
        p.device_states.insert("\0".into(), json!({}));
        assert!(validate_profile("p", &p).is_err());
    }

    #[test]
    fn accepts_a_normal_profile() {
        let mut p = Profile::default();
        p.device_states
            .insert("d".into(), json!({"rgb": {"mode": "static"}}));
        assert!(validate_profile("p", &p).is_ok());
    }
}
