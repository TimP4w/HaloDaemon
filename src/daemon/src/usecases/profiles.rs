use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::Profile;
use crate::engines::focus_watcher::ControlMsg;
use crate::state::AppState;
use halod_protocol::types::DEFAULT_PROFILE_NAME;

/// Applies the active profile's stored device states to all currently-connected devices.
/// Called on startup after discovery and after every profile switch.
pub async fn load_active_profile(app: Arc<AppState>) {
    let (active_name, states, known_devices, sensor_visibility) = {
        let cfg = app.config.read().await;
        (
            cfg.active_profile.clone(),
            cfg.active_profile_data().device_states.clone(),
            cfg.known_devices.clone(),
            cfg.sensor_visibility.clone(),
        )
    };

    // Remove all devices from the LCD engine before restoring state. The engine
    // holds device_slots across the full tick (including stream_frame), so this
    // call blocks until any in-progress frame is finished. Without this, a tick
    // that started just before the profile switch can stream a frame after
    // restore_state calls reset_to_default or upload_gif.
    //
    // Clear lcd_template_id first: the engine tick re-derives device_templates
    // from lcd_template_id() each tick, so if we remove_device without clearing
    // it first, the very next tick fires, sees the old template still set, and
    // re-inserts the slot (causing a brief engine re-run visible as a frame-
    // counter reset before load_state catches up).
    if let Some(engines) = app.engines.get() {
        let devices = app.devices.lock().await.clone();
        for device in &devices {
            if let Some(lcd) = device.as_lcd() {
                lcd.set_lcd_template_id(None);
            }
            engines.lcd.remove_device(&device.id()).await;
        }
    }

    let devices = app.devices.lock().await.clone();
    log::info!("[Profile] Loading '{}' onto {} device(s)", active_name, devices.len());
    for device in &devices {
        if let Some(state) = states.get(&device.id()) {
            log::debug!("[Profile] Restoring state for '{}'", device.name());
            device.load_state(state).await;
        }
        if let Some(record) = known_devices.get(&device.id()) {
            device.set_active_state(record.active_state.clone());
        }
        for (sensor_id, vis_state) in &sensor_visibility {
            device.set_sensor_visibility(sensor_id, vis_state.clone());
        }
    }
}

fn save_config(cfg: &crate::config::Config, app: &crate::state::AppState) {
    #[cfg(not(test))]
    app.request_config_save(cfg.clone());
    #[cfg(test)]
    let _ = (cfg, app);
}

/// Switch profile from the focus watcher without saving to disk.
pub async fn switch_profile_direct(name: String, app: Arc<AppState>) {
    {
        let mut cfg = app.config.write().await;
        if !cfg.profiles.contains_key(&name) {
            log::warn!("[FocusWatcher] Profile '{name}' not found, skipping switch");
            return;
        }
        cfg.active_profile = name;
    }
    load_active_profile(app.clone()).await;
    crate::ipc::broadcast_state(app).await;
}

pub async fn switch_profile(msg: Value, app: Arc<AppState>) -> Result<()> {
    let name = msg["name"]
        .as_str()
        .ok_or_else(|| anyhow!("missing name"))?
        .to_string();

    let snap = {
        let mut cfg = app.config.write().await;
        if !cfg.profiles.contains_key(&name) {
            anyhow::bail!("profile not found: {name}");
        }
        log::info!("[Profile] Switching to '{name}'");
        cfg.active_profile = name;
        cfg.clone()
    };
    save_config(&snap, &app);
    load_active_profile(app.clone()).await;
    crate::ipc::broadcast_state(app.clone()).await;
    {
        let guard = app.focus_watcher_tx.lock().await;
        if let Some(tx) = &*guard {
            let _ = tx.try_send(ControlMsg::ManualSwitch { profile: snap.active_profile.clone() });
        }
    }
    Ok(())
}

