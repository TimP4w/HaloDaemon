// SPDX-License-Identifier: GPL-3.0-or-later
//! One-shot low-battery threshold tracking for native notifications.

use std::collections::HashMap;

use halod_shared::types::{AppState, BatteryStatus, DeviceCapability};

const THRESHOLDS: [u8; 3] = [5, 10, 20];

#[derive(Debug, PartialEq, Eq)]
pub struct LowBatteryAlert {
    pub device: String,
    pub battery: String,
    pub level: u8,
    pub threshold: u8,
}

#[derive(Default)]
pub struct LowBatteryTracker {
    /// Lowest threshold already reported in the current low-battery episode.
    reported: HashMap<(String, String), u8>,
}

impl LowBatteryTracker {
    pub fn observe(&mut self, state: &AppState) -> Vec<LowBatteryAlert> {
        let mut alerts = Vec::new();
        for device in &state.devices {
            for capability in &device.capabilities {
                let DeviceCapability::Battery(batteries) = capability else {
                    continue;
                };
                for battery in batteries {
                    let key = (device.id.clone(), battery.key.clone());
                    if battery.status == BatteryStatus::Charging || battery.level >= 20 {
                        self.reported.remove(&key);
                        continue;
                    }
                    if battery.status == BatteryStatus::Unknown {
                        continue;
                    }
                    let threshold = THRESHOLDS
                        .into_iter()
                        .find(|threshold| battery.level < *threshold)
                        .expect("a discharging battery below 20 has a threshold");
                    if self
                        .reported
                        .get(&key)
                        .is_some_and(|reported| *reported <= threshold)
                    {
                        continue;
                    }
                    self.reported.insert(key, threshold);
                    alerts.push(LowBatteryAlert {
                        device: device.name.clone(),
                        battery: battery.label.clone(),
                        level: battery.level,
                        threshold,
                    });
                }
            }
        }
        alerts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::{Battery, DeviceType, WireDevice};

    fn state(level: u8, status: BatteryStatus) -> AppState {
        AppState {
            devices: vec![WireDevice {
                id: "mouse".into(),
                name: "Mouse".into(),
                device_type: DeviceType::Mouse,
                capabilities: vec![DeviceCapability::Battery(vec![Battery {
                    key: "main".into(),
                    label: "Battery".into(),
                    level,
                    status,
                }])],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn reports_each_crossed_threshold_once() {
        let mut tracker = LowBatteryTracker::default();
        assert!(tracker
            .observe(&state(20, BatteryStatus::Discharging))
            .is_empty());
        assert_eq!(
            tracker.observe(&state(19, BatteryStatus::Discharging))[0].threshold,
            20
        );
        assert!(tracker
            .observe(&state(15, BatteryStatus::Discharging))
            .is_empty());
        assert_eq!(
            tracker.observe(&state(9, BatteryStatus::Discharging))[0].threshold,
            10
        );
        assert_eq!(
            tracker.observe(&state(4, BatteryStatus::Discharging))[0].threshold,
            5
        );
        assert!(tracker
            .observe(&state(3, BatteryStatus::Discharging))
            .is_empty());
    }

    #[test]
    fn charging_rearms_a_new_episode() {
        let mut tracker = LowBatteryTracker::default();
        assert_eq!(
            tracker
                .observe(&state(19, BatteryStatus::Discharging))
                .len(),
            1
        );
        assert!(tracker
            .observe(&state(19, BatteryStatus::Charging))
            .is_empty());
        assert_eq!(
            tracker
                .observe(&state(19, BatteryStatus::Discharging))
                .len(),
            1
        );
    }

    #[test]
    fn unknown_readings_do_not_notify_or_rearm() {
        let mut tracker = LowBatteryTracker::default();
        assert_eq!(
            tracker.observe(&state(9, BatteryStatus::Discharging)).len(),
            1
        );
        assert!(tracker
            .observe(&state(0, BatteryStatus::Unknown))
            .is_empty());
        assert!(tracker
            .observe(&state(9, BatteryStatus::Discharging))
            .is_empty());
    }

    #[test]
    fn first_low_snapshot_only_reports_the_most_urgent_threshold() {
        let alerts = LowBatteryTracker::default().observe(&state(4, BatteryStatus::Discharging));
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].threshold, 5);
        assert_eq!(alerts[0].level, 4);
    }
}
