// SPDX-License-Identifier: GPL-3.0-or-later
//! Reconnect/liveness watcher for config-instantiated integration plugins:
//! connects one offline at startup, reconnects one that dropped (greying its
//! devices meanwhile), and diffs a live one for controller add/remove — the
//! re-enumerate doubling as the liveness probe. Spawned from `main`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// Next-tick delay, backing off with the attempt streak: 5,5,10,20,30,30s.
fn tick_delay(attempt: u32) -> Duration {
    let secs = match attempt {
        0 => BASE_TICK_SECS,
        n => (BASE_TICK_SECS << (n - 1).min(3)).min(MAX_TICK_SECS),
    };
    Duration::from_secs(secs)
}

/// Per-plugin reconnect state, replacing the old `failures: HashMap<_, u32>` +
/// `in_progress: HashSet<_>` pair. Transitions: `Offline -> Connecting ->
/// Online | Backoff`; `Backoff -> Connecting -> Online | Backoff` (attempt+1);
/// any -> `Disabled` once no longer enabled/granted, back to `Offline` if
/// re-enabled. A live root dropping mid-diff (Case C) doesn't transition this
/// state — it stays `Online` until next tick's liveness check routes it
/// through Case B, which does.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum MonitorState {
    Offline,
    Connecting,
    Online,
    Backoff { attempt: u32, deadline: Instant },
    Disabled,
    Stopping,
}

/// Background service driven from `main`: reconnects offline integrations, greys
/// dropped ones, and diffs live ones for controller add/remove. Backs off
/// exponentially while an integration stays unreachable.
pub async fn integration_monitor(app: Arc<AppState>) {
    let mut states: HashMap<String, MonitorState> = HashMap::new();
    let mut shutdown_rx = app.persistence.shutdown_tx.subscribe();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(BASE_TICK_SECS)) => {}
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    for state in states.values_mut() { *state = MonitorState::Stopping; }
                    return;
                }
            }
        }
        tick_once(&app, &mut states).await;
    }
}

/// Advance `plugin_id` to `Backoff`, incrementing its attempt streak.
fn backoff(states: &mut HashMap<String, MonitorState>, plugin_id: &str, prev_attempt: u32) {
    let attempt = prev_attempt + 1;
    states.insert(
        plugin_id.to_owned(),
        MonitorState::Backoff {
            attempt,
            deadline: Instant::now() + tick_delay(attempt),
        },
    );
}

/// This plugin's current backoff attempt streak, or 0 if it isn't backing off.
fn attempt_of(states: &HashMap<String, MonitorState>, plugin_id: &str) -> u32 {
    match states.get(plugin_id) {
        Some(MonitorState::Backoff { attempt, .. }) => *attempt,
        _ => 0,
    }
}

