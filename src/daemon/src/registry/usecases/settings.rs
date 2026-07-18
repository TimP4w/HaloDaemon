// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::Result;
use std::sync::Arc;

use crate::state::{AppState, EngineRunConfig};
use halod_shared::commands::EngineKind;

pub async fn set_engine_config(
    kind: EngineKind,
    enabled: Option<bool>,
    tick_ms: Option<u64>,
    fps: Option<u64>,
    failsafe_duty: Option<u8>,
    app: Arc<AppState>,
) -> Result<()> {
    let failsafe_duty = failsafe_duty.map(|d| d.min(100));

    {
        let mut cfg = app.config.write().await;
        match kind {
            EngineKind::FanCurve => {
                if let Some(v) = enabled {
                    cfg.cooling.fan_curve_enabled = v;
                }
                if let Some(ms) = tick_ms {
                    cfg.cooling.fan_curve_tick_ms = ms.clamp(500, 60_000);
                }
                if let Some(d) = failsafe_duty {
                    cfg.cooling.fan_failsafe_duty = d;
                }
            }
            EngineKind::Canvas => {
                if let Some(v) = enabled {
                    cfg.rgb.canvas_enabled = v;
                }
                if let Some(fps) = fps {
                    cfg.rgb.canvas_fps = fps.clamp(1, 60) as u32;
                }
            }
            EngineKind::Lcd => {
                if let Some(v) = enabled {
                    cfg.lcd.enabled = v;
                }
                if let Some(fps) = fps {
                    cfg.lcd.fps = fps.clamp(1, 60) as u32;
                }
            }
        }
    }

    app.request_config_save();

    // Signal the affected engine to re-read config.
    match kind {
        EngineKind::FanCurve => {
            let cooling = app.config.read().await.cooling.clone();
            if let Some(tx) = app.cooling.cfg_tx() {
                let _ = tx.send(EngineRunConfig::fan_curve(&cooling));
            }
            if failsafe_duty.is_some() {
                if let Some(tx) = app.cooling.failsafe_duty_tx() {
                    let _ = tx.send(cooling.fan_failsafe_duty);
                }
            }
        }
        EngineKind::Canvas => {
            let rgb = app.config.read().await.rgb.clone();
            if let Some(tx) = app.lighting.cfg_tx() {
                let _ = tx.send(EngineRunConfig::canvas(&rgb));
            }
        }
        EngineKind::Lcd => {
            let lcd = app.config.read().await.lcd.clone();
            if let Some(tx) = app.lcd.cfg_tx() {
                let _ = tx.send(EngineRunConfig::lcd(&lcd));
            }
        }
    }

    let domain = match kind {
        EngineKind::FanCurve => crate::ipc::Domain::Cooling,
        EngineKind::Canvas => crate::ipc::Domain::Lighting,
        EngineKind::Lcd => crate::ipc::Domain::Lcd,
    };
    crate::ipc::broadcast_delta(&app, &[domain]).await;
    Ok(())
}

pub async fn set_log_level(level: String, app: Arc<AppState>) -> Result<()> {
    let level_filter = level
        .parse::<log::LevelFilter>()
        .map_err(|_| anyhow::anyhow!("invalid log level: {level}"))?;
    if level_filter == log::LevelFilter::Trace {
        anyhow::bail!("trace logging is not supported");
    }

    log::set_max_level(crate::logger::without_trace(level_filter));

    {
        let mut cfg = app.config.write().await;
        cfg.gui.log_level = level.to_lowercase();
    }
    app.request_config_save();
    log::info!("Log level changed to {level}");
    crate::ipc::broadcast_delta(&app, &[crate::ipc::Domain::Gui]).await;
    Ok(())
}

pub async fn set_language(lang: String, app: Arc<AppState>) -> Result<()> {
    let lang = lang.to_lowercase();
    if !halod_shared::types::SUPPORTED_LANGUAGES.contains(&lang.as_str()) {
        anyhow::bail!("unsupported language: {lang}");
    }
    {
        let mut cfg = app.config.write().await;
        cfg.gui.language = lang.clone();
    }
    app.request_config_save();
    log::info!("UI language changed to {lang}");
    crate::ipc::broadcast_delta(&app, &[crate::ipc::Domain::Gui]).await;
    Ok(())
}

