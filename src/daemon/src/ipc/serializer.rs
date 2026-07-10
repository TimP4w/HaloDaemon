// SPDX-License-Identifier: GPL-3.0-or-later
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::state::AppState;
use halod_shared::types::AppState as WireAppState;

pub async fn serialize_state(
    app: &Arc<AppState>,
    cfg: crate::config::Config,
    process_icons: HashMap<String, String>,
) -> Value {
    let disc = app.discovery.lock().await.clone();
    let snap = app.snapshot_devices(&cfg).await;
    let cooling = app.cooling.snapshot(snap.fan_curves).await;
    let lighting = app.lighting.snapshot(&cfg, snap.placed_zones).await;
    let lcd = app
        .lcd
        .snapshot(snap.lcd_templates, snap.lcd_template_params)
        .await;

    let wire = WireAppState {
        discovery: disc,
        devices: snap.devices,
        active_profile: cfg.active_profile.clone(),
        profiles: cfg.profile_names(),
        cooling,
        lighting,
        lcd,
        global_config: cfg.global.clone(),
        log_entries: app.recent_logs(100),
        config_dir: crate::config::config_dir().display().to_string(),
        app_rules: cfg.app_rules.clone(),
        focus_watcher_supported: app.focus.supported(),
        ffmpeg_available: crate::lcd::engine::video::ffmpeg_available(),
        profile_overrides: cfg.profile_overrides(),
        process_icons,
        plugins: crate::drivers::plugins::list(app.secret_store.as_ref()),
        plugins_rediscover_pending: app
            .plugins_rediscover_pending
            .load(std::sync::atomic::Ordering::Relaxed),
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
        let cfg = app.config.read().await.clone();
        let value = serialize_state(&app, cfg, HashMap::new()).await;
        let wire: halod_shared::types::AppState = serde_json::from_value(value).unwrap();
        assert_eq!(wire.devices.len(), 0);
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