/// One pass over every enabled integration. Factored out of the loop so tests
/// can drive it without the sleep.
pub(super) async fn tick_once(app: &Arc<AppState>, states: &mut HashMap<String, MonitorState>) {
    // A full rescan touches every device — skip the whole tick. A scoped
    // reconcile only owns its own plugins' devices — skip just those.
    let paused: Option<HashSet<String>> = {
        use crate::registry::discovery::DiscoveryScope;
        match &*app.discovery_scope.read().await {
            DiscoveryScope::Clean => None,
            DiscoveryScope::Full => return,
            DiscoveryScope::PluginSet { plugin_ids, .. } => Some(plugin_ids.clone()),
        }
    };

    let manifests = app.registry.integration_manifests();
    let enabled: HashSet<String> = manifests.iter().map(|m| m.plugin_id.clone()).collect();
    // No longer enabled/granted — mark Disabled rather than silently dropping
    // bookkeeping, so a toggled-off integration stays visible in state.
    for (id, state) in states.iter_mut() {
        if !enabled.contains(id) {
            *state = MonitorState::Disabled;
        }
    }
    // New or just re-enabled — starts Offline, advanced below.
    for id in &enabled {
        match states.get(id) {
            None | Some(MonitorState::Disabled) => {
                states.insert(id.clone(), MonitorState::Offline);
            }
            _ => {}
        }
    }

    for manifest in manifests {
        let plugin_id = manifest.plugin_id.clone();
        if matches!(states.get(&plugin_id), Some(MonitorState::Connecting))
            || paused.as_ref().is_some_and(|p| p.contains(&plugin_id))
        {
            continue;
        }
        if states.get(&plugin_id).is_some_and(|state| {
            matches!(state, MonitorState::Backoff { deadline, .. } if *deadline > Instant::now())
        }) {
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
                let prev_attempt = attempt_of(states, &plugin_id);
                states.insert(plugin_id.clone(), MonitorState::Connecting);
                integration_scan::discover_one(app, &plugin_id).await;
                let registered = find_root(app, &plugin_id).await.is_some();
                if registered {
                    states.insert(plugin_id, MonitorState::Online);
                    broadcast_state(app).await;
                } else {
                    backoff(states, &plugin_id, prev_attempt);
                }
            }
            Some(root) if root.is_live() => {
                // Case C: alive — diff children (also detects a server drop).
                reconcile_live_root(app, &root, states).await;
            }
            Some(root) => {
                // Case B: greyed — probe, and rebuild a fresh pool on success.
                reconnect_offline_root(app, &manifest, &root, states).await;
            }
        }
    }
}

