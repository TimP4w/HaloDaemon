// SPDX-License-Identifier: GPL-3.0-or-later
//! Managing device plugins: enable/disable, import, and delete.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use halod_shared::types::Permission;

use crate::state::AppState;

/// Enable or disable a plugin and persist the choice. Staged — see the module
/// docs; call [`apply_pending_changes`] to hand the device to/from its
/// native driver.
pub async fn set_enabled(id: String, enabled: bool, app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        cfg.plugins_disabled.retain(|x| x != &id);
        if !enabled {
            cfg.plugins_disabled.push(id.clone());
        }
        crate::drivers::plugins::set_disabled(&cfg.plugins_disabled);
    }
    app.request_config_save();
    mark_pending_and_broadcast(&app).await;
    Ok(())
}

/// Replace the set of permissions granted to a plugin and persist the
/// choice. Staged — see the module docs.
pub async fn set_permissions(
    id: String,
    granted: Vec<Permission>,
    app: Arc<AppState>,
) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        if granted.is_empty() {
            cfg.plugin_permissions.remove(&id);
        } else {
            cfg.plugin_permissions.insert(id, granted);
        }
        crate::drivers::plugins::set_granted(&cfg.plugin_permissions);
    }
    app.request_config_save();
    mark_pending_and_broadcast(&app).await;
    Ok(())
}

/// Replace a plugin's user-editable config values and persist the choice.
/// Values keyed to a manifest-declared `secure` field go through
/// [`AppState::secret_store`] instead of the plaintext config file; an absent
/// (or empty) secure key leaves the previously stored secret untouched, so
/// the GUI never has to round-trip a secret to keep it. Staged.
pub async fn set_config(
    id: String,
    values: std::collections::HashMap<String, String>,
    app: Arc<AppState>,
) -> Result<()> {
    let secure_keys = crate::drivers::plugins::secure_config_keys_for(&id);
    {
        let mut cfg = app.config.write().await;
        let plaintext = cfg.plugin_config.entry(id.clone()).or_default();
        for (key, value) in &values {
            if secure_keys.iter().any(|k| k == key) {
                if !value.is_empty() {
                    app.secret_store
                        .set(&id, key, value)
                        .with_context(|| format!("storing secret '{key}' for plugin '{id}'"))?;
                }
            } else {
                plaintext.insert(key.clone(), value.clone());
            }
        }
        if plaintext.is_empty() {
            cfg.plugin_config.remove(&id);
        }
        crate::drivers::plugins::set_config_values(&cfg.plugin_config);
    }
    app.request_config_save();
    mark_pending_and_broadcast(&app).await;
    Ok(())
}

/// Install a Lua plugin script into the plugins directory. The source is
/// validated as a manifest first, so a malformed script is rejected before
/// any file is written. Staged — see the module docs.
pub async fn import(filename: String, source: String, app: Arc<AppState>) -> Result<()> {
    let name = sanitize_lua_filename(&filename);
    let manifest = crate::drivers::plugins::parse_manifest(&source, Path::new(&name))
        .context("plugin script is not a valid manifest")?;

    let dir = crate::config::plugins_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating plugins dir {}", dir.display()))?;
    let path = dir.join(&name);
    std::fs::write(&path, source).with_context(|| format!("writing plugin {}", path.display()))?;
    log::info!("Imported plugin {}", path.display());

    // A manual import gets the GUI's blocking consent modal instead of the
    // auto-discovery toast — suppress it before the reload below would
    // otherwise fire one for this exact plugin.
    crate::drivers::plugins::suppress_permission_notice(&manifest.plugin_id);

    reload_registry(&app).await;
    mark_pending_and_broadcast(&app).await;
    Ok(())
}

