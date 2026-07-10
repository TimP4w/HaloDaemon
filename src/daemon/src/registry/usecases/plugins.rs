// SPDX-License-Identifier: GPL-3.0-or-later
//! Enabling/disabling device plugins.

use std::sync::Arc;

use anyhow::Result;

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

    // Clean-slate re-discovery. Closing every device and re-running the startup
    // path is the only way to correctly hand hardware between a plugin and its
    // native driver without tracking per-device discovery handles.
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
    Ok(())
}
