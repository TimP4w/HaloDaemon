// SPDX-License-Identifier: GPL-3.0-or-later
//! Managing device plugins: enable/disable, import, and delete.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use halod_shared::types::Permission;
use serde_json::json;

use crate::ipc::ClientHandle;
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

/// Replace the set of permissions granted to a plugin and persist the choice.
/// Also records the plugin's current script hash as acknowledged: granting is
/// an explicit consent to run *this* script, so the plugin activates and stays
/// active only until its content changes (trust-on-first-use). Staged — see the
/// module docs.
pub async fn set_permissions(
    id: String,
    granted: Vec<Permission>,
    app: Arc<AppState>,
) -> Result<()> {
    let hash = crate::drivers::plugins::content_hash_for(&id);
    {
        let mut cfg = app.config.write().await;
        if granted.is_empty() {
            // Revoke: drop the grant and its content pin, back to pristine.
            cfg.plugin_permissions.remove(&id);
            cfg.plugin_acknowledged.remove(&id);
        } else {
            cfg.plugin_permissions.insert(id.clone(), granted);
            // Pin the grant to the exact script the user is consenting to.
            match hash {
                Some(h) => {
                    cfg.plugin_acknowledged.insert(id, h);
                }
                None => {
                    cfg.plugin_acknowledged.remove(&id);
                }
            }
        }
        crate::drivers::plugins::set_granted(&cfg.plugin_permissions);
        crate::drivers::plugins::set_acknowledged(&cfg.plugin_acknowledged);
    }
    app.request_config_save();
    mark_pending_and_broadcast(&app).await;
    Ok(())
}

/// Split `values` into the plaintext `cfg.plugin_config` map and the secret
/// store (for manifest-declared `secure` fields), then re-publish the
/// plaintext config to the plugin registry and persist. Shared by
/// [`set_config`] (staged, applies via [`apply_pending_changes`]) and
/// [`super::integrations::set_integration_config`] (applies immediately,
/// scoped to one integration). An absent (or empty) secure value leaves the
/// previously stored secret untouched, so the GUI never has to round-trip a
/// secret to keep it.
pub(crate) async fn persist_config_values(
    id: &str,
    values: &std::collections::HashMap<String, String>,
    app: &Arc<AppState>,
) -> Result<()> {
    let secure_keys: std::collections::HashSet<String> =
        crate::drivers::plugins::secure_config_keys_for(id)
            .into_iter()
            .collect();
    let mut cfg = app.config.write().await;
    let plaintext = cfg.plugin_config.entry(id.to_owned()).or_default();
    for (key, value) in values {
        if secure_keys.contains(key) {
            if !value.is_empty() {
                app.secret_store
                    .set(id, key, value)
                    .with_context(|| format!("storing secret '{key}' for plugin '{id}'"))?;
            }
        } else {
            plaintext.insert(key.clone(), value.clone());
        }
    }
    if plaintext.is_empty() {
        cfg.plugin_config.remove(id);
    }
    crate::drivers::plugins::set_config_values(&cfg.plugin_config);
    Ok(())
}

/// Fetch a plugin's display-only asset and send it to the client as base64. Not staged — a pure read.
pub async fn get_asset(plugin_id: String, name: String, client: ClientHandle) -> Result<()> {
    let bytes = crate::drivers::plugins::read_asset(&plugin_id, &name)?;
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    client.send_json(&json!({
        "type": "plugin_asset",
        "plugin_id": plugin_id,
        "name": name,
        "data_b64": data_b64,
    }));
    Ok(())
}

/// Replace a plugin's user-editable config values and persist the choice.
/// Staged — see the module docs.
pub async fn set_config(
    id: String,
    values: std::collections::HashMap<String, String>,
    app: Arc<AppState>,
) -> Result<()> {
    persist_config_values(&id, &values, &app).await?;
    app.request_config_save();
    mark_pending_and_broadcast(&app).await;
    Ok(())
}

