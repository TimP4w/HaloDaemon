// SPDX-License-Identifier: GPL-3.0-or-later
//! Domain validation for a persisted profile, applied on config load so a
//! hand-edited or corrupt profile can't push oversized state into memory.

use anyhow::{ensure, Result};

use super::config::Profile;

const MAX_DEVICE_STATES: usize = 512;
const MAX_DEVICE_ID_LEN: usize = 256;
const STATE_MAX_DEPTH: usize = 32;
const STATE_MAX_NODES: usize = 50_000;

pub fn validate_profile(name: &str, profile: &Profile) -> Result<()> {
    ensure!(
        profile.device_states.len() <= MAX_DEVICE_STATES,
        "profile '{name}' has too many device states"
    );
    for (device_id, state) in &profile.device_states {
        ensure!(
            device_id.len() <= MAX_DEVICE_ID_LEN,
            "profile '{name}' has an over-long device id"
        );
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
    fn accepts_a_normal_profile() {
        let mut p = Profile::default();
        p.device_states
            .insert("d".into(), json!({"rgb": {"mode": "static"}}));
        assert!(validate_profile("p", &p).is_ok());
    }
}
