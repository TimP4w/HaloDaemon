// SPDX-License-Identifier: GPL-3.0-or-later
//! Routes semantic topics to domain-owned projections.

use std::sync::Arc;

use crate::application::state::AppState;
use crate::config::Config;
use crate::domain::device::projection::DevicesSnapshot;
use halod_shared::bus::{topic, BusValue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Topic {
    Discovery,
    Device(String),
    Devices,
    Profiles,
    Cooling,
    Lighting,
    Lcd,
    Gui,
    Health,
    ProcessIcons,
    Plugins,
    ConfigDir,
}

pub(super) const ALL_TOPICS: &[Topic] = &[
    Topic::Discovery,
    Topic::Devices,
    Topic::Profiles,
    Topic::Cooling,
    Topic::Lighting,
    Topic::Lcd,
    Topic::Gui,
    Topic::Health,
    Topic::ProcessIcons,
    Topic::Plugins,
    Topic::ConfigDir,
];

pub async fn produce(
    app: &Arc<AppState>,
    cfg: &Config,
    requested: &[Topic],
) -> Vec<(String, BusValue)> {
    let needs_all_devices = requested.iter().any(|topic| {
        matches!(
            topic,
            Topic::Devices | Topic::Cooling | Topic::Lighting | Topic::Lcd | Topic::Plugins
        )
    });
    let selected_ids: std::collections::HashSet<String> = requested
        .iter()
        .filter_map(|topic| match topic {
            Topic::Device(id) => Some(id.clone()),
            _ => None,
        })
        .collect();
    let DevicesSnapshot {
        devices,
        fan_curves,
        placed_zones,
        lcd_templates,
        lcd_template_params,
    } = if needs_all_devices {
        app.snapshot_devices(cfg).await
    } else if !selected_ids.is_empty() {
        app.snapshot_selected_devices(cfg, Some(&selected_ids))
            .await
    } else {
        DevicesSnapshot::default()
    };
    let mut records = Vec::new();
    for requested_topic in requested {
        match requested_topic {
            Topic::Discovery => records.push((
                topic::DISCOVERY.into(),
                BusValue::Discovery(app.discovery.lock().await.clone()),
            )),
            Topic::Device(id) => records.extend(
                devices
                    .iter()
                    .filter(|device| &device.id == id)
                    .cloned()
                    .map(|device| (topic::device(&device.id), BusValue::Device(device))),
            ),
            Topic::Devices => records.extend(
                devices
                    .iter()
                    .cloned()
                    .map(|device| (topic::device(&device.id), BusValue::Device(device))),
            ),
            Topic::Profiles => records.push((
                topic::PROFILES.into(),
                BusValue::Profiles(crate::domain::profiles::projection::profiles(cfg)),
            )),
            Topic::Cooling => records.push((
                topic::COOLING.into(),
                BusValue::Cooling(
                    crate::domain::cooling::projection::project(app, cfg, fan_curves.clone()).await,
                ),
            )),
            Topic::Lighting => records.push((
                topic::LIGHTING.into(),
                BusValue::Lighting(
                    crate::domain::lighting::projection::project(app, cfg, placed_zones.clone())
                        .await,
                ),
            )),
            Topic::Lcd => records.push((
                topic::LCD.into(),
                BusValue::Lcd(
                    crate::domain::lcd::projection::project(
                        app,
                        cfg,
                        lcd_templates.clone(),
                        lcd_template_params.clone(),
                    )
                    .await,
                ),
            )),
            Topic::Gui => records.push((topic::GUI.into(), BusValue::Gui(cfg.gui.clone()))),
            Topic::Health => records.push((
                topic::HEALTH.into(),
                BusValue::Health(halod_shared::types::HealthCheckState {
                    focus_watcher_supported: app.focus.supported(),
                    ffmpeg_available: crate::domain::lcd::engine::video::ffmpeg_available(),
                }),
            )),
            Topic::ProcessIcons => records.push((
                topic::PROCESS_ICONS.into(),
                BusValue::ProcessIcons(crate::domain::profiles::projection::process_icons(cfg)),
            )),
            Topic::Plugins => records.push((
                topic::PLUGINS.into(),
                BusValue::Plugins(
                    crate::domain::plugin::projection::project(app, cfg, &devices).await,
                ),
            )),
            Topic::ConfigDir => records.push((
                topic::CONFIG_DIR.into(),
                BusValue::ConfigDir(crate::config::config_dir().display().to_string()),
            )),
        }
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::drivers::Device;
    use crate::test_support::MockDevice;

    #[tokio::test]
    async fn gui_projection_does_not_require_a_device_snapshot() {
        let app = Arc::new(AppState::new(Config::default()));
        let records = produce(&app, &app.config.read().await.clone(), &[Topic::Gui]).await;
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0].1, BusValue::Gui(_)));
    }

    #[tokio::test]
    async fn targeted_device_projection_emits_only_that_device() {
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry.write().await.extend([
            Arc::new(MockDevice::new("first")) as Arc<dyn Device>,
            Arc::new(MockDevice::new("second")) as Arc<dyn Device>,
        ]);
        let records = produce(
            &app,
            &app.config.read().await.clone(),
            &[Topic::Device("second".into())],
        )
        .await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, topic::device("second"));
    }
}
