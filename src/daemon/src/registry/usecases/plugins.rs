// SPDX-License-Identifier: GPL-3.0-or-later
//! Managing device plugins: enable/disable, import, and delete.

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::state::AppState;

/// Enable or disable a plugin, persist the choice, and re-run discovery so the
/// change applies to currently-connected hardware: a disabled plugin releases
/// its device to the native driver; an enabled plugin shadows native again.
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
    rediscover_devices(app).await;
    Ok(())
}

/// Install a Lua plugin script into the plugins directory, then re-run
/// discovery. The source is validated as a manifest first, so a malformed
/// script is rejected before any file is written.
pub async fn import(filename: String, source: String, app: Arc<AppState>) -> Result<()> {
    let name = sanitize_lua_filename(&filename);
    crate::drivers::plugins::parse_manifest(&source, Path::new(&name))
        .context("plugin script is not a valid manifest")?;

    let dir = crate::config::plugins_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating plugins dir {}", dir.display()))?;
    let path = dir.join(&name);
    std::fs::write(&path, source).with_context(|| format!("writing plugin {}", path.display()))?;
    log::info!("Imported plugin {}", path.display());

    reload_and_rediscover(app).await;
    Ok(())
}

/// Delete a user plugin script by id, then re-run discovery. Built-in plugins
/// have no on-disk script and are refused.
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

    reload_and_rediscover(app).await;
    Ok(())
}

/// Re-read the plugins directory (picking up added/removed scripts), re-apply
/// the disabled set, then run a clean-slate re-discovery.
async fn reload_and_rediscover(app: Arc<AppState>) {
    crate::drivers::plugins::load_all(&crate::config::plugins_dir());
    crate::drivers::plugins::set_disabled(&app.config.read().await.plugins_disabled);
    rediscover_devices(app).await;
}

/// Clean-slate re-discovery: close every device and re-run the startup path,
/// then broadcast. Closing everything is the only way to correctly hand
/// hardware between a plugin and its native driver without tracking per-device
/// discovery handles.
async fn rediscover_devices(app: Arc<AppState>) {
    let previous = {
        let mut devices = app.devices.write().await;
        std::mem::take(&mut *devices)
    };
    for device in &previous {
        device.close().await;
    }
    drop(previous);

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
}