/// Case C: root live — diff children, registering added and removing departed.
/// An `Err` means the server dropped (root greyed inside `resync_children`).
async fn reconcile_live_root(
    app: &Arc<AppState>,
    root: &Arc<dyn Device>,
    states: &mut HashMap<String, MonitorState>,
) {
    let Some(ctrl) = root.as_controller() else {
        return;
    };
    let existing = child_ids_of(app, root.id()).await;
    match ctrl.resync_children(&existing).await {
        Err(e) => {
            // Server dropped mid-enumerate; `resync_children` already greyed the
            // device. Surface the new offline state to the GUI. Next tick's
            // liveness check routes this plugin through Case B.
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
                states.insert(id, MonitorState::Online);
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
    states: &mut HashMap<String, MonitorState>,
) {
    let plugin_id = manifest.plugin_id.clone();
    let prev_attempt = attempt_of(states, &plugin_id);
    states.insert(plugin_id.clone(), MonitorState::Connecting);
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
            states.insert(plugin_id.clone(), MonitorState::Online);
            app.registry.clear_connect_error(&plugin_id);
            broadcast_state(app).await;
        }
        Ok(Err(e)) => {
            backoff(states, &plugin_id, prev_attempt);
            report_connect_failure(app, manifest, &plugin_id, format!("{e:#}")).await;
        }
        Err(join) => {
            backoff(states, &plugin_id, prev_attempt);
            report_connect_failure(
                app,
                manifest,
                &plugin_id,
                format!("probe task panicked: {join}"),
            )
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
        crate::registry::usecases::registration::close_device(app, d).await;
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

    async fn assert_tick_is_a_no_op_under(scope: crate::registry::discovery::DiscoveryScope) {
        crate::test_support::with_tmp_config(|app| async move {
            app.set_discovery_scope(scope).await;
            let root = MockRoot::new("openrgb");
            // Would report a drop if enumerated — but the tick must skip it.
            root.scripted(Err(anyhow::anyhow!("should not be called")));
            app.devices
                .write()
                .await
                .push(root.clone() as Arc<dyn Device>);

            let mut states = HashMap::new();
            tick_once(&app, &mut states).await;

            assert!(root.is_live(), "tick was skipped, so the root stays live");
        })
        .await;
    }

    #[tokio::test]
    async fn discovery_filter_makes_a_tick_a_no_op() {
        use crate::registry::discovery::{DiscoveryFilter, DiscoveryScope};
        assert_tick_is_a_no_op_under(DiscoveryScope::PluginSet {
            plugin_ids: ["openrgb".to_string()].into_iter().collect(),
            filter: Arc::new(DiscoveryFilter { specs: vec![] }),
        })
        .await;
    }

    #[tokio::test]
    async fn a_scoped_reconcile_does_not_pause_an_unrelated_integration() {
        use crate::registry::discovery::{DiscoveryFilter, DiscoveryScope};
        crate::test_support::with_tmp_config(|app| async move {
            // tick_once's loop is driven by integration_manifests(), not just
            // app.devices — register one (consent-satisfied) so the mock root
            // is actually reached.
            register_integration(&app, "openrgb");

            app.set_discovery_scope(DiscoveryScope::PluginSet {
                plugin_ids: ["some-other-plugin".to_string()].into_iter().collect(),
                filter: Arc::new(DiscoveryFilter { specs: vec![] }),
            })
            .await;
            let root = MockRoot::new("openrgb");
            root.scripted(Err(anyhow::anyhow!("dropped")));
            app.devices
                .write()
                .await
                .push(root.clone() as Arc<dyn Device>);

            let mut states = HashMap::new();
            tick_once(&app, &mut states).await;

            assert!(
                !root.is_live(),
                "an unrelated plugin's scope must not pause this integration's tick"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn full_rescan_also_makes_a_tick_a_no_op() {
        use crate::registry::discovery::DiscoveryScope;
        assert_tick_is_a_no_op_under(DiscoveryScope::Full).await;
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

    #[test]
    fn backoff_increments_the_attempt_streak_from_zero() {
        let mut states = HashMap::new();
        assert_eq!(attempt_of(&states, "p"), 0);
        let prev = attempt_of(&states, "p");
        backoff(&mut states, "p", prev);
        assert_eq!(attempt_of(&states, "p"), 1);
        let prev = attempt_of(&states, "p");
        backoff(&mut states, "p", prev);
        assert_eq!(attempt_of(&states, "p"), 2);
    }

    /// Register a consent-satisfied integration manifest for `id` in `app`'s
    /// registry, so `tick_once`'s manifest-driven loop reaches it.
    fn register_integration(app: &Arc<AppState>, id: &str) {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(id);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            format!(
                "id: {id}\ntype: integration\npermissions: [network]\ntransports:\n  tcp:\n    host_key: host\n    port_key: port\nconfig:\n  fields:\n    - key: host\n      label: Host\n      kind: text\n    - key: port\n      label: Port\n      kind: number\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
        let manifest = crate::drivers::plugins::parse_manifest_from_dir(&dir).unwrap();
        app.registry
            .update(|s| s.manifests = vec![manifest.clone()]);
        let mut policy = crate::config::PluginPolicy::default();
        policy.enabled.push(id.to_string());
        policy.integrations_enabled.push(id.to_string());
        policy
            .accepted_authorities
            .insert(id.to_string(), app.registry.authority_for(id).unwrap());
        app.registry.replace_policy(&policy);
    }

    #[tokio::test]
    async fn a_plugin_no_longer_enabled_transitions_to_disabled() {
        crate::test_support::with_tmp_config(|app| async move {
            // No manifest registered for "gone" — it's no longer enabled.
            let mut states = HashMap::from([(
                "gone".to_string(),
                MonitorState::Backoff {
                    attempt: 3,
                    deadline: Instant::now(),
                },
            )]);
            tick_once(&app, &mut states).await;
            assert_eq!(states.get("gone"), Some(&MonitorState::Disabled));
        })
        .await;
    }

    #[tokio::test]
    async fn a_reenabled_plugin_leaves_disabled_and_gets_processed() {
        crate::test_support::with_tmp_config(|app| async move {
            register_integration(&app, "openrgb");
            let root = MockRoot::new("openrgb");
            app.devices
                .write()
                .await
                .push(root.clone() as Arc<dyn Device>);

            let mut states = HashMap::from([("openrgb".to_string(), MonitorState::Disabled)]);
            tick_once(&app, &mut states).await;

            // Disabled -> Offline (seeded) -> Case C (root live, no-op resync) -> Online.
            assert_eq!(states.get("openrgb"), Some(&MonitorState::Online));
        })
        .await;
    }
}