/// Delete a user plugin script by id. Built-in plugins have no on-disk
/// script and are refused. Staged — see the module docs.
pub async fn delete(id: String, app: Arc<AppState>) -> Result<()> {
    if crate::drivers::plugins::is_builtin(&id) {
        bail!("cannot delete built-in plugin '{id}'");
    }
    let path = crate::config::plugins_dir().join(format!("{id}.lua"));
    match std::fs::remove_file(&path) {
        Ok(()) => log::info!("Deleted plugin {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::warn!("Plugin {} already gone", path.display());
        }
        Err(e) => return Err(e).with_context(|| format!("deleting {}", path.display())),
    }

    // Purge any stored secrets before the registry reload drops the manifest
    // that names which keys were secure.
    for key in crate::drivers::plugins::secure_config_keys_for(&id) {
        if let Err(e) = app.secret_store.delete(&id, &key) {
            log::warn!("deleting secret '{key}' for plugin '{id}': {e:#}");
        }
    }

    // Drop it from the disabled set and plaintext config too, so a later
    // re-import starts clean.
    let changed = {
        let mut cfg = app.config.write().await;
        let before = cfg.plugins_disabled.len();
        cfg.plugins_disabled.retain(|x| x != &id);
        let disabled_changed = cfg.plugins_disabled.len() != before;
        let config_changed = cfg.plugin_config.remove(&id).is_some();
        if disabled_changed {
            crate::drivers::plugins::set_disabled(&cfg.plugins_disabled);
        }
        if config_changed {
            crate::drivers::plugins::set_config_values(&cfg.plugin_config);
        }
        disabled_changed || config_changed
    };
    if changed {
        app.request_config_save();
    }

    reload_registry(&app).await;
    mark_pending_and_broadcast(&app).await;
    Ok(())
}

pub async fn apply_pending_changes(app: Arc<AppState>) -> Result<()> {
    app.plugins_rediscover_pending
        .store(false, Ordering::Relaxed);
    rediscover_devices(app).await;
    Ok(())
}

/// Re-read the plugins directory (picking up added/removed scripts) and
/// re-apply the disabled/granted sets, without touching live devices.
async fn reload_registry(app: &Arc<AppState>) {
    crate::drivers::plugins::load_all(&crate::config::plugins_dir());
    let cfg = app.config.read().await;
    crate::drivers::plugins::set_disabled(&cfg.plugins_disabled);
    crate::drivers::plugins::set_granted(&cfg.plugin_permissions);
    crate::drivers::plugins::set_config_values(&cfg.plugin_config);
    crate::drivers::plugins::set_secret_store(app.secret_store.clone());
}

/// Flag a rediscovery as needed and push the updated plugin listing (and the
/// pending flag itself) so the GUI can show it immediately.
async fn mark_pending_and_broadcast(app: &Arc<AppState>) {
    app.plugins_rediscover_pending
        .store(true, Ordering::Relaxed);
    crate::ipc::broadcast_state(app).await;
}

/// Clean-slate re-discovery: close every device and re-run the startup path,
/// then broadcast.
async fn rediscover_devices(app: Arc<AppState>) {
    let previous = {
        let mut devices = app.devices.write().await;
        std::mem::take(&mut *devices)
    };
    for device in &previous {
        device.close().await;
    }
    drop(previous);

    app.hid.clear().await;

    crate::registry::initialize_app_state(app.clone()).await;
    crate::ipc::broadcast_state(&app).await;
}

