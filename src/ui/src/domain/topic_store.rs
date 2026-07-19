// SPDX-License-Identifier: GPL-3.0-or-later
//! Typed GUI mirror of the daemon's authoritative state bus.

use std::collections::HashMap;

use halod_shared::bus::{BusRecord, BusSnapshot, BusTransaction, BusValue};
use halod_shared::types::{
    CoolingState, DiscoveryStatus, GuiConfig, HealthCheckState, LcdState, LightingOverviewState,
    PluginsState, ProfileState, WireDevice,
};

#[derive(Clone, Default)]
pub struct TopicStore {
    pub revision: u64,
    pub discovery: DiscoveryStatus,
    pub devices: Vec<WireDevice>,
    pub profiles: ProfileState,
    pub cooling: CoolingState,
    pub lighting: LightingOverviewState,
    pub lcd: LcdState,
    pub gui: GuiConfig,
    pub config_dir: String,
    pub health: HealthCheckState,
    pub process_icons: HashMap<String, String>,
    pub plugins: PluginsState,
}

impl TopicStore {
    pub fn replace_snapshot(&mut self, snapshot: BusSnapshot) {
        *self = Self::default();
        self.revision = snapshot.revision;
        for record in snapshot.records {
            self.apply_record(record);
        }
        self.sort_devices();
    }

    pub fn apply_transaction(&mut self, transaction: BusTransaction) {
        if transaction.revision <= self.revision {
            return;
        }
        for key in transaction.tombstones {
            if let Some(id) = key.strip_prefix(halod_shared::bus::topic::DEVICE_PREFIX) {
                self.devices.retain(|device| device.id != id);
            }
        }
        for record in transaction.upserts {
            self.apply_record(record);
        }
        self.revision = transaction.revision;
        self.sort_devices();
    }

    fn apply_record(&mut self, record: BusRecord) {
        match record.value {
            BusValue::Discovery(value) => self.discovery = value,
            BusValue::Device(value) => {
                if let Some(existing) = self.devices.iter_mut().find(|device| device.id == value.id)
                {
                    *existing = value;
                } else {
                    self.devices.push(value);
                }
            }
            BusValue::Profiles(value) => self.profiles = value,
            BusValue::Cooling(value) => self.cooling = value,
            BusValue::Lighting(value) => self.lighting = value,
            BusValue::Lcd(value) => self.lcd = value,
            BusValue::Gui(value) => self.gui = value,
            BusValue::Health(value) => self.health = value,
            BusValue::ProcessIcons(value) => self.process_icons = value,
            BusValue::Plugins(value) => self.plugins = value,
            BusValue::ConfigDir(value) => self.config_dir = value,
        }
    }

    fn sort_devices(&mut self) {
        self.devices.sort_by(|left, right| left.id.cmp(&right.id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::bus::{BusRecordStatus, BusValue};

    fn record(key: &str, revision: u64, value: BusValue) -> BusRecord {
        BusRecord {
            key: key.into(),
            value,
            status: BusRecordStatus::Fresh,
            revision,
        }
    }

    #[test]
    fn transaction_upserts_and_removes_devices_atomically() {
        let mut store = TopicStore::default();
        let mut first = WireDevice::default();
        first.id = "first".into();
        store.apply_transaction(BusTransaction {
            revision: 1,
            upserts: vec![record(
                &halod_shared::bus::topic::device("first"),
                1,
                BusValue::Device(first),
            )],
            tombstones: Vec::new(),
        });
        let mut second = WireDevice::default();
        second.id = "second".into();
        store.apply_transaction(BusTransaction {
            revision: 2,
            upserts: vec![record(
                &halod_shared::bus::topic::device("second"),
                2,
                BusValue::Device(second),
            )],
            tombstones: vec![halod_shared::bus::topic::device("first")],
        });
        assert_eq!(store.revision, 2);
        assert_eq!(store.devices.len(), 1);
        assert_eq!(store.devices[0].id, "second");
    }

    #[test]
    fn stale_transaction_is_ignored() {
        let mut store = TopicStore {
            revision: 4,
            ..Default::default()
        };
        store.apply_transaction(BusTransaction {
            revision: 3,
            upserts: vec![record(
                halod_shared::bus::topic::CONFIG_DIR,
                3,
                BusValue::ConfigDir("old".into()),
            )],
            tombstones: Vec::new(),
        });
        assert!(store.config_dir.is_empty());
    }
}