pub async fn add_profile(msg: Value, app: Arc<AppState>) -> Result<()> {
    let name = msg["name"]
        .as_str()
        .ok_or_else(|| anyhow!("missing name"))?
        .to_string();
    if name.is_empty() {
        anyhow::bail!("profile name must not be empty");
    }

    let snap = {
        let mut cfg = app.config.write().await;
        if cfg.profiles.contains_key(&name) {
            anyhow::bail!("profile already exists: {name}");
        }
        log::info!("[Profile] Adding '{name}'");
        // Seed fan curves from the default profile's device_states so new profiles inherit the baseline.
        let mut inherited_states: HashMap<String, serde_json::Value> = HashMap::new();
        for (device_id, state) in cfg
            .profiles
            .get(DEFAULT_PROFILE_NAME)
            .map(|p| &p.device_states)
            .into_iter()
            .flat_map(|m| m.iter())
        {
            if let Some(curve) = state.get("fan_curve").filter(|v| !v.is_null()) {
                inherited_states
                    .insert(device_id.clone(), serde_json::json!({ "fan_curve": curve }));
            }
        }
        let new_profile = Profile {
            device_states: inherited_states,
            ..Profile::default()
        };
        cfg.profiles.insert(name.clone(), new_profile);
        cfg.active_profile = name;
        cfg.clone()
    };
    save_config(&snap, &app);
    load_active_profile(app.clone()).await;
    crate::ipc::broadcast_state(app).await;
    Ok(())
}

pub async fn remove_profile(msg: Value, app: Arc<AppState>) -> Result<()> {
    let name = msg["name"]
        .as_str()
        .ok_or_else(|| anyhow!("missing name"))?
        .to_string();
    if name == DEFAULT_PROFILE_NAME {
        anyhow::bail!("cannot remove the default profile");
    }

    let snap = {
        let mut cfg = app.config.write().await;
        if !cfg.profiles.contains_key(&name) {
            anyhow::bail!("profile not found: {name}");
        }
        log::info!("[Profile] Removing '{name}'");
        cfg.profiles.remove(&name);
        if cfg.active_profile == name {
            cfg.active_profile = DEFAULT_PROFILE_NAME.to_string();
        }
        cfg.clone()
    };
    save_config(&snap, &app);
    load_active_profile(app.clone()).await;
    crate::ipc::broadcast_state(app).await;
    Ok(())
}