/// Recursively copy `src` into `dst` (both directories), creating `dst`.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .with_context(|| format!("creating plugin dir {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Install a plugin package (a directory containing `plugin.yaml` + its entry
/// script) as a new plugin directory: validated in place first, then copied
/// in and re-parsed from its final location. Staged — see the module docs.
pub async fn import(source_dir: String, app: Arc<AppState>) -> Result<()> {
    let src = Path::new(&source_dir);
    let parsed = crate::drivers::plugins::parse_manifest_from_dir(src)
        .context("plugin package is not a valid manifest")?;
    let id = parsed.plugin_id.clone();

    let dst = crate::config::plugins_dir().join(&id);
    if dst.exists() {
        bail!("a plugin '{id}' is already installed");
    }
    copy_dir_all(src, &dst)?;
    log::info!(
        "Imported plugin package {} into {}",
        src.display(),
        dst.display()
    );

    let manifest = crate::drivers::plugins::parse_manifest_from_dir(&dst)
        .context("re-parsing imported plugin directory")?;

    // A manual import gets the GUI's blocking consent modal instead of the
    // auto-discovery toast — suppress it before the reload below would
    // otherwise fire one for this exact plugin.
    crate::drivers::plugins::suppress_permission_notice(&manifest.plugin_id);

    reload_registry(&app).await;
    mark_pending_and_broadcast(&app).await;
    Ok(())
}

/// Delete a user plugin directory by id. A repo-sourced plugin has no
/// standalone directory to delete on its own — remove its repo instead —
/// so this refuses anything but a `Local` plugin. Staged — see the module docs.
pub async fn delete(id: String, app: Arc<AppState>) -> Result<()> {
    let is_local = crate::drivers::plugins::list(&*app.secret_store)
        .into_iter()
        .find(|p| p.id == id)
        .map(|p| matches!(p.source, halod_shared::types::PluginSource::Local))
        .unwrap_or(true);
    if !is_local {
        bail!("plugin '{id}' is provided by a repository — remove the repository instead");
    }
    let dir = crate::config::plugins_dir().join(&id);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => log::info!("Deleted plugin {}", dir.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::warn!("Plugin {} already gone", dir.display());
        }
        Err(e) => return Err(e).with_context(|| format!("deleting {}", dir.display())),
    }

    if purge_plugin_state(&id, &app).await {
        app.request_config_save();
    }

    reload_registry(&app).await;
    mark_pending_and_broadcast(&app).await;
    Ok(())
}

/// Purge one plugin id's secret, disabled flag, and plaintext config; returns whether config changed. Shared by [`delete`] and `repos::remove_repo`.
pub(crate) async fn purge_plugin_state(id: &str, app: &Arc<AppState>) -> bool {
    for key in crate::drivers::plugins::secure_config_keys_for(id) {
        if let Err(e) = app.secret_store.delete(id, &key) {
            log::warn!("deleting secret '{key}' for plugin '{id}': {e:#}");
        }
    }

    let mut cfg = app.config.write().await;
    let before = cfg.plugins_disabled.len();
    cfg.plugins_disabled.retain(|x| x != id);
    let disabled_changed = cfg.plugins_disabled.len() != before;
    let config_changed = cfg.plugin_config.remove(id).is_some();
    if disabled_changed {
        crate::drivers::plugins::set_disabled(&cfg.plugins_disabled);
    }
    if config_changed {
        crate::drivers::plugins::set_config_values(&cfg.plugin_config);
    }
    disabled_changed || config_changed
}

pub async fn apply_pending_changes(app: Arc<AppState>) -> Result<()> {
    app.plugins_rediscover_pending
        .store(false, Ordering::Relaxed);
    rediscover_devices(app).await;
    Ok(())
}

/// Re-read the plugins directory and every configured git-repo source, and re-apply the disabled/granted sets. Shared with `repos.rs`.
pub(crate) async fn reload_registry(app: &Arc<AppState>) {
    let cfg = app.config.read().await;
    crate::drivers::plugins::load_all_with_repos(
        &crate::config::plugins_dir(),
        &crate::drivers::plugins::repo_plugin_dirs(&cfg.plugin_repos),
    );
    crate::drivers::plugins::set_disabled(&cfg.plugins_disabled);
    crate::drivers::plugins::set_granted(&cfg.plugin_permissions);
    crate::drivers::plugins::set_acknowledged(&cfg.plugin_acknowledged);
    crate::drivers::plugins::set_config_values(&cfg.plugin_config);
    crate::drivers::plugins::set_integrations_disabled(&cfg.integrations_disabled);
    crate::drivers::plugins::set_secret_store(app.secret_store.clone());
}