/// Sanitize a user-supplied file name into a safe `<slug>.lua` file name: the
/// stem lower-cased to `[a-z0-9-]` with runs of other characters collapsed to a
/// single `-`, trimmed, then the `.lua` extension. Guards against path
/// traversal (separators become `-`) and empty names.
fn sanitize_lua_filename(filename: &str) -> String {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mut slug = String::new();
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "plugin" } else { slug };
    format!("{slug}.lua")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_slugs_and_forces_lua_extension() {
        assert_eq!(sanitize_lua_filename("My Driver.lua"), "my-driver.lua");
        assert_eq!(sanitize_lua_filename("wled_udp"), "wled-udp.lua");
        assert_eq!(sanitize_lua_filename("a  b--c.lua"), "a-b-c.lua");
    }

    #[test]
    fn sanitize_strips_path_traversal_and_handles_empty() {
        // A path separator can never survive into the written file name.
        assert!(!sanitize_lua_filename("../../etc/passwd").contains('/'));
        assert_eq!(sanitize_lua_filename("///"), "plugin.lua");
        assert_eq!(sanitize_lua_filename(""), "plugin.lua");
    }

    #[tokio::test]
    async fn set_enabled_stages_without_touching_live_devices() {
        crate::test_support::with_tmp_config(|app| async move {
            app.devices.write().await.push(std::sync::Arc::new(
                crate::test_support::MockDevice::new("stays-open"),
            ));

            set_enabled("some_plugin".into(), false, app.clone())
                .await
                .unwrap();

            assert!(
                app.plugins_rediscover_pending.load(Ordering::Relaxed),
                "staged edit must flag a pending rediscover"
            );
            assert_eq!(
                app.devices.read().await.len(),
                1,
                "staging must not close/reopen live devices"
            );
            assert!(app
                .config
                .read()
                .await
                .plugins_disabled
                .contains(&"some_plugin".to_string()));
        })
        .await;
    }

    #[tokio::test]
    async fn apply_pending_changes_clears_the_flag() {
        // Real discovery runs here (initialize_app_state hits actual hardware
        // on the test machine), so this only asserts the flag transition —
        // asserting on the resulting device set would depend on what hardware
        // happens to be attached to the runner.
        crate::test_support::with_tmp_config(|app| async move {
            app.plugins_rediscover_pending
                .store(true, Ordering::Relaxed);

            apply_pending_changes(app.clone()).await.unwrap();

            assert!(!app.plugins_rediscover_pending.load(Ordering::Relaxed));
        })
        .await;
    }

    const CONFIG_TEST_PLUGIN: &str = r#"
        return {
          identity = { vendor = "x", model = "y" },
          match = { transport = "hid", vid = 1, pid = 2 },
          config = { fields = {
            { key = "host", label = "Host" },
            { key = "token", label = "Token", secure = true },
          } },
        }
    "#;

    /// Loads `CONFIG_TEST_PLUGIN` into the (process-wide) plugin registry for
    /// the duration of `f`, then restores the registry to just the built-ins.
    /// Callers must already hold `TEST_GLOBALS_LOCK`.
    async fn with_config_test_plugin<F, Fut>(f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cfgtest.lua"), CONFIG_TEST_PLUGIN).unwrap();
        crate::drivers::plugins::load_all(dir.path());
        f().await;
        crate::drivers::plugins::load_all(std::path::Path::new("/nonexistent"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn set_config_splits_secure_values_into_the_secret_store() {
        let _guard = crate::drivers::plugins::TEST_GLOBALS_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::test_support::with_tmp_config(|app| async move {
            with_config_test_plugin(|| async {
                let mut values = std::collections::HashMap::new();
                values.insert("host".to_string(), "127.0.0.1".to_string());
                values.insert("token".to_string(), "s3cr3t".to_string());
                set_config("cfgtest".into(), values, app.clone())
                    .await
                    .unwrap();

                let cfg = app.config.read().await;
                assert_eq!(
                    cfg.plugin_config.get("cfgtest").and_then(|m| m.get("host")),
                    Some(&"127.0.0.1".to_string())
                );
                assert!(
                    !cfg.plugin_config
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
    #[allow(clippy::await_holding_lock)]
    async fn set_config_with_blank_secure_value_keeps_the_existing_secret() {
        let _guard = crate::drivers::plugins::TEST_GLOBALS_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::test_support::with_tmp_config(|app| async move {
            with_config_test_plugin(|| async {
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
    #[allow(clippy::await_holding_lock)]
    async fn delete_purges_the_plugins_stored_secret() {
        let _guard = crate::drivers::plugins::TEST_GLOBALS_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::test_support::with_tmp_config(|app| async move {
            let dir = crate::config::plugins_dir();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("cfgtest.lua"), CONFIG_TEST_PLUGIN).unwrap();
            crate::drivers::plugins::load_all(&dir);

            let mut values = std::collections::HashMap::new();
            values.insert("token".to_string(), "s3cr3t".to_string());
            set_config("cfgtest".into(), values, app.clone())
                .await
                .unwrap();
            assert_eq!(
                app.secret_store.get("cfgtest", "token").unwrap(),
                Some("s3cr3t".to_string())
            );

            delete("cfgtest".into(), app.clone()).await.unwrap();

            assert_eq!(app.secret_store.get("cfgtest", "token").unwrap(), None);
        })
        .await;
    }
}
