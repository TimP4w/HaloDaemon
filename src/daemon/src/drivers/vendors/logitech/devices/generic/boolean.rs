// SPDX-License-Identifier: GPL-3.0-or-later
//! Host-mode toggle for `LogitechDevice` — the `BooleanCapability` impl backed by
//! ONBOARD_PROFILES getMode/setMode (0x01 = onboard, 0x02 = host).

use anyhow::{bail, Result};
use async_trait::async_trait;

use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
use crate::drivers::vendors::logitech::devices::generic::state::is_host_mode;
use crate::drivers::vendors::logitech::protocols::hidpp::feature;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::settings::{MODE_HOST, MODE_ONBOARD};
use crate::drivers::BooleanCapability;
use halod_shared::types::Boolean;

#[async_trait]
impl BooleanCapability for LogitechDevice {
    async fn get_booleans(&self) -> Result<Vec<Boolean>> {
        let state = self.state.lock().await;
        let Some(mode) = state.profile.onboard_mode else {
            return Ok(vec![]);
        };
        Ok(vec![Boolean {
            key: "host_mode".to_string(),
            label: "Host Mode".to_string(),
            value: is_host_mode(mode),
            read_only: false,
            category: "Profiles".to_string(),
            visible_when: None,
        }])
    }

    fn state_key(&self) -> &'static str {
        "boolean"
    }

    fn save_state(&self) -> serde_json::Value {
        // Sync read: try_lock returns None when the lock is contended; host_mode
        // is a rare write (user-initiated) so contention here is negligible.
        let host_mode = self
            .state
            .try_lock()
            .ok()
            .and_then(|s| s.profile.onboard_mode.map(is_host_mode));
        match host_mode {
            Some(v) => serde_json::json!({ "host_mode": v }),
            None => serde_json::Value::Null,
        }
    }

    async fn restore_state(&self, v: &serde_json::Value) {
        let Some(want_host) = v.get("host_mode").and_then(|h| h.as_bool()) else {
            return;
        };
        // Only apply if the device's current mode differs from what we want.
        let current = self
            .state
            .lock()
            .await
            .profile
            .onboard_mode
            .map(is_host_mode);
        if current != Some(want_host) {
            if let Err(e) = self.set_boolean("host_mode", want_host).await {
                log::warn!("[{}] restoring host_mode={want_host} failed: {e}", self.id);
            }
        }
    }

    async fn set_boolean(&self, key: &str, value: bool) -> Result<()> {
        if key != "host_mode" {
            bail!("unknown boolean key: {key}");
        }
        if !self
            .state
            .lock()
            .await
            .features
            .contains_key(&feature::ONBOARD_PROFILES)
        {
            bail!("ONBOARD_PROFILES not available");
        }
        self.hidpp2().await.set_onboard_mode(value).await?;
        let mode_byte = if value { MODE_HOST } else { MODE_ONBOARD };
        self.state.lock().await.profile.onboard_mode = Some(mode_byte);

        // Reload DPI + onboard-profile state: switching modes changes which
        // profile the firmware drives, so re-read the device profile to refresh
        // the reported DPI steps and profile directory for the new mode.
        {
            let mut state = self.state.lock().await;
            let features = state.features.clone();
            self.init_onboard(&features, &mut state).await;
            self.init_dpi(&features, &mut state).await;
        }

        // When switching to host mode, re-enable SW LED control and reapply RGB.
        if value {
            self.restore_rgb_control().await;
        }

        Ok(())
    }
}
