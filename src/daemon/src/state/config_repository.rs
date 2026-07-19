// SPDX-License-Identifier: GPL-3.0-or-later
//! Persistent configuration authority and debounced save signalling.

use super::Persistence;
use crate::config::Config;
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

pub struct ConfigRepository {
    value: RwLock<Config>,
    persistence: Persistence,
}

impl ConfigRepository {
    pub fn new(value: Config) -> Self {
        Self {
            value: RwLock::new(value),
            persistence: Persistence::new(),
        }
    }

    pub async fn read(&self) -> RwLockReadGuard<'_, Config> {
        self.value.read().await
    }
    pub async fn write(&self) -> RwLockWriteGuard<'_, Config> {
        self.value.write().await
    }

    pub fn request_save(&self) {
        self.persistence
            .save_tx
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    pub(crate) fn persistence(&self) -> &Persistence {
        &self.persistence
    }
}
