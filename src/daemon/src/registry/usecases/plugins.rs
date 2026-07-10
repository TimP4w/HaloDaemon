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

    // Drop it from the disabled set too, so a later re-import starts enabled.
    let changed = {
        let mut cfg = app.config.write().await;
        let before = cfg.plugins_disabled.len();
        cfg.plugins_disabled.retain(|x| x != &id);
        let changed = cfg.plugins_disabled.len() != before;
        if changed {
            crate::drivers::plugins::set_disabled(&cfg.plugins_disabled);
        }
        changed
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
}
