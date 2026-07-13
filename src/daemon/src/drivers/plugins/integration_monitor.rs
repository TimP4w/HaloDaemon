// SPDX-License-Identifier: GPL-3.0-or-later
//! Reconnect/liveness watcher for config-instantiated integration plugins:
//! connects one offline at startup, reconnects one that dropped (greying its
//! devices meanwhile), and diffs a live one for controller add/remove — the
//! re-enumerate doubling as the liveness probe. Spawned from `main`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::drivers::Device;
use crate::ipc::broadcast_state;
use crate::registry::usecases::registration::{register_device, unregister_device_and_children};
use crate::state::AppState;

use super::integration_scan;
use super::manifest::PluginManifest;

/// Base cadence between ticks when everything is healthy.
const BASE_TICK_SECS: u64 = 5;
/// Backoff ceiling for a persistently-failing integration.
const MAX_TICK_SECS: u64 = 30;

/// Next-tick delay, backing off with the worst failure streak: 5,5,10,20,30,30s.
fn tick_delay(max_failures: u32) -> Duration {
    let secs = match max_failures {
        0 => BASE_TICK_SECS,
        n => (BASE_TICK_SECS << (n - 1).min(3)).min(MAX_TICK_SECS),
    };
    Duration::from_secs(secs)
}

/// Background service driven from `main`: reconnects offline integrations, greys
/// dropped ones, and diffs live ones for controller add/remove. Backs off
/// exponentially while an integration stays unreachable.
pub async fn integration_monitor(app: Arc<AppState>) {
    let mut failures: HashMap<String, u32> = HashMap::new();
    let mut in_progress: HashSet<String> = HashSet::new();
    loop {
        let delay = tick_delay(failures.values().copied().max().unwrap_or(0));
        tokio::time::sleep(delay).await;
        tick_once(&app, &mut failures, &mut in_progress).await;
    }
}

/// One pass over every enabled integration. Factored out of the loop so tests
/// can drive it without the sleep. `failures` tracks the per-plugin backoff
/// streak; `in_progress` guards a plugin whose (awaited) rebuild is running.
pub(super) async fn tick_once(
    app: &Arc<AppState>,
    failures: &mut HashMap<String, u32>,
    in_progress: &mut HashSet<String>,
) {
    // Don't fight a scoped or global rediscovery while it owns device teardown.
    if app.discovery_filter.read().await.is_some() {
        return;
    }

    let manifests = app.registry.integration_manifests();
    // Drop bookkeeping for integrations no longer enabled.
    let enabled: HashSet<String> = manifests.iter().map(|m| m.plugin_id.clone()).collect();
    failures.retain(|id, _| enabled.contains(id));
    in_progress.retain(|id| enabled.contains(id));

    for manifest in manifests {
        let plugin_id = manifest.plugin_id.clone();
        if in_progress.contains(&plugin_id) {
            continue;
        }
        // Re-check membership right before acting so a just-disabled integration
        // (e.g. via `set_integration_enabled`) is never resurrected mid-tick.
        if app.registry.integration_manifest(&plugin_id).is_none() {
            continue;
        }

        match find_root(app, &plugin_id).await {
            None => {
                // Case A: no root registered — try to (re)connect. Idempotent:
                // `discover_one` registers only on a successful connect.
                in_progress.insert(plugin_id.clone());
                integration_scan::discover_one(app, &plugin_id).await;
                let registered = find_root(app, &plugin_id).await.is_some();
                in_progress.remove(&plugin_id);
                if registered {
                    failures.remove(&plugin_id);
                    broadcast_state(app).await;
                } else {
                    *failures.entry(plugin_id).or_insert(0) += 1;
                }
            }
            Some(root) if root.is_live() => {
                // Case C: alive — diff children (also detects a server drop).
                reconcile_live_root(app, &root, failures).await;
            }
            Some(root) => {
                // Case B: greyed — probe, and rebuild a fresh pool on success.
                in_progress.insert(plugin_id.clone());
                reconnect_offline_root(app, &manifest, &root, failures).await;
                in_progress.remove(&plugin_id);
            }
        }
    }
}

/// Case C: root live — diff children, registering added and removing departed.
/// An `Err` means the server dropped (root greyed inside `resync_children`).
async fn reconcile_live_root(
    app: &Arc<AppState>,
    root: &Arc<dyn Device>,
    failures: &mut HashMap<String, u32>,
) {
    let Some(ctrl) = root.as_controller() else {
        return;
    };
    let existing = child_ids_of(app, root.id()).await;
    match ctrl.resync_children(&existing).await {
        Err(e) => {
            // Server dropped mid-enumerate; `resync_children` already greyed the
            // device. Surface the new offline state to the GUI.
            log::info!("[integration] {} dropped: {e:#}", root.id());
            broadcast_state(app).await;
        }
        Ok((added, gone)) => {
            let changed = !added.is_empty() || !gone.is_empty();
            for child in added {
                register_device(app, child).await;
            }
            remove_children(app, &gone).await;
            if changed {
                broadcast_state(app).await;
            }
            if let Some(id) = root.integration_id() {
                failures.remove(&id);
            }
        }
    }
}

