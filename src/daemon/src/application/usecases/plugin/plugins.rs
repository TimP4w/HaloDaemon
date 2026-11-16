// SPDX-License-Identifier: GPL-3.0-or-later
//! Managing repository-provided plugins: enable/disable and configuration.

use crate::domain::events::ChangeSink as _;

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use halod_shared::messages::DaemonMessage;

use crate::application::ipc::ClientHandle;
use crate::application::state::AppState;

pub async fn set_enabled(id: String, enabled: bool, app: Arc<AppState>) -> Result<()> {
    if enabled && !app.registry.supports_current_platform(&id) {
        bail!("plugin '{id}' is not supported on this platform");
    }
    {
        let mut cfg = app.config.write().await;
        cfg.plugins.enabled.retain(|x| x != &id);
        if enabled {
            cfg.plugins.enabled.push(id.clone());
        }
        app.registry.replace_policy(&cfg.plugins);
    }
    app.request_config_save();
    let device_ids = {
        let devices = app.device_registry.read().await;
        devices
            .iter()
            .filter(|device| {
                device.owning_plugin_id().as_deref() == Some(id.as_str())
                    || device.integration_id().as_deref() == Some(id.as_str())
            })
            .map(|device| device.id().to_owned())
            .collect::<Vec<_>>()
    };
    app.registry.clear_operational_errors(&id, &device_ids);
    app.data_bus.invalidate_owner(&id);
    reconcile_plugins(&app, std::slice::from_ref(&id)).await;
    Ok(())
}

/// Split `values` into the plaintext `cfg.plugins.config` map and the secret
/// store (for manifest-declared `secure` fields), then re-publish the
/// plaintext config to the plugin registry and persist. Shared by
/// [`set_config`] and [`super::integrations::set_integration_config`]. An
/// absent (or empty) secure value leaves the
/// previously stored secret untouched, so the GUI never has to round-trip a
/// secret to keep it.
pub(crate) async fn persist_config_values(
    id: &str,
    values: &std::collections::HashMap<String, String>,
    app: &Arc<AppState>,
) -> Result<()> {
    let values = app.registry.normalize_config_values(id, values);
    app.registry.validate_config_values(id, &values)?;
    let secure_keys: std::collections::HashSet<String> = app
        .registry
        .secure_config_keys_for(id)
        .into_iter()
        .collect();
    let secrets = values
        .iter()
        .filter(|(key, value)| secure_keys.contains(*key) && !value.is_empty())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Vec<_>>();
    if !secrets.is_empty() {
        let store = Arc::clone(&app.secret_store);
        let plugin_id = id.to_owned();
        tokio::task::spawn_blocking(move || -> Result<()> {
            for (key, value) in secrets {
                store
                    .set(&plugin_id, &key, &value)
                    .with_context(|| format!("storing secret '{key}' for plugin '{plugin_id}'"))?;
            }
            Ok(())
        })
        .await
        .context("secret-store task failed")??;
    }
    let mut cfg = app.config.write().await;
    let plaintext = cfg.plugins.config.entry(id.to_owned()).or_default();
    for (key, value) in &values {
        if !secure_keys.contains(key) {
            plaintext.insert(key.clone(), value.clone());
        }
    }
    if plaintext.is_empty() {
        cfg.plugins.config.remove(id);
    }
    app.registry.replace_policy(&cfg.plugins);
    Ok(())
}

/// Fetch a plugin's display-only asset and send it to the client as base64. Not staged — a pure read.
pub async fn get_asset(
    plugin_id: String,
    name: String,
    client: ClientHandle,
    app: Arc<AppState>,
) -> Result<()> {
    let bytes = app.registry.read_asset(&plugin_id, &name)?;
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    client.send(&DaemonMessage::PluginAsset {
        plugin_id,
        name,
        data_b64,
    });
    Ok(())
}

