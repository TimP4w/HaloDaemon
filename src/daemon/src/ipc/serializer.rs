// SPDX-License-Identifier: GPL-3.0-or-later
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::state::AppState;
use halod_shared::types::{
    AppState as WireAppState, HealthCheckState, PluginRepoInfo, PluginsState, ProfileState,
};

pub async fn serialize_state(
    app: &Arc<AppState>,
    cfg: crate::config::Config,
    process_icons: HashMap<String, String>,
) -> Value {
    let disc = app.discovery.lock().await.clone();
    let snap = app.snapshot_devices(&cfg).await;
    // The daemon persists each domain's config separately; the wire form nests
    // it under the matching State struct, so inject it here (the domain
    // snapshots stay config-free).
    let mut cooling = app.cooling.snapshot(snap.fan_curves).await;
    cooling.config = cfg.cooling.clone();
    let mut lighting = app
        .lighting
        .snapshot(&app.registry, &cfg, snap.placed_zones)
        .await;
    lighting.config = cfg.rgb.clone();
    let mut lcd = app
        .lcd
        .snapshot(snap.lcd_templates, snap.lcd_template_params)
        .await;
    lcd.config = cfg.lcd.clone();

    let wire = WireAppState {
        discovery: disc,
        devices: snap.devices,
        profiles: ProfileState {
            active: cfg.active_profile.clone(),
            available: cfg.profile_names(),
            app_rules: cfg.app_rules.clone(),
            overrides: cfg.profile_overrides(),
        },
        cooling,
        lighting,
        lcd,
        gui: cfg.gui.clone(),
        config_dir: crate::config::config_dir().display().to_string(),
        health: HealthCheckState {
            focus_watcher_supported: app.focus.supported(),
            ffmpeg_available: crate::lcd::engine::video::ffmpeg_available(),
        },
        process_icons,
        plugins: PluginsState {
            plugins: app.registry.list(app.secret_store.as_ref()),
            repos: cfg
                .plugins
                .repos
                .iter()
                .map(|r| PluginRepoInfo {
                    url: r.url.clone(),
                    slug: r.slug.clone(),
                    repository_id: r.repository_id.clone(),
                    branch: r.branch.clone(),
                    locked_sha: r.locked_sha.clone(),
                    active_revision: r.active_revision.clone(),
                    previous_verified_sha: r.previous_verified_sha.clone(),
                    last_sync: r.last_sync.clone(),
                    official: r.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG,
                })
                .collect(),
            skipped: app.registry.skipped(),
        },
    };
    match serde_json::to_value(wire) {
        Ok(v) => v,
        Err(e) => {
            log::error!("serialize_state failed (non-finite float in config?): {e}");
            serde_json::json!({"__serialize_error": true})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, drivers::Device, test_support::MockDevice};

    #[tokio::test]
    async fn serialize_empty_state() {
        let app = Arc::new(AppState::new(Config::default()));
        let mut cfg = app.config.read().await.clone();
        cfg.cooling.fan_failsafe_duty = 42;
        cfg.rgb.canvas_fps = 33;
        cfg.lcd.fps = 12;
        cfg.gui.language = "it".into();
        let value = serialize_state(&app, cfg, HashMap::new()).await;
        let wire: halod_shared::types::AppState = serde_json::from_value(value).unwrap();
        assert_eq!(wire.devices.len(), 0);
        // The per-domain config is injected into each nested State struct.
        assert_eq!(wire.cooling.config.fan_failsafe_duty, 42);
        assert_eq!(wire.lighting.config.canvas_fps, 33);
        assert_eq!(wire.lcd.config.fps, 12);
        assert_eq!(wire.gui.language, "it");
        assert_eq!(wire.profiles.active, "default");
    }

    #[tokio::test]
    async fn serialize_with_one_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev: Arc<dyn Device> = Arc::new(
            MockDevice::new("test_device")
                .with_name("Test Fan")
                .with_vendor("Acme")
                .with_model("Fan 3000")
                .with_fan()
                .with_rgb(),
        );
        app.devices.write().await.push(dev);
        let cfg = app.config.read().await.clone();
        let value = serialize_state(&app, cfg, HashMap::new()).await;
        let wire: halod_shared::types::AppState = serde_json::from_value(value).unwrap();
        assert_eq!(wire.devices.len(), 1);
        assert_eq!(wire.devices[0].id, "test_device");
        assert_eq!(wire.devices[0].name, "Test Fan");
        assert_eq!(wire.devices[0].vendor, "Acme");
    }
}
