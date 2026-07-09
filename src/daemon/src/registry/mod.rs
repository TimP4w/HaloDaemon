// SPDX-License-Identifier: GPL-3.0-or-later
pub mod config;
pub mod discovery;
pub mod snapshot;
pub mod state;
pub mod usecases;

use anyhow::Result;
use std::sync::Arc;

use crate::drivers::Device;
use crate::state::AppState;
use halod_shared::types::VisibilityState;

pub use state::{HidTracking, HidTrackingEntry};

/// Look up the device with the given `id` and return an owned clone, holding the
/// `devices` lock only for the lookup. Error if the device is not found or is
/// disabled — every device-action usecase routes through here, so a disabled
/// device rejects all actions. Visibility changes use a raw lookup instead.
pub async fn require_device_owned_id(id: &str, app: &AppState) -> Result<Arc<dyn Device>> {
    let device = app
        .find_device_by_id(id)
        .await
        .ok_or_else(|| anyhow::anyhow!("device not found: {id}"))?;
    if device.active_state() == VisibilityState::Disabled {
        anyhow::bail!("device is disabled: {id}");
    }
    Ok(device)
}

pub async fn seed_known_devices(app: Arc<AppState>) {
    let devices = app.devices.read().await.clone();
    let mut cfg = app.config.write().await;
    for device in &devices {
        config::ensure_record(&mut cfg.known_devices, device.id(), Some(device.as_ref()));
    }
}

pub async fn initialize_app_state(app: Arc<AppState>) {
    discovery::discover_devices(app.clone()).await;
    seed_known_devices(app.clone()).await;
    usecases::chain::restore_saved_chains(app.clone()).await;
    crate::profiles::usecases::profiles::load_active_profile(app.clone()).await;
}

#[cfg(test)]
mod guard_tests {
    use super::require_device_owned_id;
    use crate::config::Config;
    use crate::drivers::Device;
    use crate::state::AppState;
    use crate::test_support::MockDevice;
    use halod_shared::types::VisibilityState;
    use std::sync::Arc;

    #[tokio::test]
    async fn require_device_owned_id_rejects_disabled_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        dev.set_active_state(VisibilityState::Disabled);
        app.devices.write().await.push(dev);

        let result = require_device_owned_id("dev1", &app).await;
        let msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            msg.contains("disabled"),
            "expected disabled error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn require_device_owned_id_allows_visible_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        app.devices.write().await.push(dev);

        assert!(require_device_owned_id("dev1", &app).await.is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use async_trait::async_trait;

    struct StubDevice {
        id: &'static str,
    }

    #[async_trait]
    impl Device for StubDevice {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            "stub"
        }
        fn vendor(&self) -> &str {
            "stub"
        }
        fn model(&self) -> &str {
            "stub"
        }
        async fn initialize(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<crate::drivers::CapabilityRef<'_>> {
            vec![]
        }
    }

    async fn app_with_devices(ids: &[&'static str]) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        let mut devices = app.devices.write().await;
        for id in ids {
            devices.push(Arc::new(StubDevice { id }) as Arc<dyn Device>);
        }
        drop(devices);
        app
    }

    #[tokio::test]
    async fn require_device_owned_id_returns_matching_device() {
        let app = app_with_devices(&["dev_a", "dev_b"]).await;
        let found = require_device_owned_id("dev_b", &app).await.unwrap();
        assert_eq!(found.id(), "dev_b");
    }

    #[tokio::test]
    async fn require_device_owned_id_errors_when_not_found() {
        let app = app_with_devices(&["dev_a"]).await;
        let err = require_device_owned_id("dev_z", &app)
            .await
            .err()
            .expect("expected error");
        assert!(err.to_string().contains("dev_z"));
    }
}
