// SPDX-License-Identifier: GPL-3.0-or-later
//! Device collection, registration bookkeeping, and per-device command ordering.

use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::domain::device::Device;
use halod_shared::ids::{DeviceId, PluginId};

#[derive(Default)]
pub struct DeviceRegistry {
    devices: RwLock<Vec<Arc<dyn Device>>>,
    revision: AtomicU64,
    pub(crate) registrations: Mutex<HashSet<DeviceId>>,
    pub(crate) children: Mutex<HashMap<DeviceId, HashSet<DeviceId>>>,
    command_locks: Mutex<HashMap<CommandTarget, Arc<Mutex<()>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CommandTarget {
    Device(DeviceId),
    Plugin(PluginId),
}

impl DeviceRegistry {
    pub async fn read(&self) -> RwLockReadGuard<'_, Vec<Arc<dyn Device>>> {
        self.devices.read().await
    }

    pub async fn write(&self) -> DeviceRegistryWriteGuard<'_> {
        DeviceRegistryWriteGuard {
            guard: self.devices.write().await,
            revision: &self.revision,
        }
    }

    #[cfg(test)]
    pub fn try_write(
        &self,
    ) -> Result<RwLockWriteGuard<'_, Vec<Arc<dyn Device>>>, tokio::sync::TryLockError> {
        self.devices.try_write()
    }

    pub async fn find(&self, id: &DeviceId) -> Option<Arc<dyn Device>> {
        self.devices
            .read()
            .await
            .iter()
            .find(|d| d.id() == id)
            .cloned()
    }

    pub async fn known_ids(&self) -> HashSet<DeviceId> {
        let mut ids = self.registrations.lock().await.clone();
        ids.extend(self.devices.read().await.iter().map(|d| d.id().to_owned()));
        ids
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub async fn command_lock(&self, target: CommandTarget) -> Arc<Mutex<()>> {
        Arc::clone(self.command_locks.lock().await.entry(target).or_default())
    }
}

pub struct DeviceRegistryWriteGuard<'a> {
    guard: RwLockWriteGuard<'a, Vec<Arc<dyn Device>>>,
    revision: &'a AtomicU64,
}

impl Deref for DeviceRegistryWriteGuard<'_> {
    type Target = Vec<Arc<dyn Device>>;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl DerefMut for DeviceRegistryWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl Drop for DeviceRegistryWriteGuard<'_> {
    fn drop(&mut self) {
        self.revision.fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockDevice;

    #[tokio::test]
    async fn known_ids_cover_registered_and_in_flight_devices() {
        let registry = DeviceRegistry::default();
        registry
            .write()
            .await
            .push(Arc::new(MockDevice::new("registered")));
        registry
            .registrations
            .lock()
            .await
            .insert("in-flight".parse().unwrap());
        assert_eq!(
            registry.known_ids().await,
            HashSet::from(["registered".parse().unwrap(), "in-flight".parse().unwrap()])
        );
    }
}
