// SPDX-License-Identifier: GPL-3.0-or-later
//! Low-battery notification policy over retained device records.

use std::collections::HashMap;
use std::sync::Arc;

use halod_shared::bus::BusValue;
use halod_shared::types::{BatteryStatus, DeviceCapability, NotificationCode, WireDevice};

use crate::application::state::AppState;

const THRESHOLDS: [u8; 3] = [5, 10, 20];

#[derive(Default)]
struct Tracker {
    reported: HashMap<(String, String), u8>,
}

impl Tracker {
    fn observe(&mut self, device: &WireDevice) -> Vec<NotificationCode> {
        let mut notifications = Vec::new();
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
                notifications.push(NotificationCode::LowBattery {
                    device: device.name.clone(),
                    battery: battery.label.clone(),
                    level: battery.level,
                    threshold,
                });
            }
        }
        notifications
    }

    fn remove_device(&mut self, id: &str) {
        self.reported.retain(|(device, _), _| device != id);
    }
}

pub async fn watcher(app: Arc<AppState>) {
    let mut changes = app.data_bus.subscribe_transactions();
    let mut tracker = Tracker::default();
    loop {
        let transaction = match changes.recv().await {
            Ok(transaction) => transaction,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                let snapshot = app
                    .data_bus
                    .state_snapshot(&[halod_shared::bus::topic::DEVICE_PREFIX.into()]);
                halod_shared::bus::BusTransaction {
                    revision: snapshot.revision,
                    upserts: snapshot.records,
                    tombstones: Vec::new(),
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        for key in transaction.tombstones {
            if let Some(id) = key.strip_prefix(halod_shared::bus::topic::DEVICE_PREFIX) {
                tracker.remove_device(id);
            }
        }
        let enabled = app.config.read().await.gui.low_battery_notifications;
        for record in transaction.upserts {
            let BusValue::Device(device) = record.value else {
                continue;
            };
            let notifications = tracker.observe(&device);
            if enabled {
                for notification in notifications {
                    crate::application::notifications::send(&app, notification).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::{Battery, DeviceType};

    fn device(level: u8, status: BatteryStatus) -> WireDevice {
        WireDevice {
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
        }
    }

    #[test]
    fn reports_crossed_thresholds_and_rearms_after_charging() {
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker
                .observe(&device(19, BatteryStatus::Discharging))
                .len(),
            1
        );
        assert!(tracker
            .observe(&device(15, BatteryStatus::Discharging))
            .is_empty());
        assert_eq!(
            tracker
                .observe(&device(9, BatteryStatus::Discharging))
                .len(),
            1
        );
        assert!(tracker
            .observe(&device(9, BatteryStatus::Charging))
            .is_empty());
        assert_eq!(
            tracker
                .observe(&device(9, BatteryStatus::Discharging))
                .len(),
            1
        );
    }
}
