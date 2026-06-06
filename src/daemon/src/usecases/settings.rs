use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use crate::state::{AppState, EngineRunConfig};

pub async fn set_engine_config(msg: Value, app: Arc<AppState>) -> Result<()> {
    let engine = msg["engine"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing engine"))?;
    let enabled = msg["enabled"].as_bool();
    let failsafe_duty = msg["failsafe_duty"].as_u64().map(|v| v.clamp(0, 100) as u8);

    let cfg_snap = {
        let mut cfg = app.config.write().await;
        match engine {
            "fan_curve" => {
                if let Some(v) = enabled {
                    cfg.global.engine_fan_curve_enabled = v;
                }
                if let Some(ms) = msg["tick_ms"].as_u64() {
                    cfg.global.engine_fan_curve_tick_ms = ms.clamp(500, 60_000);
                }
                if let Some(d) = failsafe_duty {
                    cfg.global.fan_failsafe_duty = d;
                }
            }
            "canvas" => {
                if let Some(v) = enabled {
                    cfg.global.engine_canvas_enabled = v;
                }
                if let Some(fps) = msg["fps"].as_u64() {
                    cfg.global.engine_canvas_fps = fps.clamp(1, 60) as u32;
                }
            }
            "lcd" => {
                if let Some(v) = enabled {
                    cfg.global.engine_lcd_enabled = v;
                }
                if let Some(fps) = msg["fps"].as_u64() {
                    cfg.global.engine_lcd_fps = fps.clamp(1, 60) as u32;
                }
            }
            other => return Err(anyhow::anyhow!("unknown engine: {other}")),
        }
        cfg.clone()
    };

    app.request_config_save(cfg_snap.clone());

    // Signal the affected engine to re-read config.
    if let Some(engines) = app.engines.get() {
        match engine {
            "fan_curve" => {
                let run_cfg = EngineRunConfig {
                    enabled: cfg_snap.global.engine_fan_curve_enabled,
                    tick_ms: cfg_snap.global.engine_fan_curve_tick_ms,
                    failsafe_duty: cfg_snap.global.fan_failsafe_duty,
                };
                let _ = engines.fan_curve_cfg_tx.send(run_cfg);
            }
            "canvas" => {
                let tick_ms = 1000 / cfg_snap.global.engine_canvas_fps.max(1) as u64;
                let run_cfg = EngineRunConfig {
                    enabled: cfg_snap.global.engine_canvas_enabled,
                    tick_ms,
                    failsafe_duty: 0,
                };
                let _ = engines.canvas_cfg_tx.send(run_cfg);
            }
            "lcd" => {
                let tick_ms = 1000 / cfg_snap.global.engine_lcd_fps.max(1) as u64;
                let run_cfg = EngineRunConfig {
                    enabled: cfg_snap.global.engine_lcd_enabled,
                    tick_ms,
                    failsafe_duty: 0,
                };
                let _ = engines.lcd_cfg_tx.send(run_cfg);
            }
            _ => {}
        }
    }

    Ok(())
}

pub async fn set_log_level(msg: Value, app: Arc<AppState>) -> Result<()> {
    let level_str = msg["level"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing level"))?;

    let level_filter = level_str
        .parse::<log::LevelFilter>()
        .map_err(|_| anyhow::anyhow!("invalid log level: {level_str}"))?;

    log::set_max_level(level_filter);

    let cfg_snap = {
        let mut cfg = app.config.write().await;
        cfg.global.log_level = level_str.to_lowercase();
        cfg.clone()
    };
    app.request_config_save(cfg_snap);
    log::info!("Log level changed to {level_str}");
    Ok(())
}

pub async fn set_fan_failsafe_duty(msg: Value, app: Arc<AppState>) -> Result<()> {
    let duty = msg["duty"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing duty"))?
        .clamp(0, 100) as u8;

    let cfg_snap = {
        let mut cfg = app.config.write().await;
        cfg.global.fan_failsafe_duty = duty;
        cfg.clone()
    };
    app.request_config_save(cfg_snap.clone());

    // Propagate to fan curve engine watch channel.
    if let Some(engines) = app.engines.get() {
        let run_cfg = EngineRunConfig {
            enabled: cfg_snap.global.engine_fan_curve_enabled,
            tick_ms: cfg_snap.global.engine_fan_curve_tick_ms,
            failsafe_duty: duty,
        };
        let _ = engines.fan_curve_cfg_tx.send(run_cfg);
    }

    Ok(())
}

pub async fn set_ui_config(msg: Value, app: Arc<AppState>) -> Result<()> {
    let close_to_tray = msg["close_to_tray"]
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("missing close_to_tray"))?;

    let cfg_snap = {
        let mut cfg = app.config.write().await;
        cfg.global.close_to_tray = close_to_tray;
        cfg.clone()
    };
    app.request_config_save(cfg_snap);
    Ok(())
}

pub async fn rediscover(app: Arc<AppState>) -> Result<()> {
    log::info!("Rediscovery triggered via UI");
    crate::discovery::discover_devices(Arc::clone(&app)).await;

    let controllers: Vec<std::sync::Arc<dyn crate::drivers::Device>> =
        app.devices.lock().await.clone();
    for dev in controllers {
        if let Some(ctrl) = dev.as_controller() {
            ctrl.rescan_children(Arc::clone(&app)).await;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::state::AppState;
    use std::sync::Mutex;

    // Serialize env-var mutations so parallel tests don't race over HALOD_CONFIG_DIR.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    async fn with_tmp_config<F, Fut>(f: F)
    where
        F: FnOnce(Arc<AppState>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HALOD_CONFIG_DIR", tmp.path());
        f(make_app()).await;
        std::env::remove_var("HALOD_CONFIG_DIR");
    }

    #[tokio::test]
    async fn set_engine_config_fan_curve_updates_enabled() {
        with_tmp_config(|app| async move {
            let msg = serde_json::json!({"engine": "fan_curve", "enabled": false});
            set_engine_config(msg, app.clone()).await.unwrap();
            assert!(!app.config.read().await.global.engine_fan_curve_enabled);
        })
        .await;
    }

    #[tokio::test]
    async fn set_engine_config_fan_curve_clamps_tick_ms_to_minimum() {
        with_tmp_config(|app| async move {
            let msg = serde_json::json!({"engine": "fan_curve", "tick_ms": 100u64});
            set_engine_config(msg, app.clone()).await.unwrap();
            assert_eq!(app.config.read().await.global.engine_fan_curve_tick_ms, 500);
        })
        .await;
    }

    #[tokio::test]
    async fn set_engine_config_canvas_clamps_fps_to_maximum() {
        with_tmp_config(|app| async move {
            let msg = serde_json::json!({"engine": "canvas", "fps": 999u64});
            set_engine_config(msg, app.clone()).await.unwrap();
            assert_eq!(app.config.read().await.global.engine_canvas_fps, 60);
        })
        .await;
    }

    #[tokio::test]
    async fn set_engine_config_unknown_engine_returns_error() {
        with_tmp_config(|app| async move {
            let msg = serde_json::json!({"engine": "unknown"});
            assert!(set_engine_config(msg, app).await.is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn set_fan_failsafe_duty_clamps_to_100() {
        with_tmp_config(|app| async move {
            let msg = serde_json::json!({"duty": 200u64});
            set_fan_failsafe_duty(msg, app.clone()).await.unwrap();
            assert_eq!(app.config.read().await.global.fan_failsafe_duty, 100);
        })
        .await;
    }

    #[tokio::test]
    async fn set_fan_failsafe_duty_missing_field_returns_error() {
        with_tmp_config(|app| async move {
            let msg = serde_json::json!({});
            assert!(set_fan_failsafe_duty(msg, app).await.is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn set_log_level_invalid_level_returns_error() {
        with_tmp_config(|app| async move {
            let msg = serde_json::json!({"level": "nonsense"});
            assert!(set_log_level(msg, app).await.is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn set_log_level_updates_config() {
        with_tmp_config(|app| async move {
            let msg = serde_json::json!({"level": "debug"});
            set_log_level(msg, app.clone()).await.unwrap();
            assert_eq!(app.config.read().await.global.log_level, "debug");
        })
        .await;
    }

    #[tokio::test]
    async fn set_ui_config_updates_close_to_tray() {
        with_tmp_config(|app| async move {
            let msg = serde_json::json!({"close_to_tray": false});
            set_ui_config(msg, app.clone()).await.unwrap();
            assert!(!app.config.read().await.global.close_to_tray);
        })
        .await;
    }

    #[tokio::test]
    async fn set_ui_config_missing_field_returns_error() {
        with_tmp_config(|app| async move {
            let msg = serde_json::json!({});
            assert!(set_ui_config(msg, app).await.is_err());
        })
        .await;
    }
}
