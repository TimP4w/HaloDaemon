// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::Result;
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::profiles::device_state::persist_device_state;
use crate::domain::profiles::model::Profile;
use crate::domain::profiles::observers::active_window::ControlMsg;
use halod_shared::types::DEFAULT_PROFILE_NAME;

/// Applies the active profile's stored device states to all currently-connected devices.
/// Called on startup after discovery and after every profile switch.
pub async fn load_active_profile(app: Arc<AppState>) {
    let (active_name, known_devices) = {
        let cfg = app.config.read().await;
        (cfg.active_profile.clone(), cfg.known_devices.clone())
    };

    // Remove devices from the LCD engine before restoring state, so an in-flight
    // tick can't stream a frame after load_state resets the device. Clear
    // lcd_template_id first, else the next tick re-derives it and re-adds the
    // slot before remove_device takes effect.
    let devices = app.device_registry.read().await.clone();
    if let Some(lcd_engine) = app.lcd.engine() {
        for device in &devices {
            if let Some(lcd) = device.as_lcd() {
                lcd.set_lcd_template_id(None);
            }
            lcd_engine.remove_device(device.id()).await;
        }
    }
    log::info!(
        "[Profile] Loading '{}' onto {} device(s)",
        active_name,
        devices.len()
    );
    for device in &devices {
        let effective = {
            let cfg = app.config.read().await;
            cfg.effective_device_state(device.id())
        };
        if !effective.is_null() {
            log::debug!(
                "[Profile] Restoring effective state for '{}'",
                device.name()
            );
            device.load_state(&effective).await;
        }
        if let Some(record) = known_devices.get(device.id()) {
            device.set_active_state(record.active_state.clone());
        }
    }

    // Re-activate restored LCD templates: the slots were removed above and the
    // engine loop may be parked idle, so a restored lcd_template_id alone won't
    // render until something wakes it.
    if let Some(lcd_engine) = app.lcd.engine() {
        for device in &devices {
            let Some(lcd) = device.as_lcd() else { continue };
            let Some(template_id) = lcd.lcd_template_id() else {
                continue;
            };
            lcd_engine
                .set_template_active(device.id(), &template_id, &lcd.lcd_template_params())
                .await;
        }
    }

    // Re-start restored LCD video playback: like templates, the panel was
    // cleared above and won't replay on its own. If the saved file no longer
    // exists, warn once, drop the stale path, and fall back to Default so the
    // device stops advertising a video it can't play.
    if let Some(video) = app.lcd.video() {
        for device in &devices {
            let Some(lcd) = device.as_lcd() else { continue };
            let Some(path) = lcd.video_path() else {
                continue;
            };
            if std::path::Path::new(&path).is_file() {
                if let Err(e) = video.start(device.id(), &path).await {
                    log::warn!(
                        "[Profile] Failed to replay LCD video for '{}': {e:#}",
                        device.name()
                    );
                }
            } else {
                log::warn!(
                    "[Profile] Saved LCD video '{path}' for '{}' no longer exists; clearing",
                    device.name()
                );
                lcd.set_video_path(None);
                lcd.lcd_state()
                    .set_mode(halod_shared::types::LcdMode::Default);
                persist_device_state(&app, device.as_ref()).await;
            }
        }
    }
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
    app.record_change(crate::application::bus::coordinator::Change::ProfileSwitch)
        .await;
    let profile = app.config.read().await.active_profile.clone();
    crate::infrastructure::platform::notify::send(
        &app,
        halod_shared::types::NotificationCode::ProfileSwitched { profile },
    )
    .await;
}

pub async fn switch_profile(name: String, app: Arc<AppState>) -> Result<()> {
    let snap = {
        let mut cfg = app.config.write().await;
        if !cfg.profiles.contains_key(&name) {
            anyhow::bail!("profile not found: {name}");
        }
        log::info!("[Profile] Switching to '{name}'");
        cfg.active_profile = name;
        cfg.clone()
    };
    app.request_config_save();
    load_active_profile(app.clone()).await;
    app.record_change(crate::application::bus::coordinator::Change::ProfileSwitch)
        .await;
    crate::infrastructure::platform::notify::send(
        &app,
        halod_shared::types::NotificationCode::ProfileSwitched {
            profile: snap.active_profile.clone(),
        },
    )
    .await;
    app.focus
        .notify(ControlMsg::ManualSwitch {
            profile: snap.active_profile.clone(),
        })
        .await;
    Ok(())
}