pub async fn set_ui_config(
    close_to_tray: bool,
    suppress_dependency_warning: bool,
    hide_window_controls: bool,
    low_battery_notifications: bool,
    app: Arc<AppState>,
) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        cfg.gui.close_to_tray = close_to_tray;
        cfg.gui.suppress_dependency_warning = suppress_dependency_warning;
        cfg.gui.hide_window_controls = hide_window_controls;
        cfg.gui.low_battery_notifications = low_battery_notifications;
    }
    app.request_config_save();
    crate::ipc::broadcast_delta(&app, &[crate::ipc::Domain::Gui]).await;
    Ok(())
}

/// Allow or deny the daemon contacting GitHub for official plugins and
/// automatic update checks. An allowed request also retries a missing official
/// checkout, which lets first-run onboarding recover from an offline bootstrap.
pub async fn set_plugin_download_consent(allowed: bool, app: Arc<AppState>) -> Result<()> {
    use halod_shared::types::PluginDownloadConsent;
    {
        let mut cfg = app.config.write().await;
        cfg.gui.plugin_downloads = if allowed {
            PluginDownloadConsent::Allowed
        } else {
            PluginDownloadConsent::Denied
        };
    }
    app.request_config_save();
    crate::ipc::broadcast_delta(&app, &[crate::ipc::Domain::Gui]).await;
    if allowed {
        crate::registry::ensure_official_repo(&app).await;
        crate::plugin::usecases::plugins::reload_registry(&app).await;
        let official_plugins = app
            .registry
            .list(&*app.secret_store)
            .into_iter()
            .filter_map(|p| match p.source {
                halod_shared::types::PluginSource::Repo { slug }
                    if slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG =>
                {
                    Some(p.id)
                }
                _ => None,
            })
            .collect();
        crate::plugin::usecases::plugins::apply_repo_plugins(app.clone(), official_plugins).await?;
        crate::registry::start_update_check(app.clone()).await;
    }
    Ok(())
}

pub async fn mark_tour_seen(tour: String, app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        cfg.gui.seen_tours.insert(tour);
    }
    app.request_config_save();
    crate::ipc::broadcast_delta(&app, &[crate::ipc::Domain::Gui]).await;
    Ok(())
}

pub async fn reset_tours_seen(app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        cfg.gui.seen_tours.clear();
    }
    app.request_config_save();
    crate::ipc::broadcast_delta(&app, &[crate::ipc::Domain::Gui]).await;
    Ok(())
}

