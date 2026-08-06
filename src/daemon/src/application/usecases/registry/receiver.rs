// SPDX-License-Identifier: GPL-3.0-or-later
use crate::domain::events::ChangeSink as _;

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::registry::require_device_owned_id;

/// Reconcile the explicitly-owned children of a dynamic controller. Plugin
/// children use stable hardware ids (for example a receiver serial), so a
/// string-prefix scan cannot safely identify them.
pub(crate) async fn reconcile_owned_children(
    device: &Arc<dyn crate::domain::device::Device>,
    app: &Arc<AppState>,
) -> bool {
    let Some(controller) = device.as_controller() else {
        return false;
    };
    let existing = app
        .device_registry
        .children
        .lock()
        .await
        .get(device.id())
        .cloned()
        .unwrap_or_default();
    let Ok((added, gone)) = controller.resync_children(&existing).await else {
        return false;
    };

    let mut registered = std::collections::HashSet::new();
    for child in added {
        let child_id = child.id().to_owned();
        if crate::application::usecases::registry::registration::register_device(app, child).await {
            registered.insert(child_id);
        }
    }
    if !gone.is_empty() {
        let removed: Vec<Arc<dyn crate::domain::device::Device>> = {
            let mut devices = app.device_registry.write().await;
            let mut removed = Vec::new();
            devices.retain(|candidate| {
                if gone.iter().any(|child_id| child_id == candidate.id()) {
                    removed.push(candidate.clone());
                    false
                } else {
                    true
                }
            });
            removed
        };
        for child in removed {
            log::info!(
                "[receiver] Removed {} after receiver slot changed",
                child.id()
            );
            super::registration::close_device(app, &child).await;
        }
    }
    let changed = !gone.is_empty() || !registered.is_empty();
    if changed {
        let mut owners = app.device_registry.children.lock().await;
        let children = owners.entry(device.id().to_owned()).or_default();
        for child_id in gone {
            children.remove(&child_id);
        }
        children.extend(registered);
    }
    changed
}

/// Re-initialize the registered children that are no longer live: a paired slot
/// survives its device sleeping, so the diff above never revisits one. Each
/// unreachable child costs a full HID++ timeout — run this per connection event,
/// never on a poll.
pub(crate) async fn revive_owned_children(
    device: &Arc<dyn crate::domain::device::Device>,
    app: &Arc<AppState>,
) -> bool {
    let children = app
        .device_registry
        .children
        .lock()
        .await
        .get(device.id())
        .cloned()
        .unwrap_or_default();
    let mut revived = false;
    for child_id in children {
        let Some(child) = app.find_device_by_id(&child_id).await else {
            continue;
        };
        if child.is_live()
            || child.is_unrecoverable()
            || child.active_state() == halod_shared::types::VisibilityState::Disabled
        {
            continue;
        }
        match super::registration::init_device(app, &child).await {
            Ok(true) => {
                super::registration::restore_saved_state(app, &child).await;
                log::info!("[receiver] Reinitialized {} after it came back", child.id());
                revived = true;
            }
            _ => log::debug!("[receiver] {} is still unreachable", child.id()),
        }
    }
    revived
}

pub async fn start_pairing(id: String, timeout_secs: u8, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let cap = device
        .as_pairing()
        .context("device does not support pairing")?;
    cap.start_pairing(timeout_secs).await?;
    app.record_change(crate::domain::events::Change::Device(id))
        .await;
    Ok(())
}

pub async fn stop_pairing(id: String, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let cap = device
        .as_pairing()
        .context("device does not support pairing")?;
    cap.stop_pairing().await?;
    app.record_change(crate::domain::events::Change::Device(id))
        .await;
    Ok(())
}

pub async fn unpair(id: String, slot: u8, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let cap = device
        .as_pairing()
        .context("device does not support pairing")?;
    if let Some(removed) = cap.unpair(slot).await? {
        let removed_id = removed.id();
        app.device_registry
            .write()
            .await
            .retain(|d| d.id() != removed_id);
        super::registration::close_device(&app, &removed).await;
        log::info!("[receiver] Removed {removed_id} after unpair");
    } else {
        // Lua/plugin controllers cannot return a concrete child `Arc` through
        // the pairing ABI. Diff their owned children after the hardware write
        // instead, which removes the slot that the plugin just cleared.
        reconcile_owned_children(&device, &app).await;
    }
    app.record_change(crate::domain::events::Change::PluginTopology)
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::domain::device::{CapabilityRef, Device, PairingCapability};
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
        app.device_registry.write().await.push(dev);
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

    #[derive(Default)]
    struct MockController;

    impl crate::domain::device::Controller for MockController {}

    #[async_trait]
    impl Device for MockController {
        fn id(&self) -> &str {
            "mock_controller"
        }
        fn name(&self) -> &str {
            "Mock Controller"
        }
        fn vendor(&self) -> &str {
            "Mock"
        }
        fn model(&self) -> &str {
            "Controller"
        }
        async fn initialize(&self) -> Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::Controller(self)]
        }
    }

    #[tokio::test]
    async fn revive_reinitializes_only_the_children_that_came_back() {
        use crate::test_support::MockDevice;
        let app = Arc::new(AppState::new(Config::default()));
        let root = Arc::new(MockController) as Arc<dyn Device>;
        let asleep = Arc::new(MockDevice::new("child-asleep").offline());
        let awake = Arc::new(MockDevice::new("child-awake"));
        {
            let mut cfg = app.config.write().await;
            for id in ["child-asleep", "child-awake"] {
                cfg.active_profile_data_mut()
                    .device_states
                    .insert(id.into(), serde_json::json!({ "x": 1 }));
            }
        }
        {
            let mut devices = app.device_registry.write().await;
            devices.push(root.clone());
            devices.push(asleep.clone() as Arc<dyn Device>);
            devices.push(awake.clone() as Arc<dyn Device>);
        }
        app.device_registry
            .children
            .lock()
            .await
            .entry("mock_controller".into())
            .or_default()
            .extend(["child-asleep".to_string(), "child-awake".to_string()]);

        assert!(revive_owned_children(&root, &app).await);
        assert!(
            !reconcile_owned_children(&root, &app).await,
            "reviving a child is not a pairing-table change, so the caller that \
             waits for the table to catch up must keep waiting"
        );

        assert!(
            asleep.load_called.load(Ordering::SeqCst),
            "an unreachable child is re-initialized and its saved state restored"
        );
        assert!(
            !awake.load_called.load(Ordering::SeqCst),
            "a live child is left alone"
        );
    }

    #[tokio::test]
    async fn start_pairing_unknown_device_errors() {
        let app = Arc::new(AppState::new(Config::default()));
        assert!(start_pairing("nope".into(), 30, app).await.is_err());
    }
}
