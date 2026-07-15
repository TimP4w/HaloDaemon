// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-integration enable/disable and config, independent of the generic
//! plugin toggle (which only governs whether the Lua may run at all — see
//! `usecases::plugins`). Unlike plugin edits, these apply immediately and are
//! scoped to the one integration: only its root device and the children it
//! exposes are torn down and rebuilt, never the whole device set.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use super::registration::unregister_device_and_children;
use crate::drivers::plugins::integration_scan;
use crate::state::AppState;

/// Close and drop `id`'s integration root (and the children it exposes) from
/// `app.devices`, if currently registered. No-op otherwise.
async fn disable_one(app: &Arc<AppState>, id: &str) -> Option<String> {
    let root_id = {
        let devices = app.devices.read().await;
        devices
            .iter()
            .find(|d| d.integration_id().as_deref() == Some(id))
            .map(|d| d.id().to_owned())
    };
    if let Some(root_id) = root_id {
        unregister_device_and_children(app, &root_id).await;
        Some(root_id)
    } else {
        None
    }
}

/// Connect and register `id`'s integration root (and its children), if it's
/// currently enabled and permission-satisfied. No-op otherwise.
async fn enable_one(app: &Arc<AppState>, id: &str) {
    let _ = integration_scan::discover_one(app, id).await;
}

/// Enable or disable a single integration, independent of the generic plugin
/// toggle. Applies immediately and only touches this one integration's root
/// + exposed devices — no global rediscovery.
pub async fn set_integration_enabled(id: String, enabled: bool, app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        cfg.plugins.integrations_enabled.retain(|x| x != &id);
        if enabled {
            cfg.plugins.integrations_enabled.push(id.clone());
        }
        app.registry.replace_policy(&cfg.plugins);
    }
    app.request_config_save();

    if enabled {
        enable_one(&app, &id).await;
    } else {
        let root_id = disable_one(&app, &id).await;
        app.registry
            .clear_operational_errors(&id, root_id.as_slice());
    }
    crate::ipc::broadcast_state(&app).await;
    Ok(())
}

/// Replace a single integration's user-editable config values and reconnect
/// just that integration (e.g. a changed host/port takes effect immediately)
/// — every other device is left untouched.
pub async fn set_integration_config(
    id: String,
    values: HashMap<String, String>,
    app: Arc<AppState>,
) -> Result<()> {
    super::plugins::persist_config_values(&id, &values, &app).await?;
    app.request_config_save();

    let root_id = disable_one(&app, &id).await;
    app.registry
        .clear_operational_errors(&id, root_id.as_slice());
    enable_one(&app, &id).await;
    crate::ipc::broadcast_state(&app).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockDevice;
    use std::sync::atomic::Ordering;

    /// An integration-type plugin declaring the one secure config field the
    /// secret-store test exercises. `timeout_ms` is tiny so a scoped reconnect
    /// attempt (nothing is actually listening) fails fast rather than
    /// stalling the test.
    const INTEGRATION_CONFIG_TEST_PLUGIN: &str = "return {}";

    /// Loads `INTEGRATION_CONFIG_TEST_PLUGIN` into `app`'s plugin registry for
    /// the duration of `f`.
    async fn with_integration_config_test_plugin<F, Fut>(app: &Arc<AppState>, f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("inttest");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.yaml"),
            "id: inttest\ntype: integration\npermissions: [network]\ntransports:\n  tcp:\n    host_key: host\n    port_key: port\n    timeout_ms: 50\nconfig:\n  fields:\n    - key: host\n      label: Host\n      default: 127.0.0.1\n    - key: port\n      label: Port\n      default: '12345'\n    - key: token\n      label: Token\n      secure: true\n",
        )
        .unwrap();
        std::fs::write(plugin_dir.join("main.lua"), INTEGRATION_CONFIG_TEST_PLUGIN).unwrap();
        app.registry.load_all(dir.path());
        f().await;
        app.registry.load_all(std::path::Path::new("/nonexistent"));
    }

    #[tokio::test]
    async fn set_integration_enabled_persists_the_policy() {
        crate::test_support::with_tmp_config(|app| async move {
            let root = Arc::new(MockDevice::new("openrgb-root").with_integration_id("openrgb"));
            let child = Arc::new(MockDevice::new("openrgb-root_ctrl_0"));
            let unrelated = Arc::new(MockDevice::new("other-device"));
            {
                let mut devices = app.devices.write().await;
                devices.push(root.clone());
                devices.push(child.clone());
                devices.push(unrelated.clone());
            }

            set_integration_enabled("openrgb".into(), false, app.clone())
                .await
                .unwrap();

            assert!(!app
                .config
                .read()
                .await
                .plugins
                .integrations_enabled
                .contains(&"openrgb".to_string()));
            let remaining = app.devices.read().await;
            assert_eq!(
                remaining.len(),
                1,
                "only the integration's subtree is torn down"
            );
            assert_eq!(remaining[0].id(), "other-device");
            drop(remaining);

            assert!(root.closed.load(Ordering::SeqCst));
            assert!(child.closed.load(Ordering::SeqCst));
            assert!(!unrelated.closed.load(Ordering::SeqCst));
        })
        .await;
    }

    #[tokio::test]
    async fn set_integration_config_splits_secure_values_into_the_secret_store() {
        crate::test_support::with_tmp_config(|app| async move {
            with_integration_config_test_plugin(&app, || async {
                let mut values = HashMap::new();
                values.insert("host".to_string(), "127.0.0.1".to_string());
                values.insert("token".to_string(), "s3cr3t".to_string());
                set_integration_config("inttest".into(), values, app.clone())
                    .await
                    .unwrap();

                let cfg = app.config.read().await;
                assert_eq!(
                    cfg.plugins
                        .config
                        .get("inttest")
                        .and_then(|m| m.get("host")),
                    Some(&"127.0.0.1".to_string())
                );
                assert!(
                    !cfg.plugins
                        .config
                        .get("inttest")
                        .is_some_and(|m| m.contains_key("token")),
                    "a secure value must never land in the plaintext config map"
                );
                drop(cfg);
                assert_eq!(
                    app.secret_store.get("inttest", "token").unwrap(),
                    Some("s3cr3t".to_string())
                );
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn disable_one_is_a_no_op_when_the_integration_is_not_registered() {
        crate::test_support::with_tmp_config(|app| async move {
            let unrelated = Arc::new(MockDevice::new("other-device"));
            app.devices.write().await.push(unrelated.clone());

            disable_one(&app, "does-not-exist").await;

            assert_eq!(app.devices.read().await.len(), 1);
            assert!(!unrelated.closed.load(Ordering::SeqCst));
        })
        .await;
    }
}