pub async fn rediscover(app: Arc<AppState>) -> Result<()> {
    log::info!("Rediscovery triggered via UI");
    // Re-read all sources so a freshly-dropped package is picked up by a
    // "Scan now" without restarting the daemon. This must use the shared
    // reload path to retain a process-local development repository.
    crate::plugin::usecases::plugins::reload_registry(&app).await;
    crate::plugin::usecases::plugins::reconcile_full(&app).await;

    let controllers: Vec<std::sync::Arc<dyn crate::drivers::Device>> =
        app.devices.read().await.clone();
    for dev in controllers {
        if let Some(ctrl) = dev.as_controller() {
            // Child ids from plugin packages are stable hardware ids (for
            // example a Logitech serial or NVIDIA UUID), not necessarily
            // strings prefixed by the controller id. Keep the ownership set
            // established at registration and let the controller perform an
            // exact live diff against it.
            let existing = app
                .device_children
                .lock()
                .await
                .get(dev.id())
                .cloned()
                .unwrap_or_default();
            let Ok((added, gone)) = ctrl.resync_children(&existing).await else {
                continue;
            };

            let mut registered = std::collections::HashSet::new();
            for child in added {
                let child_id = child.id().to_owned();
                if crate::registry::usecases::registration::register_device(&app, child).await {
                    registered.insert(child_id);
                }
            }

            if !gone.is_empty() {
                let removed: Vec<std::sync::Arc<dyn crate::drivers::Device>> = {
                    let mut devices = app.devices.write().await;
                    let mut removed = Vec::new();
                    devices.retain(|device| {
                        if gone.iter().any(|id| id == device.id()) {
                            removed.push(device.clone());
                            false
                        } else {
                            true
                        }
                    });
                    removed
                };
                for child in removed {
                    super::registration::close_device(&app, &child).await;
                }
            }

            if !gone.is_empty() || !registered.is_empty() {
                let mut owners = app.device_children.lock().await;
                let children = owners.entry(dev.id().to_owned()).or_default();
                for id in gone {
                    children.remove(&id);
                }
                children.extend(registered);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tmp_config;

    #[tokio::test]
    async fn set_engine_config_fan_curve_updates_enabled() {
        with_tmp_config(|app| async move {
            set_engine_config(
                EngineKind::FanCurve,
                Some(false),
                None,
                None,
                None,
                app.clone(),
            )
            .await
            .unwrap();
            assert!(!app.config.read().await.cooling.fan_curve_enabled);
        })
        .await;
    }

    #[tokio::test]
    async fn set_engine_config_fan_curve_clamps_tick_ms_to_minimum() {
        with_tmp_config(|app| async move {
            set_engine_config(
                EngineKind::FanCurve,
                None,
                Some(100),
                None,
                None,
                app.clone(),
            )
            .await
            .unwrap();
            assert_eq!(app.config.read().await.cooling.fan_curve_tick_ms, 500);
        })
        .await;
    }

    #[tokio::test]
    async fn set_engine_config_canvas_clamps_fps_to_maximum() {
        with_tmp_config(|app| async move {
            set_engine_config(EngineKind::Canvas, None, None, Some(999), None, app.clone())
                .await
                .unwrap();
            assert_eq!(app.config.read().await.rgb.canvas_fps, 60);
        })
        .await;
    }

    #[tokio::test]
    async fn set_log_level_invalid_level_returns_error() {
        with_tmp_config(|app| async move {
            assert!(set_log_level("nonsense".into(), app.clone()).await.is_err());
            assert!(set_log_level("trace".into(), app).await.is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn set_log_level_updates_config() {
        with_tmp_config(|app| async move {
            set_log_level("debug".into(), app.clone()).await.unwrap();
            assert_eq!(app.config.read().await.gui.log_level, "debug");
        })
        .await;
    }

    #[tokio::test]
    async fn set_language_rejects_unsupported() {
        with_tmp_config(|app| async move {
            assert!(set_language("xx".into(), app).await.is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn set_language_persists_supported_code() {
        with_tmp_config(|app| async move {
            let generation_before = app.state_gen.load(std::sync::atomic::Ordering::Relaxed);
            set_language("EN".into(), app.clone()).await.unwrap();
            assert_eq!(app.config.read().await.gui.language, "en");
            assert_eq!(
                app.state_gen.load(std::sync::atomic::Ordering::Relaxed),
                generation_before + 1,
                "changing language must publish a GUI state delta"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn set_ui_config_updates_close_to_tray() {
        with_tmp_config(|app| async move {
            set_ui_config(false, true, true, false, app.clone())
                .await
                .unwrap();
            let cfg = app.config.read().await;
            assert!(!cfg.gui.close_to_tray);
            assert!(cfg.gui.suppress_dependency_warning);
            assert!(cfg.gui.hide_window_controls);
            assert!(!cfg.gui.low_battery_notifications);
        })
        .await;
    }

    #[tokio::test]
    async fn mark_tour_seen_inserts_and_is_idempotent() {
        with_tmp_config(|app| async move {
            mark_tour_seen("page:home".into(), app.clone())
                .await
                .unwrap();
            mark_tour_seen("page:home".into(), app.clone())
                .await
                .unwrap();
            mark_tour_seen("tab:cooling".into(), app.clone())
                .await
                .unwrap();
            let cfg = app.config.read().await;
            assert_eq!(cfg.gui.seen_tours.len(), 2);
            assert!(cfg.gui.seen_tours.contains("page:home"));
            assert!(cfg.gui.seen_tours.contains("tab:cooling"));
        })
        .await;
    }

    #[tokio::test]
    async fn reset_tours_seen_clears_all() {
        with_tmp_config(|app| async move {
            mark_tour_seen("page:home".into(), app.clone())
                .await
                .unwrap();
            reset_tours_seen(app.clone()).await.unwrap();
            assert!(app.config.read().await.gui.seen_tours.is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn set_engine_config_lcd_updates_enabled() {
        with_tmp_config(|app| async move {
            set_engine_config(EngineKind::Lcd, Some(false), None, None, None, app.clone())
                .await
                .unwrap();
            assert!(!app.config.read().await.lcd.enabled);
        })
        .await;
    }

    #[tokio::test]
    async fn set_engine_config_lcd_clamps_fps_to_maximum() {
        with_tmp_config(|app| async move {
            set_engine_config(EngineKind::Lcd, None, None, Some(999), None, app.clone())
                .await
                .unwrap();
            assert_eq!(app.config.read().await.lcd.fps, 60);
        })
        .await;
    }
}