pub async fn get_lcd_widget_icon(
    catalog_id: String,
    client: ClientHandle,
    app: Arc<AppState>,
) -> Result<()> {
    let bytes = app.registry.read_widget_icon(&catalog_id)?;
    client.send(&DaemonMessage::LcdWidgetIcon {
        catalog_id,
        data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
    });
    Ok(())
}

pub async fn get_lcd_preset(
    catalog_id: String,
    client: ClientHandle,
    app: Arc<AppState>,
) -> Result<()> {
    let def = app.registry.read_lcd_preset(&catalog_id)?;
    client.send(&DaemonMessage::LcdPluginPreset { catalog_id, def });
    Ok(())
}

/// Enumerate the host's serial ports for a `serial_port` config-field dropdown.
pub async fn list_serial_ports(client: ClientHandle) -> Result<()> {
    let ports = crate::infrastructure::drivers::transports::serial::list_ports();
    client.send(&DaemonMessage::SerialPorts { ports });
    Ok(())
}

/// Return current Linux udev-rule drift without modifying system files.
pub async fn get_udev_rules_status(client: ClientHandle, app: Arc<AppState>) -> Result<()> {
    client.send(&DaemonMessage::UdevRulesStatus {
        data: app.registry.udev_rules_status(),
    });
    Ok(())
}

/// Replace a plugin's user-editable config values and reconcile its devices.
pub async fn set_config(
    id: String,
    values: std::collections::HashMap<String, String>,
    app: Arc<AppState>,
) -> Result<()> {
    persist_config_values(&id, &values, &app).await?;
    app.request_config_save();
    reconcile_plugins(&app, &[id]).await;
    Ok(())
}

/// Purge every id-keyed piece of plugin state (secrets, enable flags, config),
/// returning whether config changed.
pub(crate) async fn purge_plugin_state(id: &str, app: &Arc<AppState>) -> bool {
    let keys = app.registry.secure_config_keys_for(id);
    let store = Arc::clone(&app.secret_store);
    let plugin_id = id.to_owned();
    if let Err(error) = tokio::task::spawn_blocking(move || {
        for key in keys {
            if let Err(error) = store.delete(&plugin_id, &key) {
                log::warn!("deleting secret '{key}' for plugin '{plugin_id}': {error:#}");
            }
        }
    })
    .await
    {
        log::warn!("secret-store cleanup task failed for plugin '{id}': {error}");
    }

    let mut cfg = app.config.write().await;
    let before = cfg.plugins.enabled.len();
    cfg.plugins.enabled.retain(|x| x != id);
    let enabled_changed = cfg.plugins.enabled.len() != before;
    let config_changed = cfg.plugins.config.remove(id).is_some();
    let integ_before = cfg.plugins.integrations_enabled.len();
    cfg.plugins.integrations_enabled.retain(|x| x != id);
    let integ_changed = cfg.plugins.integrations_enabled.len() != integ_before;

    let changed = enabled_changed || config_changed || integ_changed;
    if changed {
        app.registry.replace_policy(&cfg.plugins);
    }
    changed
}

pub(crate) async fn reload_registry(app: &Arc<AppState>) {
    let cfg = app.config.read().await;
    app.registry.load_all(&cfg.plugins.repos);
    app.registry.replace_policy(&cfg.plugins);
    drop(cfg);
    // Manifests just changed, so their derived requirements may have too.
    if let Err(error) = refresh_requirements(app).await {
        log::warn!("refreshing plugin requirements after registry reload failed: {error:#}");
    }
    refresh_recommendations(app).await;
}

/// Run host requirement probes away from Tokio's worker threads. A kernel
/// module probe may wait up to its process timeout, so even this synchronous
/// cache refresh must be treated as blocking work by async callers.
pub(crate) async fn refresh_requirements(app: &Arc<AppState>) -> Result<()> {
    let app = Arc::clone(app);
    tokio::task::spawn_blocking(move || app.registry.refresh_requirements())
        .await
        .context("plugin requirement probe task failed")
}