/// Case B: root greyed — probe the server; on success rebuild a fresh pool, on
/// failure leave the greyed device in place and back off.
async fn reconnect_offline_root(
    app: &Arc<AppState>,
    manifest: &PluginManifest,
    root: &Arc<dyn Device>,
    failures: &mut HashMap<String, u32>,
) {
    let plugin_id = manifest.plugin_id.clone();
    let granted = app.registry.granted_for(&plugin_id);
    let config = app
        .registry
        .resolved_config_for(app.secret_store.as_ref(), &plugin_id, &granted);
    let probe = {
        let manifest = manifest.clone();
        tokio::task::spawn_blocking(move || {
            integration_scan::open_probe(&manifest, &config, &granted)
        })
        .await
    };
    match probe {
        Ok(Ok(_io)) => {
            unregister_device_and_children(app, root.id()).await;
            integration_scan::discover_one(app, &plugin_id).await;
            failures.remove(&plugin_id);
            app.registry.clear_connect_error(&plugin_id);
            broadcast_state(app).await;
        }
        Ok(Err(e)) => {
            *failures.entry(plugin_id.clone()).or_insert(0) += 1;
            report_connect_failure(app, manifest, &plugin_id, format!("{e:#}")).await;
        }
        Err(join) => {
            *failures.entry(plugin_id.clone()).or_insert(0) += 1;
            report_connect_failure(app, manifest, &plugin_id, format!("probe task panicked: {join}"))
                .await;
        }
    }
}

/// Emit a deduplicated connect-failure notification + persisted plugin issue for
/// a reconnect attempt, then broadcast state so the plugin page reflects it.
async fn report_connect_failure(
    app: &Arc<AppState>,
    manifest: &PluginManifest,
    plugin_id: &str,
    detail: String,
) {
    app.registry
        .report_connect_error(app, plugin_id, &manifest.display_name(), detail)
        .await;
    broadcast_state(app).await;
}

/// The registered integration root for `plugin_id`, if any.
async fn find_root(app: &Arc<AppState>, plugin_id: &str) -> Option<Arc<dyn Device>> {
    app.devices
        .read()
        .await
        .iter()
        .find(|d| d.integration_id().as_deref() == Some(plugin_id))
        .cloned()
}

/// Ids of the currently-registered children of integration root `root_id`
/// (`{root_id}_ctrl_*`, the scheme `build_child` uses).
async fn child_ids_of(app: &Arc<AppState>, root_id: &str) -> HashSet<String> {
    let prefix = format!("{root_id}_ctrl_");
    app.devices
        .read()
        .await
        .iter()
        .filter(|d| d.id().starts_with(&prefix))
        .map(|d| d.id().to_owned())
        .collect()
}