pub async fn add_profile(name: String, app: Arc<AppState>) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("profile name must not be empty");
    }

    {
        let mut cfg = app.config.write().await;
        if cfg.profiles.contains_key(&name) {
            anyhow::bail!("profile already exists: {name}");
        }
        log::info!("[Profile] Adding '{name}'");
        cfg.profiles.insert(name.clone(), Profile::default());
        cfg.active_profile = name.clone();
    }
    app.request_config_save();
    load_active_profile(app.clone()).await;
    app.focus
        .notify(ControlMsg::ManualSwitch { profile: name })
        .await;
    app.record_change(crate::application::bus::coordinator::Change::ProfileSwitch)
        .await;
    Ok(())
}

pub async fn remove_profile(name: String, app: Arc<AppState>) -> Result<()> {
    if name == DEFAULT_PROFILE_NAME {
        anyhow::bail!("cannot remove the default profile");
    }

    let active_removed = {
        let mut cfg = app.config.write().await;
        if !cfg.profiles.contains_key(&name) {
            anyhow::bail!("profile not found: {name}");
        }
        log::info!("[Profile] Removing '{name}'");
        cfg.profiles.remove(&name);
        let active_removed = cfg.active_profile == name;
        if active_removed {
            cfg.active_profile = DEFAULT_PROFILE_NAME.to_string();
        }
        active_removed
    };
    app.request_config_save();
    if active_removed {
        load_active_profile(app.clone()).await;
        app.focus
            .notify(ControlMsg::ManualSwitch {
                profile: DEFAULT_PROFILE_NAME.to_string(),
            })
            .await;
        app.record_change(crate::application::bus::coordinator::Change::ProfileSwitch)
            .await;
    } else {
        app.record_change(crate::application::bus::coordinator::Change::Profiles)
            .await;
    }
    Ok(())
}

pub async fn rename_profile(old_name: String, new_name: String, app: Arc<AppState>) -> Result<()> {
    if old_name == DEFAULT_PROFILE_NAME {
        anyhow::bail!("cannot rename the default profile");
    }
    if new_name.is_empty() {
        anyhow::bail!("profile name must not be empty");
    }

    let active_renamed = {
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
        let active_renamed = cfg.active_profile == old_name;
        if active_renamed {
            cfg.active_profile = new_name.clone();
        }
        active_renamed
    };
    app.request_config_save();
    if active_renamed {
        app.focus
            .notify(ControlMsg::ManualSwitch { profile: new_name })
            .await;
    }
    app.record_change(crate::application::bus::coordinator::Change::Profiles)
        .await;
    Ok(())
}

