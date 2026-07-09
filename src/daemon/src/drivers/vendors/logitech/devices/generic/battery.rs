// SPDX-License-Identifier: GPL-3.0-or-later
//! Battery for `LogitechDevice` — the `BatteryCapability` impl and the voltage
//! re-poll loop. Source detection and decoding live in the protocol
//! ([`Hidpp20::battery_source`] / [`Hidpp20::read_battery`]); this file only
//! caches the reading and decides how often to re-read.
//!
//! UNIFIED_BATTERY (0x1004) reports a percentage and is read at init (and on
//! reconnect). The voltage features — ADC_MEASUREMENT (0x1F20) / BATTERY_VOLTAGE
//! (0x1001) — report a cell voltage with no change notifications, so they are
//! re-polled on a timer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::drivers::vendors::generic::devices::common::TaskHandle;
use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
use crate::drivers::vendors::logitech::devices::generic::state::LogitechDeviceState;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::battery::BatterySource;
use crate::drivers::BatteryCapability;
use halod_shared::types::{Battery, BatteryStatus};

const POLL_INTERVAL_SECS: u64 = 30;

impl LogitechDevice {
    pub(super) async fn init_battery(
        &self,
        features: &HashMap<u16, u8>,
        state: &mut LogitechDeviceState,
    ) {
        let hidpp = self.hidpp2_with(features).await;
        let source = hidpp.battery_source();
        state.battery.source = source;
        if let Some(reading) = hidpp.read_battery(source).await {
            state.battery.battery_level = Some(reading.percent);
            state.battery.battery_charging = reading.charging;
        }
    }

    /// Start the 30s battery re-poll, but only for voltage-battery devices —
    /// they emit no change notifications. UNIFIED/None devices are no-ops.
    pub(super) async fn start_battery_poll(&self) {
        let source = self.state.lock().await.battery.source;
        if !matches!(source, BatterySource::Voltage(_)) {
            return;
        }
        let state = Arc::clone(&self.state);
        let hidpp = self.hidpp2().await;
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
            loop {
                interval.tick().await;
                let (online, src) = {
                    let s = state.lock().await;
                    (s.online, s.battery.source)
                };
                if !online {
                    continue;
                }
                if let Some(reading) = hidpp.read_battery(src).await {
                    let mut s = state.lock().await;
                    s.battery.battery_level = Some(reading.percent);
                    s.battery.battery_charging = reading.charging;
                }
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(handle));
    }
}

#[async_trait]
impl BatteryCapability for LogitechDevice {
    async fn get_batteries(&self) -> Result<Vec<Battery>> {
        let state = self.state.lock().await;
        if let Some(level) = state.battery.battery_level {
            Ok(vec![Battery {
                key: "battery".to_string(),
                label: "Battery".to_string(),
                level,
                status: if state.battery.battery_charging {
                    BatteryStatus::Charging
                } else {
                    BatteryStatus::Discharging
                },
            }])
        } else {
            Ok(vec![])
        }
    }
}
