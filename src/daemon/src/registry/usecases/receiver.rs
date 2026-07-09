// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{Context, Result};
use std::sync::Arc;

use crate::ipc::broadcast_state;
use crate::registry::require_device_owned_id;
use crate::state::AppState;

pub async fn start_pairing(id: String, timeout_secs: u8, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let cap = device
        .as_pairing()
        .context("device does not support pairing")?;
    cap.start_pairing(timeout_secs).await?;
    broadcast_state(&app).await;
    Ok(())
}

pub async fn stop_pairing(id: String, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let cap = device
        .as_pairing()
        .context("device does not support pairing")?;
    cap.stop_pairing().await?;
    broadcast_state(&app).await;
    Ok(())
}

pub async fn unpair(id: String, slot: u8, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let cap = device
        .as_pairing()
        .context("device does not support pairing")?;
    if let Some(removed) = cap.unpair(slot).await? {
        let removed_id = removed.id();
        app.devices.write().await.retain(|d| d.id() != removed_id);
        removed.close().await;
        log::info!("[receiver] Removed {removed_id} after unpair");
    }
    broadcast_state(&app).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::{CapabilityRef, Device, PairingCapability};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

    #[derive(Default)]
    struct MockReceiver {
        started_timeout: AtomicU8,
        unpaired_slot: AtomicU8,
        stopped: AtomicBool,
    }

    #[async_trait]
    impl PairingCapability for MockReceiver {
        async fn start_pairing(&self, timeout_secs: u8) -> Result<()> {
            self.started_timeout.store(timeout_secs, Ordering::SeqCst);
            Ok(())
        }
        async fn stop_pairing(&self) -> Result<()> {
            self.stopped.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn unpair(&self, slot: u8) -> Result<Option<Arc<dyn Device>>> {
            self.unpaired_slot.store(slot, Ordering::SeqCst);
            Ok(None)
        }
    }

    #[async_trait]
    impl Device for MockReceiver {
        fn id(&self) -> &str {
            "mock_receiver"
        }
        fn name(&self) -> &str {
            "Mock Receiver"
        }
        fn vendor(&self) -> &str {
            "Mock"
        }
        fn model(&self) -> &str {
            "Receiver"
        }
        async fn initialize(&self) -> Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::Pairing(self)]
        }
    }

    async fn app_with(dev: Arc<dyn Device>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.write().await.push(dev);
        app
    }

    #[tokio::test]
    async fn start_pairing_forwards_timeout() {
        let mock = Arc::new(MockReceiver::default());
        let app = app_with(Arc::clone(&mock) as Arc<dyn Device>).await;
        start_pairing("mock_receiver".into(), 30, app)
            .await
            .unwrap();
        assert_eq!(mock.started_timeout.load(Ordering::SeqCst), 30);
    }

    #[tokio::test]
    async fn stop_pairing_closes_lock() {
        let mock = Arc::new(MockReceiver::default());
        let app = app_with(Arc::clone(&mock) as Arc<dyn Device>).await;
        stop_pairing("mock_receiver".into(), app).await.unwrap();
        assert!(mock.stopped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn unpair_forwards_slot() {
        let mock = Arc::new(MockReceiver::default());
        let app = app_with(Arc::clone(&mock) as Arc<dyn Device>).await;
        unpair("mock_receiver".into(), 3, app).await.unwrap();
        assert_eq!(mock.unpaired_slot.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn start_pairing_unknown_device_errors() {
        let app = Arc::new(AppState::new(Config::default()));
        assert!(start_pairing("nope".into(), 30, app).await.is_err());
    }
}