/// Remove exactly the `gone` ids from `app.devices` and close them, leaving
/// siblings online (a targeted subset, unlike a full teardown).
async fn remove_children(app: &Arc<AppState>, gone: &[String]) {
    if gone.is_empty() {
        return;
    }
    let removed: Vec<Arc<dyn Device>> = {
        let mut devices = app.devices.write().await;
        let mut removed = Vec::new();
        devices.retain(|d| {
            if gone.iter().any(|g| g == d.id()) {
                removed.push(d.clone());
                false
            } else {
                true
            }
        });
        removed
    };
    for d in &removed {
        d.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::{CapabilityRef, Controller};
    use crate::test_support::MockDevice;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// A mock integration root whose `resync_children` returns a scripted result
    /// (like `MockController` in the HID hotplug tests). Its `is_live` flag is
    /// settable; an `Err` script greys it, mirroring the real `LuaDevice`.
    type ScriptedResync = Result<(Vec<Arc<dyn Device>>, Vec<String>)>;

    struct MockRoot {
        id: String,
        integration_id: String,
        live: Arc<AtomicBool>,
        script: Mutex<Option<ScriptedResync>>,
    }

    impl MockRoot {
        fn new(plugin_id: &str) -> Arc<Self> {
            Arc::new(Self {
                id: format!("{plugin_id}-root"),
                integration_id: plugin_id.to_string(),
                live: Arc::new(AtomicBool::new(true)),
                script: Mutex::new(None),
            })
        }

        fn scripted(self: &Arc<Self>, result: ScriptedResync) {
            *self.script.lock().unwrap() = Some(result);
        }
    }

    #[async_trait]
    impl Device for MockRoot {
        fn id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            &self.id
        }
        fn vendor(&self) -> &str {
            "Test"
        }
        fn model(&self) -> &str {
            &self.id
        }
        async fn initialize(&self) -> Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn integration_id(&self) -> Option<String> {
            Some(self.integration_id.clone())
        }
        fn is_live(&self) -> bool {
            self.live.load(Ordering::Relaxed)
        }
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::Controller(self)]
        }
    }

    #[async_trait]
    impl Controller for MockRoot {
        async fn resync_children(
            &self,
            _existing: &HashSet<String>,
        ) -> Result<(Vec<Arc<dyn Device>>, Vec<String>)> {
            match self.script.lock().unwrap().take() {
                Some(Ok(v)) => Ok(v),
                Some(Err(e)) => {
                    // Mirror `LuaDevice::resync_children`: an enumerate error
                    // greys the integration.
                    self.live.store(false, Ordering::Relaxed);
                    Err(e)
                }
                None => Ok((vec![], vec![])),
            }
        }
    }

    fn child(root_id: &str, idx: u32) -> Arc<MockDevice> {
        Arc::new(MockDevice::new(&format!("{root_id}_ctrl_{idx}")))
    }

    #[tokio::test]
    async fn case_c_add_registers_the_new_child() {
        crate::test_support::with_tmp_config(|app| async move {
            let root = MockRoot::new("openrgb");
            let existing = child(&root.id, 0);
            let added = child(&root.id, 1);
            root.scripted(Ok((vec![added.clone() as Arc<dyn Device>], vec![])));
            {
                let mut devices = app.devices.write().await;
                devices.push(root.clone() as Arc<dyn Device>);
                devices.push(existing.clone() as Arc<dyn Device>);
            }

            reconcile_live_root(&app, &(root as Arc<dyn Device>), &mut HashMap::new()).await;

            let ids: Vec<String> = app
                .devices
                .read()
                .await
                .iter()
                .map(|d| d.id().to_owned())
                .collect();
            assert!(ids.iter().any(|id| id == &added.id), "new child registered");
            assert!(ids.iter().any(|id| id == &existing.id), "survivor stays");
        })
        .await;
    }

    #[tokio::test]
    async fn case_c_remove_drops_only_the_departed_child() {
        crate::test_support::with_tmp_config(|app| async move {
            let root = MockRoot::new("openrgb");
            let keep = child(&root.id, 0);
            let drop = child(&root.id, 1);
            root.scripted(Ok((vec![], vec![drop.id.clone()])));
            {
                let mut devices = app.devices.write().await;
                devices.push(root.clone() as Arc<dyn Device>);
                devices.push(keep.clone() as Arc<dyn Device>);
                devices.push(drop.clone() as Arc<dyn Device>);
            }

            reconcile_live_root(&app, &(root as Arc<dyn Device>), &mut HashMap::new()).await;

            let ids: Vec<String> = app
                .devices
                .read()
                .await
                .iter()
                .map(|d| d.id().to_owned())
                .collect();
            assert!(
                !ids.iter().any(|id| id == &drop.id),
                "departed child removed"
            );
            assert!(drop.closed.load(Ordering::SeqCst), "departed child closed");
            assert!(ids.iter().any(|id| id == &keep.id), "sibling survives");
            assert!(!keep.closed.load(Ordering::SeqCst), "sibling not closed");
        })
        .await;
    }

    #[tokio::test]
    async fn case_c_drop_greys_the_root_but_leaves_it_registered() {
        crate::test_support::with_tmp_config(|app| async move {
            let root = MockRoot::new("openrgb");
            root.scripted(Err(anyhow::anyhow!("connection reset")));
            app.devices
                .write()
                .await
                .push(root.clone() as Arc<dyn Device>);

            reconcile_live_root(
                &app,
                &(root.clone() as Arc<dyn Device>),
                &mut HashMap::new(),
            )
            .await;

            assert!(!root.is_live(), "a dropped root is greyed");
            assert_eq!(
                app.devices.read().await.len(),
                1,
                "greyed root stays registered (not removed)"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn discovery_filter_makes_a_tick_a_no_op() {
        crate::test_support::with_tmp_config(|app| async move {
            // A scoped rediscovery is in flight.
            app.set_discovery_filter(Some(Arc::new(
                crate::registry::discovery::DiscoveryFilter { specs: vec![] },
            )))
            .await;
            let root = MockRoot::new("openrgb");
            // Would report a drop if enumerated — but the tick must skip it.
            root.scripted(Err(anyhow::anyhow!("should not be called")));
            app.devices
                .write()
                .await
                .push(root.clone() as Arc<dyn Device>);

            let mut failures = HashMap::new();
            let mut in_progress = HashSet::new();
            tick_once(&app, &mut failures, &mut in_progress).await;

            assert!(root.is_live(), "tick was skipped, so the root stays live");
        })
        .await;
    }

    #[test]
    fn tick_delay_backs_off_and_caps() {
        assert_eq!(tick_delay(0), Duration::from_secs(5));
        assert_eq!(tick_delay(1), Duration::from_secs(5));
        assert_eq!(tick_delay(2), Duration::from_secs(10));
        assert_eq!(tick_delay(3), Duration::from_secs(20));
        assert_eq!(tick_delay(4), Duration::from_secs(30));
        assert_eq!(tick_delay(50), Duration::from_secs(30), "caps at 30s");
    }
}
