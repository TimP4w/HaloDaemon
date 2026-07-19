// SPDX-License-Identifier: GPL-3.0-or-later
//! Device collection, registration bookkeeping, and per-device command ordering.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::drivers::Device;

#[derive(Default)]
pub struct DeviceRegistry {
    devices: RwLock<Vec<Arc<dyn Device>>>,
    pub(crate) registrations: Mutex<HashSet<String>>,
    pub(crate) children: Mutex<HashMap<String, HashSet<String>>>,
    command_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl DeviceRegistry {
    pub async fn read(&self) -> RwLockReadGuard<'_, Vec<Arc<dyn Device>>> {
        self.devices.read().await
    }

    pub async fn write(&self) -> RwLockWriteGuard<'_, Vec<Arc<dyn Device>>> {
        self.devices.write().await
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
