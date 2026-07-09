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

    let cfg_snap = {
        let mut cfg = app.config.write().await;
        match kind {
            EngineKind::FanCurve => {
                if let Some(v) = enabled {
                    cfg.global.engine_fan_curve_enabled = v;
                }
                if let Some(ms) = tick_ms {
                    cfg.global.engine_fan_curve_tick_ms = ms.clamp(500, 60_000);
                }
                if let Some(d) = failsafe_duty {
                    cfg.global.fan_failsafe_duty = d;
                }
            }
            EngineKind::Canvas => {
                if let Some(v) = enabled {
                    cfg.global.engine_canvas_enabled = v;
                }
                if let Some(fps) = fps {
                    cfg.global.engine_canvas_fps = fps.clamp(1, 60) as u32;
                }
            }
            EngineKind::Lcd => {
                if let Some(v) = enabled {
                    cfg.global.engine_lcd_enabled = v;
                }
                if let Some(fps) = fps {
                    cfg.global.engine_lcd_fps = fps.clamp(1, 60) as u32;
                }
            }
        }
        cfg.global.clone()
    };

    app.request_config_save();

    // Signal the affected engine to re-read config.
    match kind {
        EngineKind::FanCurve => {
            if let Some(tx) = app.cooling.cfg_tx() {
                let _ = tx.send(EngineRunConfig::fan_curve(&cfg_snap));
            }
            if failsafe_duty.is_some() {
                if let Some(tx) = app.cooling.failsafe_duty_tx() {
                    let _ = tx.send(cfg_snap.fan_failsafe_duty);
                }
            }
        }
        EngineKind::Canvas => {
            if let Some(tx) = app.lighting.cfg_tx() {
                let _ = tx.send(EngineRunConfig::canvas(&cfg_snap));
            }
        }
        EngineKind::Lcd => {
            if let Some(tx) = app.lcd.cfg_tx() {
                let _ = tx.send(EngineRunConfig::lcd(&cfg_snap));
            }
        }
    }

    Ok(())
}

pub async fn set_log_level(level: String, app: Arc<AppState>) -> Result<()> {
    let level_filter = level
        .parse::<log::LevelFilter>()
        .map_err(|_| anyhow::anyhow!("invalid log level: {level}"))?;

    log::set_max_level(level_filter);

    {
        let mut cfg = app.config.write().await;
        cfg.global.log_level = level.to_lowercase();
    }
    app.request_config_save();
    log::info!("Log level changed to {level}");
    Ok(())
}

pub async fn set_language(lang: String, app: Arc<AppState>) -> Result<()> {
    let lang = lang.to_lowercase();
    if !halod_shared::types::SUPPORTED_LANGUAGES.contains(&lang.as_str()) {
        anyhow::bail!("unsupported language: {lang}");
    }
    {
        let mut cfg = app.config.write().await;
        cfg.global.language = lang.clone();
    }
    app.request_config_save();
    log::info!("UI language changed to {lang}");
    Ok(())
}

pub async fn set_ui_config(
    close_to_tray: bool,
    suppress_dependency_warning: bool,
    hide_window_controls: bool,
    app: Arc<AppState>,
) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        cfg.global.close_to_tray = close_to_tray;
        cfg.global.suppress_dependency_warning = suppress_dependency_warning;
        cfg.global.hide_window_controls = hide_window_controls;
    }
    app.request_config_save();
    Ok(())
}

pub async fn mark_tour_seen(tour: String, app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        cfg.global.seen_tours.insert(tour);
    }
    app.request_config_save();
    Ok(())
}

pub async fn reset_tours_seen(app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        cfg.global.seen_tours.clear();
    }
    app.request_config_save();
    Ok(())
}

pub async fn rediscover(app: Arc<AppState>) -> Result<()> {
    log::info!("Rediscovery triggered via UI");
    crate::registry::discovery::discover_devices(Arc::clone(&app)).await;

    let controllers: Vec<std::sync::Arc<dyn crate::drivers::Device>> =
        app.devices.read().await.clone();
    for dev in controllers {
        if let Some(ctrl) = dev.as_controller() {
            let children = ctrl.rescan_children().await;
            for child in children {
                crate::registry::usecases::registration::register_device(&app, child).await;
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
            assert!(!app.config.read().await.global.engine_fan_curve_enabled);
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
            assert_eq!(app.config.read().await.global.engine_fan_curve_tick_ms, 500);
        })
        .await;
    }

    #[tokio::test]
    async fn set_engine_config_canvas_clamps_fps_to_maximum() {
        with_tmp_config(|app| async move {
            set_engine_config(EngineKind::Canvas, None, None, Some(999), None, app.clone())
                .await
                .unwrap();
            assert_eq!(app.config.read().await.global.engine_canvas_fps, 60);
        })
        .await;
    }

    #[tokio::test]
    async fn set_log_level_invalid_level_returns_error() {
        with_tmp_config(|app| async move {
            assert!(set_log_level("nonsense".into(), app).await.is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn set_log_level_updates_config() {
        with_tmp_config(|app| async move {
            set_log_level("debug".into(), app.clone()).await.unwrap();
            assert_eq!(app.config.read().await.global.log_level, "debug");
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
            set_language("EN".into(), app.clone()).await.unwrap();
            assert_eq!(app.config.read().await.global.language, "en");
        })
        .await;
    }

    #[tokio::test]
    async fn set_ui_config_updates_close_to_tray() {
        with_tmp_config(|app| async move {
            set_ui_config(false, true, true, app.clone()).await.unwrap();
            let cfg = app.config.read().await;
            assert!(!cfg.global.close_to_tray);
            assert!(cfg.global.suppress_dependency_warning);
            assert!(cfg.global.hide_window_controls);
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
            assert_eq!(cfg.global.seen_tours.len(), 2);
            assert!(cfg.global.seen_tours.contains("page:home"));
            assert!(cfg.global.seen_tours.contains("tab:cooling"));
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
            assert!(app.config.read().await.global.seen_tours.is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn set_engine_config_lcd_updates_enabled() {
        with_tmp_config(|app| async move {
            set_engine_config(EngineKind::Lcd, Some(false), None, None, None, app.clone())
                .await
                .unwrap();
            assert!(!app.config.read().await.global.engine_lcd_enabled);
        })
        .await;
    }

    #[tokio::test]
    async fn set_engine_config_lcd_clamps_fps_to_maximum() {
        with_tmp_config(|app| async move {
            set_engine_config(EngineKind::Lcd, None, None, Some(999), None, app.clone())
                .await
                .unwrap();
            assert_eq!(app.config.read().await.global.engine_lcd_fps, 60);
        })
        .await;
    }
}
