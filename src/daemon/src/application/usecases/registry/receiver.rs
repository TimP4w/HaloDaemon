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

/// Re-establish the registered children after a connection event: a paired slot
/// survives its device sleeping, so the diff above never revisits one. Each
/// unreachable child costs a full HID++ timeout — run this per connection event,
/// never on a poll.
///
/// A child that still looks live is restored but not re-initialized. Liveness
/// only tracks calls the daemon has made, so a device that slept and woke
/// without one failing in between never stopped looking live even though its
/// firmware dropped back to its own defaults. Returns whether anything was
/// re-initialized.
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
        if child.is_unrecoverable()
            || child.active_state() == halod_shared::types::VisibilityState::Disabled
        {
            continue;
        }
        if child.is_live() {
            super::registration::restore_saved_state(app, &child).await;
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
    use crate::domain::device::Device;
    use crate::test_support::{app_with, MockDevice};
    use std::sync::atomic::Ordering;

    fn receiver() -> (Arc<MockDevice>, Arc<AppState>) {
        let mock = Arc::new(MockDevice::new("mock_receiver").with_pairing());
        (mock.clone(), app_with(vec![mock]))
    }

    #[tokio::test]
    async fn start_pairing_forwards_timeout() {
        let (mock, app) = receiver();
        start_pairing("mock_receiver".into(), 30, app)
            .await
            .unwrap();
        assert_eq!(mock.calls(), ["start_pairing 30"]);
    }

    #[tokio::test]
    async fn stop_pairing_closes_lock() {
        let (mock, app) = receiver();
        stop_pairing("mock_receiver".into(), app).await.unwrap();
        assert_eq!(mock.calls(), ["stop_pairing"]);
    }

    #[tokio::test]
    async fn unpair_forwards_slot() {
        let (mock, app) = receiver();
        unpair("mock_receiver".into(), 3, app).await.unwrap();
        assert_eq!(mock.calls(), ["unpair 3"]);
    }

    #[tokio::test]
    async fn revive_reinitializes_only_the_children_that_came_back() {
        let root: Arc<dyn Device> = Arc::new(MockDevice::new("mock_controller").with_controller());
        let asleep = Arc::new(MockDevice::new("child-asleep").offline());
        let awake = Arc::new(MockDevice::new("child-awake"));
        let app = app_with(vec![root.clone(), asleep.clone(), awake.clone()]);
        {
            let mut cfg = app.config.write().await;
            for id in ["child-asleep", "child-awake"] {
                cfg.active_profile_data_mut()
                    .device_states
                    .insert(id.into(), serde_json::json!({ "x": 1 }));
            }
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
            awake.load_called.load(Ordering::SeqCst),
            "a connection event means stale firmware even for a child that never \
             stopped looking live, so its state is restored too"
        );
        assert!(
            !awake.init_called.load(Ordering::SeqCst),
            "but a working child is not re-initialized"
        );
    }

    #[tokio::test]
    async fn start_pairing_unknown_device_errors() {
        let app = app_with(vec![]);
        assert!(start_pairing("nope".into(), 30, app).await.is_err());
    }
}
