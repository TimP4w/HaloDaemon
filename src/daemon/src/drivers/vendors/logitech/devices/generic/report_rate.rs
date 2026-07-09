// SPDX-License-Identifier: GPL-3.0-or-later
//! Report-rate (`REPORT_RATE` 0x8060 / `EXT_REPORT_RATE` 0x8061) for
//! `LogitechDevice` — the `ChoiceCapability` impl and its `init_report_rate`.
//! Rate tables and wire encoding live in the protocol; this file only mirrors
//! the read into state and coordinates the host-mode switch a rate write needs.

use std::collections::HashMap;

use anyhow::{bail, Result};
use async_trait::async_trait;

use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
use crate::drivers::vendors::logitech::devices::generic::state::{
    is_host_mode, LogitechDeviceState,
};
use crate::drivers::vendors::logitech::protocols::hidpp::feature;
use crate::drivers::{ChoiceCapability, ChoiceStateCache};
use halod_shared::types::DeviceCapability;

impl LogitechDevice {
    pub(super) async fn init_report_rate(
        &self,
        features: &HashMap<u16, u8>,
        state: &mut LogitechDeviceState,
    ) {
        if let Some(info) = self.hidpp2_with(features).await.read_report_rates().await {
            state.report_rate.options = info.options;
            state.report_rate.current = info.current;
            state.report_rate.ext = info.ext;
        }
    }
}

fn report_rate_was_host(original_mode: Option<u8>) -> bool {
    original_mode.map(is_host_mode).unwrap_or(false)
}

#[async_trait]
impl ChoiceCapability for LogitechDevice {
    fn choice_cache(&self) -> &ChoiceStateCache {
        &self.choice_cache
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        self.state
            .lock()
            .await
            .report_rate
            .to_choice()
            .map(|c| DeviceCapability::Choice(vec![c]))
    }

    async fn set_choice(&self, key: &str, selected: usize) -> Result<()> {
        if key != "report_rate" {
            bail!("Unknown choice key: {key}");
        }
        let (wire_index, ext, original_mode, has_onboard) = {
            let state = self.state.lock().await;
            let Some(opt) = state.report_rate.options.get(selected) else {
                bail!(
                    "Index {selected} out of range for report_rate options (len={})",
                    state.report_rate.options.len()
                );
            };
            (
                opt.wire_index,
                state.report_rate.ext,
                state.profile.onboard_mode,
                state.features.contains_key(&feature::ONBOARD_PROFILES),
            )
        };
        // Record only after the key and index have been validated, so invalid
        // input never pollutes the choice cache.
        self.choice_cache.record(key, selected);

        log::debug!(
            "[{}] set_choice report_rate: wire_index={wire_index} ext={ext}",
            self.id
        );

        let hidpp = self.hidpp2().await;
        let was_host = report_rate_was_host(original_mode);

        // Setting the rate requires Host mode. Switch only if we weren't already
        // in Host mode, and restore the user's original mode afterwards so this
        // doesn't silently deactivate Host mode.
        if has_onboard && !was_host {
            if let Err(e) = hidpp.set_onboard_mode(true).await {
                log::warn!("[{}] report_rate: failed to enter host mode: {e}", self.id);
            }
        }

        let result = hidpp.set_report_rate(wire_index, ext).await.map_err(|e| {
            log::warn!("[{}] report rate set failed: {e}", self.id);
            e
        });

        if has_onboard && !was_host {
            if let Err(e) = hidpp.set_onboard_mode(false).await {
                log::warn!(
                    "[{}] report_rate: failed to restore onboard mode after rate change: {e}",
                    self.id
                );
            }
        }

        result?;

        self.state.lock().await.report_rate.current = Some(wire_index);

        // If we transitioned back to Onboard mode the firmware reclaims LED
        // control, so re-enable SW control and re-apply the last known RGB.
        if !was_host {
            self.restore_rgb_control().await;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_rate_skips_dance_when_already_host() {
        assert!(report_rate_was_host(Some(0x02)));
    }

    #[test]
    fn report_rate_does_dance_when_onboard() {
        assert!(!report_rate_was_host(Some(0x01)));
    }

    #[test]
    fn report_rate_does_dance_when_mode_unknown() {
        assert!(!report_rate_was_host(None));
    }
}
