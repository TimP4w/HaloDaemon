// SPDX-License-Identifier: GPL-3.0-or-later
use crate::domain::events::ChangeSink as _;

use anyhow::Result;
use std::sync::Arc;

use crate::application::state::AppState;
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

    let change = match kind {
        EngineKind::FanCurve => crate::domain::events::Change::Cooling,
        EngineKind::Canvas => crate::domain::events::Change::Lighting,
        EngineKind::Lcd => crate::domain::events::Change::Lcd,
    };
    app.record_change(change).await;
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
    app.record_change(crate::domain::events::Change::Gui).await;
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
    app.record_change(crate::domain::events::Change::Gui).await;
    Ok(())
}

pub async fn set_home_widgets(
    widgets: Vec<halod_shared::types::HomeWidget>,
    app: Arc<AppState>,
) -> Result<()> {
    halod_shared::types::validate_home_widgets(&widgets)?;
    {
        let mut cfg = app.config.write().await;
        cfg.gui.home_widgets = widgets;
    }
    app.request_config_save();
    app.record_change(crate::domain::events::Change::Gui).await;
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
    app.record_change(crate::domain::events::Change::Gui).await;
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
    app.record_change(crate::domain::events::Change::Gui).await;
    if allowed {
        crate::domain::registry::ensure_official_repo(&app).await;
        crate::application::usecases::plugin::plugins::reload_registry(&app).await;
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
        crate::application::usecases::plugin::plugins::apply_repo_plugins(
            app.clone(),
            official_plugins,
        )
        .await?;
        crate::domain::registry::start_update_check(app.clone()).await;
    }
    Ok(())
}

pub async fn mark_tour_seen(tour: String, app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        cfg.gui.seen_tours.insert(tour);
    }
    app.request_config_save();
    app.record_change(crate::domain::events::Change::Gui).await;
    Ok(())
}

pub async fn reset_tours_seen(app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        cfg.gui.seen_tours.clear();
    }
    app.request_config_save();
    app.record_change(crate::domain::events::Change::Gui).await;
    Ok(())
}

pub async fn rediscover(app: Arc<AppState>) -> Result<()> {
    log::info!("Rediscovery triggered via UI");
    // Re-read all sources so a freshly-dropped package is picked up by a
    // "Scan now" without restarting the daemon. This must use the shared
    // reload path to retain a process-local development repository.
    crate::application::usecases::plugin::plugins::reload_registry(&app).await;
    crate::application::usecases::plugin::plugins::reconcile_full(&app).await;

    let controllers: Vec<std::sync::Arc<dyn crate::domain::device::Device>> =
        app.device_registry.read().await.clone();
    for dev in controllers {
        super::receiver::reconcile_owned_children(&dev, &app).await;
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
            let revision_before = app.data_bus.state_snapshot(&[]).revision;
            set_language("EN".into(), app.clone()).await.unwrap();
            assert_eq!(app.config.read().await.gui.language, "en");
            assert_eq!(
                app.data_bus.state_snapshot(&[]).revision,
                revision_before + 1,
                "changing language must commit a bus transaction"
            );
        })
        .await;
    }

    fn chart_widget(id: &str) -> halod_shared::types::HomeWidget {
        halod_shared::types::HomeWidget {
            id: id.into(),
            kind: halod_shared::types::HomeWidgetKind::Chart {
                sensor_id: "cpu".into(),
            },
            color: 0,
            label: String::new(),
        }
    }

    #[tokio::test]
    async fn set_home_widgets_persists_the_row() {
        with_tmp_config(|app| async move {
            let revision_before = app.data_bus.state_snapshot(&[]).revision;
            let widgets = vec![chart_widget("a"), chart_widget("b")];
            set_home_widgets(widgets.clone(), app.clone())
                .await
                .unwrap();
            assert_eq!(app.config.read().await.gui.home_widgets, widgets);
            assert_eq!(
                app.data_bus.state_snapshot(&[]).revision,
                revision_before + 1,
                "changing the widget row must commit a bus transaction"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn set_home_widgets_rejects_an_invalid_row() {
        with_tmp_config(|app| async move {
            let err = set_home_widgets(vec![chart_widget("a"), chart_widget("a")], app.clone())
                .await
                .unwrap_err();
            assert!(err.to_string().contains("duplicate"), "{err}");
            assert!(app.config.read().await.gui.home_widgets.is_empty());
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