pub async fn rename_profile(msg: Value, app: Arc<AppState>) -> Result<()> {
    let old_name = msg["old_name"]
        .as_str()
        .ok_or_else(|| anyhow!("missing old_name"))?
        .to_string();
    let new_name = msg["new_name"]
        .as_str()
        .ok_or_else(|| anyhow!("missing new_name"))?
        .to_string();

    if old_name == DEFAULT_PROFILE_NAME {
        anyhow::bail!("cannot rename the default profile");
    }
    if new_name.is_empty() {
        anyhow::bail!("profile name must not be empty");
    }

    let snap = {
        let mut cfg = app.config.write().await;
        if !cfg.profiles.contains_key(&old_name) {
            anyhow::bail!("profile not found: {old_name}");
        }
        if cfg.profiles.contains_key(&new_name) {
            anyhow::bail!("profile already exists: {new_name}");
        }
        log::info!("[Profile] Renaming '{old_name}' → '{new_name}'");
        let profile = cfg.profiles.remove(&old_name).unwrap();
        cfg.profiles.insert(new_name.clone(), profile);
        if cfg.active_profile == old_name {
            cfg.active_profile = new_name;
        }
        cfg.clone()
    };
    save_config(&snap, &app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    #[tokio::test]
    async fn switch_profile_fails_on_unknown() {
        let app = make_app();
        let err = switch_profile(json!({"name": "ghost"}), app).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn add_profile_creates_and_activates() {
        let app = make_app();
        add_profile(json!({"name": "Gaming"}), app.clone()).await.unwrap();
        let cfg = app.config.read().await;
        assert!(cfg.profiles.contains_key("Gaming"));
        assert_eq!(cfg.active_profile, "Gaming");
    }

    #[tokio::test]
    async fn add_profile_rejects_duplicate() {
        let app = make_app();
        add_profile(json!({"name": "Gaming"}), app.clone()).await.unwrap();
        let err = add_profile(json!({"name": "Gaming"}), app).await.unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn add_profile_rejects_empty_name() {
        let app = make_app();
        let err = add_profile(json!({"name": ""}), app).await.unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn remove_profile_rejects_default() {
        let app = make_app();
        let err = remove_profile(json!({"name": "default"}), app).await.unwrap_err();
        assert!(err.to_string().contains("default"));
    }

    #[tokio::test]
    async fn remove_profile_switches_to_default_when_active() {
        let app = make_app();
        add_profile(json!({"name": "Gaming"}), app.clone()).await.unwrap();
        remove_profile(json!({"name": "Gaming"}), app.clone()).await.unwrap();
        let cfg = app.config.read().await;
        assert!(!cfg.profiles.contains_key("Gaming"));
        assert_eq!(cfg.active_profile, DEFAULT_PROFILE_NAME);
    }

    #[tokio::test]
    async fn rename_profile_rejects_default() {
        let app = make_app();
        let err = rename_profile(json!({"old_name": "default", "new_name": "x"}), app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("default"));
    }

    #[tokio::test]
    async fn rename_profile_updates_active() {
        let app = make_app();
        add_profile(json!({"name": "Gaming"}), app.clone()).await.unwrap();
        rename_profile(json!({"old_name": "Gaming", "new_name": "Performance"}), app.clone())
            .await
            .unwrap();
        let cfg = app.config.read().await;
        assert!(cfg.profiles.contains_key("Performance"));
        assert!(!cfg.profiles.contains_key("Gaming"));
        assert_eq!(cfg.active_profile, "Performance");
    }

    #[tokio::test]
    async fn switch_profile_changes_active() {
        let app = make_app();
        add_profile(json!({"name": "Silent"}), app.clone()).await.unwrap();
        switch_profile(json!({"name": "default"}), app.clone()).await.unwrap();
        let cfg = app.config.read().await;
        assert_eq!(cfg.active_profile, DEFAULT_PROFILE_NAME);
    }

    #[tokio::test]
    async fn switch_profile_direct_changes_active_without_saving() {
        let app = make_app();
        add_profile(json!({"name": "Gaming"}), app.clone()).await.unwrap();
        // Confirm it starts on "Gaming" after add_profile
        assert_eq!(app.config.read().await.active_profile, "Gaming");
        // Switch back to default via direct
        crate::usecases::profiles::switch_profile_direct("default".into(), app.clone()).await;
        let cfg = app.config.read().await;
        assert_eq!(cfg.active_profile, DEFAULT_PROFILE_NAME);
    }

    #[tokio::test]
    async fn switch_profile_direct_skips_unknown_profile() {
        let app = make_app();
        // active_profile starts as "default"
        switch_profile_direct("nonexistent".into(), app.clone()).await;
        // Must stay on default, no panic
        assert_eq!(app.config.read().await.active_profile, DEFAULT_PROFILE_NAME);
    }

    #[tokio::test]
    async fn add_profile_inherits_fan_curves_from_default() {
        use crate::config::FanCurveRecord;
        let app = make_app();
        {
            let mut cfg = app.config.write().await;
            let fan_record = FanCurveRecord {
                sensor_id: Some("sensor_0".to_string()),
                points: vec![(30.0, 20.0), (80.0, 100.0)],
            };
            cfg.profiles
                .get_mut(DEFAULT_PROFILE_NAME)
                .unwrap()
                .device_states
                .insert(
                    "fan_0".to_string(),
                    serde_json::json!({ "fan_curve": fan_record }),
                );
        }
        add_profile(json!({"name": "Gaming"}), app.clone()).await.unwrap();
        let cfg = app.config.read().await;
        let gaming = cfg.profiles.get("Gaming").unwrap();
        let fan_curve_entry = gaming
            .device_states
            .get("fan_0")
            .and_then(|s| s.get("fan_curve"));
        assert!(
            fan_curve_entry.is_some(),
            "Gaming profile should inherit fan_curve from default"
        );
    }
}