/// Refresh the passive hardware snapshot used by plugin recommendations. HID
/// and USB enumeration are blocking; SMBus enumeration only lists controllers
/// and does not probe device addresses.
pub(crate) async fn refresh_recommendations(app: &Arc<AppState>) {
    let hid_usb = tokio::task::spawn_blocking(|| {
        (
            crate::application::usecases::registry::debug::enumerate_hid(),
            crate::domain::plugin::recommend::enumerate_usb(),
        )
    });
    let (_, gpu_smbus) =
        crate::infrastructure::drivers::transports::smbus::SmBusTransport::enumerate_for_debug()
            .await;
    let (hid, usb) = match hid_usb.await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            log::warn!("HID/USB recommendation scan task failed: {error}");
            Default::default()
        }
    };
    app.registry.refresh_recommendations(&hid, &usb, &gpu_smbus);
}

/// Close and unregister every currently-registered device owned by one of
/// `plugins` (plus its `_ctrl_` children); leaves every other device untouched.
async fn teardown_owned_devices(app: &Arc<AppState>, plugins: &[String]) {
    let owned_ids: Vec<String> = {
        let devices = app.device_registry.read().await;
        devices
            .iter()
            .filter(|d| {
                d.owning_plugin_id()
                    .is_some_and(|pid| plugins.contains(&pid))
            })
            .map(|d| d.id().to_owned())
            .collect()
    };
    // Untrack the FULL torn-down set (parent + children): a plugin controller and
    // its children share one HID key, and `untrack_devices` only prunes a key
    // once every device it tracks is torn down. Passing only the owning parents
    // would leave the key tracked, so the HID rescan skips it and the device
    // never comes back on re-enable.
    let mut torn_down: std::collections::HashSet<String> = std::collections::HashSet::new();
    for id in &owned_ids {
        let removed =
            crate::application::usecases::registry::registration::unregister_device_and_children(
                app, id,
            )
            .await;
        torn_down.extend(removed);
    }
    app.hid.untrack_devices(&torn_down).await;
}

/// Scoped teardown + reprobe for `plugins`: only devices owned by one of
/// these plugin ids are closed and re-discovered; every other device is
/// left untouched.
pub(crate) async fn reconcile_plugins(app: &Arc<AppState>, plugins: &[String]) {
    use crate::domain::registry::observers::discovery::PendingRediscovery;
    // Refresh the requirement cache before rediscovery so a satisfied↔missing
    // transition gates activation this cycle (recovery reactivates, loss tears down).
    if let Err(error) = refresh_requirements(app).await {
        log::warn!("refreshing plugin requirements before rediscovery failed: {error:#}");
        return;
    }
    app.merge_rediscovery(PendingRediscovery::PluginSet(
        plugins.iter().cloned().collect(),
    ))
    .await;
    drain_rediscovery(app).await;
}

pub(crate) async fn reconcile_full(app: &Arc<AppState>) {
    if let Err(error) = refresh_requirements(app).await {
        log::warn!("refreshing plugin requirements before full rediscovery failed: {error:#}");
        return;
    }
    app.merge_rediscovery(crate::domain::registry::observers::discovery::PendingRediscovery::Full)
        .await;
    drain_rediscovery(app).await;
}

async fn drain_rediscovery(app: &Arc<AppState>) {
    use crate::domain::registry::observers::discovery::{DiscoveryScope, PendingRediscovery};
    let _runner = app.rediscovery_runner.lock().await;
    loop {
        match app.take_rediscovery().await {
            PendingRediscovery::Clean => return,
            PendingRediscovery::Full => {
                reload_registry(app).await;
                app.set_discovery_scope(DiscoveryScope::Full).await;
                crate::domain::registry::observers::discovery::discover_devices(Arc::clone(app))
                    .await;
                app.set_discovery_scope(DiscoveryScope::Clean).await;
            }
            PendingRediscovery::PluginSet(plugin_ids) => {
                reconcile_plugin_set(app, &plugin_ids.into_iter().collect::<Vec<_>>()).await;
            }
        }
    }
}