/// Flag a rediscovery as needed and push the updated plugin listing so the GUI shows it immediately. Shared with `repos.rs`.
pub(crate) async fn mark_pending_and_broadcast(app: &Arc<AppState>) {
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

/// Sanitize a file name or repo URL into a safe plugin id / directory name (lower-cased `[a-z0-9-]`, path-traversal-proof).
pub(crate) fn sanitize_slug(filename: &str) -> String {
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
    if slug.is_empty() {
        "plugin".to_owned()
    } else {
        slug.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_slugs_a_file_name() {
        assert_eq!(sanitize_slug("My Driver.lua"), "my-driver");
        assert_eq!(sanitize_slug("wled_udp"), "wled-udp");
        assert_eq!(sanitize_slug("a  b--c.lua"), "a-b-c");
    }

    #[test]
    fn sanitize_strips_path_traversal_and_handles_empty() {
        // A path separator can never survive into the written directory name.
        assert!(!sanitize_slug("../../etc/passwd").contains('/'));
        assert_eq!(sanitize_slug("///"), "plugin");
        assert_eq!(sanitize_slug(""), "plugin");
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

    const CONFIG_TEST_PLUGIN: &str = r#"
        return {
          config = { fields = {
            { key = "host", label = "Host" },
            { key = "token", label = "Token", secure = true },
          } },
        }
    "#;

    /// `devices` must be declared here — a directory plugin's own Lua manifest fields are overlaid away.
    const CONFIG_TEST_PLUGIN_YAML: &str =
        "id: cfgtest\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n";

    fn write_config_test_plugin(root: &std::path::Path) {
        let dir = root.join("cfgtest");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.yaml"), CONFIG_TEST_PLUGIN_YAML).unwrap();
        std::fs::write(dir.join("main.lua"), CONFIG_TEST_PLUGIN).unwrap();
    }

    /// Loads `CONFIG_TEST_PLUGIN` into the (process-wide) plugin registry for
    /// the duration of `f`, then restores the registry to just the built-ins.
    /// Callers must already hold `TEST_GLOBALS_LOCK`.
    async fn with_config_test_plugin<F, Fut>(f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let dir = tempfile::tempdir().unwrap();
        write_config_test_plugin(dir.path());
        crate::drivers::plugins::load_all(dir.path());
        f().await;
        crate::drivers::plugins::load_all(std::path::Path::new("/nonexistent"));
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
    #[allow(clippy::await_holding_lock)]
    async fn get_asset_replies_with_base64_bytes() {
        let _guard = crate::drivers::plugins::TEST_GLOBALS_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("assetplug");
        std::fs::create_dir_all(plugin_dir.join("assets")).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.yaml"),
            "id: assetplug\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n\
             logo: logo.png\n",
        )
        .unwrap();
        std::fs::write(plugin_dir.join("main.lua"), "return {}").unwrap();
        std::fs::write(plugin_dir.join("assets/logo.png"), b"PNGDATA").unwrap();
        crate::drivers::plugins::load_all(dir.path());

        let (client, mut rx) = test_client();
        get_asset("assetplug".into(), "logo.png".into(), client)
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
        assert_eq!(decoded, b"PNGDATA");

        crate::drivers::plugins::load_all(std::path::Path::new("/nonexistent"));
    }

    #[tokio::test]
    async fn get_asset_errors_for_unknown_plugin() {
        let (client, _rx) = test_client();
        let err = get_asset("does-not-exist".into(), "logo.png".into(), client)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown plugin"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn set_permissions_records_the_acknowledged_content_hash() {
        let _guard = crate::drivers::plugins::TEST_GLOBALS_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::test_support::with_tmp_config(|app| async move {
            with_config_test_plugin(|| async {
                set_permissions("cfgtest".into(), vec![Permission::Network], app.clone())
                    .await
                    .unwrap();
                let expected = crate::drivers::plugins::content_hash_for("cfgtest");
                assert!(expected.is_some());
                let cfg = app.config.read().await;
                assert_eq!(cfg.plugin_acknowledged.get("cfgtest"), expected.as_ref());
                assert_eq!(
                    cfg.plugin_permissions.get("cfgtest"),
                    Some(&vec![Permission::Network])
                );
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn revoking_clears_both_the_grant_and_the_content_pin() {
        let _guard = crate::drivers::plugins::TEST_GLOBALS_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::test_support::with_tmp_config(|app| async move {
            with_config_test_plugin(|| async {
                set_permissions("cfgtest".into(), vec![Permission::Network], app.clone())
                    .await
                    .unwrap();
                assert!(app
                    .config
                    .read()
                    .await
                    .plugin_acknowledged
                    .contains_key("cfgtest"));

                // Empty grant = revoke: both the grant and its pin are dropped.
                set_permissions("cfgtest".into(), vec![], app.clone())
                    .await
                    .unwrap();
                let cfg = app.config.read().await;
                assert!(!cfg.plugin_permissions.contains_key("cfgtest"));
                assert!(!cfg.plugin_acknowledged.contains_key("cfgtest"));
            })
            .await;
        })
        .await;
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
            write_config_test_plugin(&dir);
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
