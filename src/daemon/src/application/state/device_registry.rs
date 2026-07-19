// SPDX-License-Identifier: GPL-3.0-or-later
//! Device collection, registration bookkeeping, and per-device command ordering.

use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::domain::device::Device;

#[derive(Default)]
pub struct DeviceRegistry {
    devices: RwLock<Vec<Arc<dyn Device>>>,
    revision: AtomicU64,
    pub(crate) registrations: Mutex<HashSet<String>>,
    pub(crate) children: Mutex<HashMap<String, HashSet<String>>>,
    command_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
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

    pub async fn find(&self, id: &str) -> Option<Arc<dyn Device>> {
        self.devices
            .read()
            .await
            .iter()
            .find(|d| d.id() == id)
            .cloned()
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub async fn command_lock(&self, id: &str) -> Arc<Mutex<()>> {
        Arc::clone(
            self.command_locks
                .lock()
                .await
                .entry(id.to_owned())
                .or_default(),
        )
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