async fn reconcile_plugin_set(app: &Arc<AppState>, plugins: &[String]) {
    use crate::domain::registry::observers::discovery::{DiscoveryFilter, DiscoveryScope};

    // Keep both sides of a manifest change in scope. Deleted/disabled plugins
    // need their old specs so a built-in host device can reclaim the hardware, while
    // newly imported or updated plugins need their new specs to claim it.
    let mut specs = app.registry.device_specs_for(plugins);

    // 1. Refresh manifests + disabled/granted/config so make_device reflects
    //    the new state, then add the post-change specs.
    reload_registry(app).await;
    specs.extend(app.registry.device_specs_for(plugins));

    // 2. Teardown: close and unregister every device owned by a changed plugin.
    teardown_owned_devices(app, plugins).await;

    // 3. Scope re-probing to only these plugins' hardware.
    app.set_discovery_scope(DiscoveryScope::PluginSet {
        plugin_ids: plugins.iter().cloned().collect(),
        filter: Arc::new(DiscoveryFilter { specs }),
    })
    .await;

    // 4. Scoped re-probe.
    crate::domain::registry::observers::discovery::discover_devices(Arc::clone(app)).await;

    // 5. Clear the scope, then seed known-device records and restore any
    //    chain layout for the newly registered devices. Each new device's own
    //    profile state was already applied by `register_device` during the
    //    scoped probe, so we deliberately skip the global `load_active_profile`
    //    here — it would clear every device's LCD slot and re-load every
    //    device's state, disturbing the untouched devices this path exists to
    //    leave alone (mirroring the integration scoped path).
    app.set_discovery_scope(DiscoveryScope::Clean).await;
    crate::domain::registry::seed_known_devices(Arc::clone(app)).await;
    crate::application::usecases::registry::chain::restore_saved_chains(Arc::clone(app)).await;
    app.record_change(crate::domain::events::Change::PluginTopology)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn set_enabled_reconciles_without_touching_unrelated_devices() {
        crate::test_support::with_tmp_config(|app| async move {
            app.device_registry.write().await.push(std::sync::Arc::new(
                crate::test_support::MockDevice::new("stays-open"),
            ));

            set_enabled("some_plugin".into(), false, app.clone())
                .await
                .unwrap();

            assert_eq!(
                app.device_registry.read().await.len(),
                1,
                "scoped reconciliation must leave unrelated devices alone"
            );
            assert!(!app
                .config
                .read()
                .await
                .plugins
                .enabled
                .contains(&"some_plugin".to_string()));
        })
        .await;
    }

    #[tokio::test]
    async fn scoped_teardown_prunes_hid_tracking_so_reprobe_can_readd() {
        // Regression: the scoped teardown must drop the torn-down device's HID
        // key, or the HID rescan skips the still-tracked key and the device never
        // comes back on re-enable/config-change (the full path clears all HID
        // tracking; the scoped path must clear just these keys).
        use crate::domain::registry::HidTrackingEntry;
        use crate::test_support::MockDevice;
        use std::sync::Arc;

        crate::test_support::with_tmp_config(|app| async move {
            let owned: Arc<dyn crate::domain::device::Device> =
                Arc::new(MockDevice::new("P-dev").with_owning_plugin_id("P"));
            let other: Arc<dyn crate::domain::device::Device> =
                Arc::new(MockDevice::new("Q-dev").with_owning_plugin_id("Q"));
            app.device_registry.write().await.push(owned.clone());
            app.device_registry.write().await.push(other.clone());
            app.hid
                .track("1234:5678:P".into(), HidTrackingEntry::Primary(vec![owned]))
                .await;
            app.hid
                .track("9abc:def0:Q".into(), HidTrackingEntry::Primary(vec![other]))
                .await;

            teardown_owned_devices(&app, &["P".to_string()]).await;

            let keys = app.hid.keys().await;
            assert!(
                !keys.contains("1234:5678:P"),
                "torn-down plugin's HID key must be untracked so a re-probe re-adds it"
            );
            assert!(
                keys.contains("9abc:def0:Q"),
                "an untouched plugin's HID key must survive the scoped teardown"
            );
            assert!(app.find_device_by_id("P-dev").await.is_none());
            assert!(app.find_device_by_id("Q-dev").await.is_some());
        })
        .await;
    }

    #[tokio::test]
    async fn scoped_teardown_untracks_a_key_shared_by_a_parent_and_its_children() {
        // Regression: a plugin controller and its children (e.g. a fan hub and
        // its fans) share one HID key. Only the parent carries the owning
        // plugin id, but `untrack_devices` prunes a key only once EVERY device it
        // tracks is torn down — so teardown must feed it the whole subtree, not
        // just the owning parents, or the key survives and the re-probe skips the
        // device.
        use crate::domain::registry::HidTrackingEntry;
        use crate::test_support::MockDevice;
        use std::sync::Arc;

        crate::test_support::with_tmp_config(|app| async move {
            let parent: Arc<dyn crate::domain::device::Device> =
                Arc::new(MockDevice::new("hub-abc").with_owning_plugin_id("hub_plugin"));
            // Chain-accessory child: shares the parent key, no owning id of its own.
            let child: Arc<dyn crate::domain::device::Device> =
                Arc::new(MockDevice::new("hub-abc_acc_0_1"));
            app.device_registry.write().await.push(parent.clone());
            app.device_registry.write().await.push(child.clone());
            app.hid
                .track(
                    "1e71:2022:S".into(),
                    HidTrackingEntry::Primary(vec![parent, child]),
                )
                .await;

            teardown_owned_devices(&app, &["hub_plugin".to_string()]).await;

            assert!(
                app.hid.keys().await.is_empty(),
                "the shared key must be untracked once the whole subtree is torn down"
            );
            assert!(app.find_device_by_id("hub-abc").await.is_none());
            assert!(app.find_device_by_id("hub-abc_acc_0_1").await.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn teardown_owned_devices_removes_only_the_owning_plugins_subtree() {
        use crate::test_support::MockDevice;
        crate::test_support::with_tmp_config(|app| async move {
            let root = Arc::new(MockDevice::new("p1-abc").with_owning_plugin_id("p1"));
            // A child registered alongside the plugin root (the `_ctrl_` scheme
            // `unregister_device_and_children` prunes) — no owner id of its own.
            let child = Arc::new(MockDevice::new("p1-abc_ctrl_0"));
            let other_plugin = Arc::new(MockDevice::new("p2-xyz").with_owning_plugin_id("p2"));
            let native = Arc::new(MockDevice::new("native-dev"));
            {
                let mut devices = app.device_registry.write().await;
                devices.push(root.clone());
                devices.push(child.clone());
                devices.push(other_plugin.clone());
                devices.push(native.clone());
            }

            teardown_owned_devices(&app, &["p1".to_string()]).await;

            let remaining: Vec<String> = app
                .device_registry
                .read()
                .await
                .iter()
                .map(|d| d.id().to_owned())
                .collect();
            assert_eq!(remaining, vec!["p2-xyz", "native-dev"]);

            assert!(root.closed.load(Ordering::SeqCst));
            assert!(child.closed.load(Ordering::SeqCst));
            assert!(!other_plugin.closed.load(Ordering::SeqCst));
            assert!(!native.closed.load(Ordering::SeqCst));
        })
        .await;
    }

    #[tokio::test]
    async fn teardown_owned_devices_is_a_noop_when_nothing_is_owned() {
        use crate::test_support::MockDevice;
        crate::test_support::with_tmp_config(|app| async move {
            let native = Arc::new(MockDevice::new("native-dev"));
            app.device_registry.write().await.push(native.clone());

            teardown_owned_devices(&app, &["p1".to_string()]).await;

            assert_eq!(app.device_registry.read().await.len(), 1);
            assert!(!native.closed.load(Ordering::SeqCst));
        })
        .await;
    }

    const CONFIG_TEST_PLUGIN: &str = "return {}";

    /// Every declaration lives in YAML; the Lua entry contains callbacks only.
    const CONFIG_TEST_PLUGIN_YAML: &str =
        "id: cfgtest\nversion: 1.0.0\napi: \"3.0\"\ncapabilities: [lighting]\ndevices:\n  - vendor: x\n    model: y\n    type: led_strip\n    match:\n      hid: { vid: 1, pid: 2 }\nconfig:\n  fields:\n    - key: host\n      label: Host\n    - key: token\n      label: Token\n      secure: true\n";

    fn write_config_test_plugin(root: &std::path::Path) {
        let dir = root.join("cfgtest");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.yaml"), CONFIG_TEST_PLUGIN_YAML).unwrap();
        std::fs::write(dir.join("main.lua"), CONFIG_TEST_PLUGIN).unwrap();
    }

    const CONFIG_TEST_REPO_SLUG: &str = "cfgtest-repo";

    fn config_test_repo_record() -> crate::config::PluginRepoRecord {
        crate::config::PluginRepoRecord {
            slug: CONFIG_TEST_REPO_SLUG.to_owned(),
            source: crate::config::PluginRepoSource::Url("https://example.org/catalog.yaml".into()),
            trusted_key: None,
        }
    }

    /// Requires `with_tmp_config`.
    async fn with_config_test_plugin<F, Fut>(app: &Arc<AppState>, f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let record = config_test_repo_record();
        write_config_test_plugin(&crate::domain::plugin::catalog::repo_dir(&record.slug));
        app.config.write().await.plugins.repos.push(record);
        reload_registry(app).await;
        f().await;
        app.config
            .write()
            .await
            .plugins
            .repos
            .retain(|r| r.slug != CONFIG_TEST_REPO_SLUG);
        reload_registry(app).await;
    }

    #[tokio::test]
    async fn enabling_and_disabling_persist_without_consent() {
        crate::test_support::with_tmp_config(|app| async move {
            with_config_test_plugin(&app, || async {
                let enabled = || async {
                    app.config
                        .read()
                        .await
                        .plugins
                        .enabled
                        .contains(&"cfgtest".to_owned())
                };
                set_enabled("cfgtest".into(), true, app.clone())
                    .await
                    .unwrap();
                assert!(enabled().await);
                set_enabled("cfgtest".into(), false, app.clone())
                    .await
                    .unwrap();
                assert!(!enabled().await);
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn enable_rejects_platform_unsupported_plugin() {
        crate::test_support::with_tmp_config(|app| async move {
            let dir = crate::config::plugins_dir();
            std::fs::create_dir_all(dir.join("winonly")).unwrap();
            // Declares a platform we are not running on, so it can never activate.
            let foreign = if std::env::consts::OS == "windows" {
                "linux"
            } else {
                "windows"
            };
            std::fs::write(
                dir.join("winonly").join("plugin.yaml"),
                format!("id: winonly\nversion: 1.0.0\napi: \"3.0\"\nplatforms: [{foreign}]\ncapabilities: [lighting]\ndevices:\n  - vendor: x\n    model: y\n    type: led_strip\n    match:\n      hid: {{ vid: 1, pid: 2 }}\n"),
            )
            .unwrap();
            std::fs::write(dir.join("winonly").join("main.lua"), CONFIG_TEST_PLUGIN).unwrap();
            app.registry.load_dir(&dir);

            let result = set_enabled("winonly".into(), true, app.clone()).await;

            let error = result.expect_err("enable must be rejected on an unsupported platform");
            assert!(error.to_string().contains("not supported on this platform"));
            assert!(
                !app.config
                    .read()
                    .await
                    .plugins
                    .enabled
                    .contains(&"winonly".to_owned()),
                "an unsupported plugin must never be persisted as enabled"
            );

            app.registry.load_dir(std::path::Path::new("/nonexistent"));
        })
        .await;
    }

    fn test_client() -> (
        ClientHandle,
        tokio::sync::mpsc::Receiver<std::sync::Arc<Vec<u8>>>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel::<std::sync::Arc<Vec<u8>>>(16);
        let client = ClientHandle {
            id: 0,
            tx,
            subs: std::sync::Arc::default(),
        };
        (client, rx)
    }

    #[tokio::test]
    async fn get_asset_replies_with_base64_bytes() {
        let app = Arc::new(AppState::new(crate::config::Config::default()));
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("assetplug");
        std::fs::create_dir_all(plugin_dir.join("assets")).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.yaml"),
            "id: assetplug\nversion: 1.0.0\napi: \"3.0\"\ncapabilities: [lighting]\ndevices:\n  - vendor: x\n    model: y\n    type: led_strip\n    match:\n      hid: { vid: 1, pid: 2 }\n\
             logo: logo.png\n",
        )
        .unwrap();
        std::fs::write(plugin_dir.join("main.lua"), "return {}").unwrap();
        let png = {
            let img = image::RgbaImage::from_pixel(16, 16, image::Rgba([1, 2, 3, 255]));
            let mut b = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut b, image::ImageFormat::Png)
                .unwrap();
            b.into_inner()
        };
        std::fs::write(plugin_dir.join("assets/logo.png"), &png).unwrap();
        app.registry.load_dir(dir.path());

        let (client, mut rx) = test_client();
        get_asset("assetplug".into(), "logo.png".into(), client, app.clone())
            .await
            .unwrap();

        let frame = rx.try_recv().expect("a frame was queued");
        let v: serde_json::Value = serde_json::from_slice(&frame[5..]).unwrap();
        assert_eq!(v["type"], "plugin_asset");
        assert_eq!(v["plugin_id"], "assetplug");
        assert_eq!(v["name"], "logo.png");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(v["data_b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, png);

        app.registry.load_dir(std::path::Path::new("/nonexistent"));
    }

    #[tokio::test]
    async fn get_asset_errors_for_unknown_plugin() {
        let app = Arc::new(AppState::new(crate::config::Config::default()));
        let (client, _rx) = test_client();
        let err = get_asset("does-not-exist".into(), "logo.png".into(), client, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown plugin"));
    }

    #[tokio::test]
    async fn set_config_splits_secure_values_into_the_secret_store() {
        crate::test_support::with_tmp_config(|app| async move {
            with_config_test_plugin(&app, || async {
                let mut values = std::collections::HashMap::new();
                values.insert("host".to_string(), "127.0.0.1".to_string());
                values.insert("token".to_string(), "s3cr3t".to_string());
                set_config("cfgtest".into(), values, app.clone())
                    .await
                    .unwrap();

                let cfg = app.config.read().await;
                assert_eq!(
                    cfg.plugins
                        .config
                        .get("cfgtest")
                        .and_then(|m| m.get("host")),
                    Some(&"127.0.0.1".to_string())
                );
                assert!(
                    !cfg.plugins
                        .config
                        .get("cfgtest")
                        .is_some_and(|m| m.contains_key("token")),
                    "a secure value must never land in the plaintext config map"
                );
                drop(cfg);
                assert_eq!(
                    app.secret_store.get("cfgtest", "token").unwrap(),
                    Some("s3cr3t".to_string())
                );
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn set_config_with_blank_secure_value_keeps_the_existing_secret() {
        crate::test_support::with_tmp_config(|app| async move {
            with_config_test_plugin(&app, || async {
                let mut first = std::collections::HashMap::new();
                first.insert("token".to_string(), "s3cr3t".to_string());
                set_config("cfgtest".into(), first, app.clone())
                    .await
                    .unwrap();

                // Re-saving with an empty secure value must not clear it.
                let mut second = std::collections::HashMap::new();
                second.insert("token".to_string(), "".to_string());
                set_config("cfgtest".into(), second, app.clone())
                    .await
                    .unwrap();

                assert_eq!(
                    app.secret_store.get("cfgtest", "token").unwrap(),
                    Some("s3cr3t".to_string())
                );
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn set_config_validates_every_field_kind_at_ingress() {
        crate::test_support::with_tmp_config(|app| async move {
            let dir = crate::config::plugins_dir();
            let pdir = dir.join("numcfg");
            std::fs::create_dir_all(&pdir).unwrap();
            std::fs::write(
                pdir.join("plugin.yaml"),
                "id: numcfg\nversion: 1.0.0\napi: \"3.0\"\ncapabilities: [lighting]\ndevices:\n  - vendor: x\n    model: y\n    type: led_strip\n    match:\n      hid: { vid: 1, pid: 2 }\nconfig:\n  fields:\n    - { key: hz, label: Hz, kind: number, default: 50, min: 1, max: 100 }\n    - { key: enabled, label: Enabled, kind: boolean, default: true }\n    - { key: mode, label: Mode, kind: enum, default: auto, options: [auto, manual] }\n    - { key: host, label: Host, kind: host, default: localhost }\n    - { key: port, label: Port, kind: port, default: 8080 }\n    - { key: endpoint, label: Endpoint, kind: url, default: 'https://example.com' }\n    - { key: timeout, label: Timeout, kind: duration_ms, default: 1000, min: 10, max: 5000 }\n",
            )
            .unwrap();
            std::fs::write(
                pdir.join("main.lua"),
                "return {}",
            )
            .unwrap();
            app.registry.load_dir(&dir);

            let resolved = app.registry.resolved_config_for(
                app.secret_store.as_ref(),
                "numcfg",
                &[],
            );
            assert_eq!(
                resolved.get("enabled"),
                Some(&crate::domain::plugin::ResolvedConfigValue::Boolean(true))
            );
            assert_eq!(
                resolved.get("hz"),
                Some(&crate::domain::plugin::ResolvedConfigValue::Number(50.0))
            );
            assert_eq!(
                resolved.get("port"),
                Some(&crate::domain::plugin::ResolvedConfigValue::Integer(8080))
            );
            assert_eq!(
                resolved.get("timeout"),
                Some(&crate::domain::plugin::ResolvedConfigValue::Integer(1000))
            );
            assert_eq!(
                resolved.get("mode"),
                Some(&crate::domain::plugin::ResolvedConfigValue::String("auto".into()))
            );

            let one = |k: &str, v: &str| {
                let mut m = std::collections::HashMap::new();
                m.insert(k.to_string(), v.to_string());
                m
            };
            assert!(set_config("numcfg".into(), one("nope", "1"), app.clone())
                .await
                .is_err());
            assert!(set_config("numcfg".into(), one("hz", "999"), app.clone())
                .await
                .is_err());
            assert!(set_config("numcfg".into(), one("hz", "abc"), app.clone())
                .await
                .is_err());
            for (key, invalid) in [
                ("enabled", "yes"),
                ("mode", "invalid"),
                ("host", "bad host"),
                ("port", "0"),
                ("port", "65536"),
                ("endpoint", "/relative"),
                ("endpoint", "ftp://example.com"),
                ("timeout", "-1"),
                ("timeout", "1.5"),
            ] {
                assert!(
                    app.registry
                        .validate_config_values("numcfg", &one(key, invalid))
                        .is_err(),
                    "{key} accepted invalid value {invalid}"
                );
            }
            for (key, valid) in [
                ("enabled", "false"),
                ("mode", "manual"),
                ("host", "127.0.0.1"),
                ("port", "443"),
                ("endpoint", "https://example.com/api"),
                ("timeout", "2500"),
            ] {
                app.registry
                    .validate_config_values("numcfg", &one(key, valid))
                    .unwrap();
            }
            set_config("numcfg".into(), one("hz", "50"), app.clone())
                .await
                .unwrap();

            app.registry.load_dir(std::path::Path::new("/nonexistent"));
        })
        .await;
    }
}