/// Persist the RGB Lighting view's device/zone selection in the active profile.
pub async fn set_lighting_targets(
    device_ids: Vec<String>,
    channels: std::collections::HashMap<String, Vec<String>>,
    app: Arc<AppState>,
) -> Result<()> {
    let cap = halod_shared::types::MAX_LIGHTING_TARGET_IDS;
    anyhow::ensure!(device_ids.len() <= cap, "too many lighting target devices");
    anyhow::ensure!(channels.len() <= cap, "too many lighting target channels");
    anyhow::ensure!(
        channels.values().all(|v| v.len() <= cap),
        "too many channels for a device"
    );
    app.config
        .write()
        .await
        .active_profile_data_mut()
        .lighting
        .targets = halod_shared::types::LightingTargets {
        device_ids,
        channels,
    };
    app.request_config_save();
    app.record_change(crate::application::bus::coordinator::Change::Lighting)
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    #[tokio::test]
    async fn set_lighting_targets_saves_into_active_profile_only() {
        let app = make_app();
        add_profile("Gaming".into(), app.clone()).await.unwrap();
        set_lighting_targets(
            vec!["dev1".into()],
            std::collections::HashMap::from([("dev1".to_string(), vec!["z0".to_string()])]),
            app.clone(),
        )
        .await
        .unwrap();

        let cfg = app.config.read().await;
        let gaming = &cfg.profiles["Gaming"].lighting.targets;
        assert_eq!(gaming.device_ids, vec!["dev1"]);
        assert_eq!(gaming.channels["dev1"], vec!["z0"]);
        assert!(cfg.profiles[DEFAULT_PROFILE_NAME]
            .lighting
            .targets
            .device_ids
            .is_empty());
    }

    #[tokio::test]
    async fn switch_profile_fails_on_unknown() {
        let app = make_app();
        let err = switch_profile("ghost".into(), app).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn add_profile_creates_and_activates() {
        let app = make_app();
        add_profile("Gaming".into(), app.clone()).await.unwrap();
        let cfg = app.config.read().await;
        assert!(cfg.profiles.contains_key("Gaming"));
        assert_eq!(cfg.active_profile, "Gaming");
    }

    #[tokio::test]
    async fn add_profile_rejects_duplicate() {
        let app = make_app();
        add_profile("Gaming".into(), app.clone()).await.unwrap();
        let err = add_profile("Gaming".into(), app).await.unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn add_profile_rejects_empty_name() {
        let app = make_app();
        let err = add_profile("".into(), app).await.unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn remove_profile_rejects_default() {
        let app = make_app();
        let err = remove_profile("default".into(), app).await.unwrap_err();
        assert!(err.to_string().contains("default"));
    }

    #[tokio::test]
    async fn remove_profile_switches_to_default_when_active() {
        let app = make_app();
        add_profile("Gaming".into(), app.clone()).await.unwrap();
        remove_profile("Gaming".into(), app.clone()).await.unwrap();
        let cfg = app.config.read().await;
        assert!(!cfg.profiles.contains_key("Gaming"));
        assert_eq!(cfg.active_profile, DEFAULT_PROFILE_NAME);
    }

    #[tokio::test]
    async fn rename_profile_rejects_default() {
        let app = make_app();
        let err = rename_profile("default".into(), "x".into(), app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("default"));
    }

    #[tokio::test]
    async fn rename_profile_updates_active() {
        let app = make_app();
        add_profile("Gaming".into(), app.clone()).await.unwrap();
        rename_profile("Gaming".into(), "Performance".into(), app.clone())
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
        add_profile("Silent".into(), app.clone()).await.unwrap();
        switch_profile("default".into(), app.clone()).await.unwrap();
        let cfg = app.config.read().await;
        assert_eq!(cfg.active_profile, DEFAULT_PROFILE_NAME);
    }

    #[tokio::test]
    async fn switch_profile_direct_changes_active_without_saving() {
        let app = make_app();
        add_profile("Gaming".into(), app.clone()).await.unwrap();
        assert_eq!(app.config.read().await.active_profile, "Gaming");
        crate::domain::profiles::usecases::profiles::switch_profile_direct(
            "default".into(),
            app.clone(),
        )
        .await;
        let cfg = app.config.read().await;
        assert_eq!(cfg.active_profile, DEFAULT_PROFILE_NAME);
        let replay = app.data_bus.replay_events(None);
        assert!(replay.events.iter().any(|event| matches!(
            &event.payload,
            halod_shared::bus::BusEventPayload::Notification(notification)
                if matches!(
                    &notification.code,
                    halod_shared::types::NotificationCode::ProfileSwitched { profile }
                        if profile == DEFAULT_PROFILE_NAME
                ) && notification.show_native
        )));
    }

    #[tokio::test]
    async fn switch_profile_direct_skips_unknown_profile() {
        let app = make_app();
        switch_profile_direct("nonexistent".into(), app.clone()).await;
        assert_eq!(app.config.read().await.active_profile, DEFAULT_PROFILE_NAME);
    }

    #[tokio::test]
    async fn load_reads_effective_merged_state() {
        let app = make_app();
        {
            let mut cfg = app.config.write().await;
            cfg.profiles
                .get_mut(DEFAULT_PROFILE_NAME)
                .unwrap()
                .device_states
                .insert(
                    "dev1".into(),
                    serde_json::json!({ "rgb": {"m": "static"}, "fan_curve": {"a": 1} }),
                );
            let mut gaming = Profile::default();
            gaming
                .device_states
                .insert("dev1".into(), serde_json::json!({ "fan_curve": {"a": 2} }));
            cfg.profiles.insert("Gaming".into(), gaming);
            cfg.active_profile = "Gaming".into();
        }
        let cfg = app.config.read().await;
        let eff = cfg.effective_device_state("dev1");
        assert_eq!(eff["rgb"]["m"], "static");
        assert_eq!(eff["fan_curve"]["a"], 2);
    }

    #[tokio::test]
    async fn load_active_profile_reactivates_saved_lcd_template() {
        use crate::domain::lcd::engine::LcdEngine;
        use crate::infrastructure::drivers::Device;
        use crate::test_support::MockDevice;

        let app = make_app();
        let dev = Arc::new(MockDevice::new("lcd0").with_lcd());
        app.device_registry
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = LcdEngine::new(app.clone());
        let (frame_tx, _) = tokio::sync::broadcast::channel(2);
        let video = crate::domain::lcd::engine::video::VideoEngine::new(app.clone(), frame_tx);
        app.lcd.set_engine(engine, video);

        // Saved state carries a template id, as `load_state` would restore it.
        app.config
            .write()
            .await
            .active_profile_data_mut()
            .device_states
            .insert(
                "lcd0".into(),
                serde_json::json!({ "lcd": { "template_id": "custom" } }),
            );

        load_active_profile(app.clone()).await;

        assert!(
            app.lcd.engine().unwrap().has_slot("lcd0").await,
            "engine must re-activate the restored template"
        );
    }

    #[tokio::test]
    async fn load_active_profile_template_survives_remove_and_readd_cycle() {
        use crate::domain::lcd::engine::LcdEngine;
        use crate::infrastructure::drivers::Device;
        use crate::test_support::MockDevice;

        let app = make_app();
        let dev = Arc::new(MockDevice::new("lcd0").with_lcd());
        app.device_registry
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = LcdEngine::new(app.clone());
        let (frame_tx, _) = tokio::sync::broadcast::channel(2);
        let video = crate::domain::lcd::engine::video::VideoEngine::new(app.clone(), frame_tx);
        app.lcd.set_engine(engine, video);

        app.config
            .write()
            .await
            .active_profile_data_mut()
            .device_states
            .insert(
                "lcd0".into(),
                serde_json::json!({ "lcd": { "template_id": "custom" } }),
            );

        // A pre-existing slot from before the (re)load, which the reload must
        // remove-then-recreate rather than leave stuck on a stale template.
        app.lcd
            .engine()
            .unwrap()
            .set_template_active("lcd0", "custom", &Default::default())
            .await;

        load_active_profile(app.clone()).await;

        assert!(
            app.lcd.engine().unwrap().has_slot("lcd0").await,
            "template must survive the remove+re-add cycle"
        );
        assert_eq!(
            dev.lcd.as_ref().unwrap().lcd_template_id(),
            Some("custom".to_string())
        );
    }

    #[tokio::test]
    async fn load_active_profile_clears_missing_lcd_video_path() {
        use crate::domain::lcd::engine::LcdEngine;
        use crate::infrastructure::drivers::Device;
        use crate::test_support::MockDevice;

        let app = make_app();
        let dev = Arc::new(MockDevice::new("lcd0").with_lcd());
        app.device_registry
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = LcdEngine::new(app.clone());
        let (frame_tx, _) = tokio::sync::broadcast::channel(2);
        let video = crate::domain::lcd::engine::video::VideoEngine::new(app.clone(), frame_tx);
        app.lcd.set_engine(engine, video);

        app.config
            .write()
            .await
            .active_profile_data_mut()
            .device_states
            .insert(
                "lcd0".into(),
                serde_json::json!({
                    "lcd": { "mode": "video", "video_path": "/nonexistent/gone.mp4" }
                }),
            );

        load_active_profile(app.clone()).await;

        let lcd = dev.lcd.as_ref().unwrap();
        assert_eq!(lcd.video_path(), None, "stale video path must be cleared");
        assert!(
            matches!(lcd.mode(), halod_shared::types::LcdMode::Default),
            "mode must fall back to Default when the video is gone"
        );
        let cfg = app.config.read().await;
        let saved = &cfg.active_profile_data().device_states["lcd0"]["lcd"];
        assert!(saved["video_path"].is_null(), "cleared path must persist");
    }

    #[tokio::test]
    async fn load_active_profile_keeps_existing_lcd_video_path() {
        use crate::domain::lcd::engine::LcdEngine;
        use crate::infrastructure::drivers::Device;
        use crate::test_support::MockDevice;

        let app = make_app();
        let dev = Arc::new(MockDevice::new("lcd0").with_lcd());
        app.device_registry
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = LcdEngine::new(app.clone());
        let (frame_tx, _) = tokio::sync::broadcast::channel(2);
        let video = crate::domain::lcd::engine::video::VideoEngine::new(app.clone(), frame_tx);
        app.lcd.set_engine(engine, video);

        // A real, existing file: playback may fail (ffmpeg absent in CI) but the
        // path must not be treated as stale and cleared.
        let path = std::env::current_exe().unwrap();
        let path_str = path.to_string_lossy().to_string();
        app.config
            .write()
            .await
            .active_profile_data_mut()
            .device_states
            .insert(
                "lcd0".into(),
                serde_json::json!({
                    "lcd": { "mode": "video", "video_path": path_str.clone() }
                }),
            );

        load_active_profile(app.clone()).await;

        assert_eq!(
            dev.lcd.as_ref().unwrap().video_path(),
            Some(path_str),
            "an existing video path must be retained for replay"
        );
    }

    #[tokio::test]
    async fn add_profile_creates_empty_profile() {
        let app = make_app();
        {
            let mut cfg = app.config.write().await;
            cfg.profiles
                .get_mut(DEFAULT_PROFILE_NAME)
                .unwrap()
                .device_states
                .insert("fan_0".into(), serde_json::json!({ "fan_curve": {"a": 1} }));
        }
        add_profile("Gaming".into(), app.clone()).await.unwrap();
        let cfg = app.config.read().await;
        let gaming = cfg.profiles.get("Gaming").unwrap();
        assert!(
            gaming.device_states.is_empty(),
            "new profile must start empty and inherit"
        );
        assert!(gaming.lighting.canvas.is_none());
    }
}
