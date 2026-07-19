// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{Config, PlacedZone};
use crate::cooling::config::FanCurveRecord;
use crate::registry::snapshot::DevicesSnapshot;
use crate::state::AppState;
use halod_shared::bus::{topic, BusValue};
use halod_shared::types::{
    CoolingState, DiscoveryStatus, HealthCheckState, LcdState, LightingOverviewState,
    PluginRepoInfo, PluginsState, ProfileState, RepoCompatibilityStatus, RepoSignatureStatus,
    WireDevice,
};

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

fn active_repo_signature_status(record: &crate::config::PluginRepoRecord) -> RepoSignatureStatus {
    let trust = if record.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG {
        crate::plugin::repo::RepositoryTrust::Official
    } else if let Some(key) = &record.trusted_key {
        crate::plugin::repo::RepositoryTrust::Pinned(key.clone())
    } else {
        return RepoSignatureStatus::Unsigned;
    };

    let root = crate::config::plugin_repos_dir().join(&record.slug);
    let result = if !record.locked_sha.is_empty() && root.join(".git").is_dir() {
        crate::plugin::repo::verify_repository_signature_at_commit(
            &root,
            &record.locked_sha,
            &trust,
        )
    } else if record.active_revision.is_some() {
        crate::plugin::repo::verify_repository_signature(
            &crate::plugin::repo::active_revision_dir(record),
            &trust,
        )
    } else {
        return RepoSignatureStatus::Invalid {
            reason: "the repository has not been downloaded or has no active revision".to_owned(),
        };
    };

    match result {
        Ok(_) => RepoSignatureStatus::Verified,
        Err(error) => RepoSignatureStatus::Invalid {
            reason: format!("{error:#}"),
        },
    }
}

async fn produce_discovery(app: &Arc<AppState>) -> DiscoveryStatus {
    app.discovery.lock().await.clone()
}

fn produce_profiles(cfg: &Config) -> ProfileState {
    ProfileState {
        active: cfg.active_profile.clone(),
        available: cfg.profile_names(),
        app_rules: cfg.app_rules.clone(),
        overrides: cfg.profile_overrides(),
    }
}

fn produce_gui(cfg: &Config) -> halod_shared::types::GuiConfig {
    cfg.gui.clone()
}

fn produce_health(app: &Arc<AppState>) -> HealthCheckState {
    HealthCheckState {
        focus_watcher_supported: app.focus.supported(),
        ffmpeg_available: crate::lcd::engine::video::ffmpeg_available(),
    }
}

fn produce_process_icons(cfg: &Config) -> HashMap<String, String> {
    let mut names: Vec<String> = cfg
        .app_rules
        .iter()
        .flat_map(|r| r.process_names.iter().cloned())
        .collect();
    names.sort();
    names.dedup();
    crate::profiles::running_apps::resolve_process_icons(&names)
}

async fn produce_cooling(
    app: &Arc<AppState>,
    cfg: &Config,
    fan_curves: Vec<(String, String, FanCurveRecord)>,
) -> CoolingState {
    let mut cooling = app.cooling.snapshot(fan_curves).await;
    cooling.config = cfg.cooling.clone();
    cooling
}

async fn produce_lighting(
    app: &Arc<AppState>,
    cfg: &Config,
    placed_zones: Vec<PlacedZone>,
) -> LightingOverviewState {
    let mut lighting = app
        .lighting
        .snapshot(&app.registry, cfg, placed_zones)
        .await;
    lighting.config = cfg.rgb.clone();
    lighting
}

async fn produce_lcd(
    app: &Arc<AppState>,
    cfg: &Config,
    lcd_templates: HashMap<String, String>,
    lcd_template_params: HashMap<String, HashMap<String, halod_shared::types::EffectParamValue>>,
) -> LcdState {
    let mut lcd = app
        .lcd
        .snapshot(&app.registry, lcd_templates, lcd_template_params)
        .await;
    lcd.config = cfg.lcd.clone();
    lcd
}

async fn produce_plugins(
    app: &Arc<AppState>,
    cfg: &Config,
    devices: &[WireDevice],
) -> PluginsState {
    let observed_repo_signatures = app.repo_signature_status.lock().await.clone();
    let repo_compatibility = app.repo_compatibility_status.lock().await.clone();

    let mut plugin_list = app.registry.list(app.secret_store.as_ref());
    for plugin in &mut plugin_list {
        if plugin.plugin_type == halod_shared::types::PluginKind::Integration
            && plugin.integration_state
                == halod_shared::types::IntegrationLifecycleState::Configured
            && devices.iter().any(|device| {
                device.integration_id.as_deref() == Some(plugin.id.as_str()) && device.connected
            })
        {
            plugin.integration_state = halod_shared::types::IntegrationLifecycleState::Active;
        }
        plugin.data_records = app
            .data_bus
            .statuses_for_owner(&plugin.id)
            .into_iter()
            .map(
                |(key, snapshot)| halod_shared::types::PluginDataRecordStatus {
                    key,
                    status: match snapshot.status {
                        crate::services::data_bus::SnapshotStatus::Fresh => "fresh",
                        crate::services::data_bus::SnapshotStatus::Stale => "stale",
                        crate::services::data_bus::SnapshotStatus::Unavailable => "unavailable",
                    }
                    .to_owned(),
                    updated_at: snapshot.published_at,
                },
            )
            .collect();
    }
    PluginsState {
        plugins: plugin_list,
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
                location: match r.source_kind {
                    crate::config::PluginRepoSourceKind::Archive => {
                        halod_shared::types::PluginRepoLocation::LocalArchive
                    }
                    crate::config::PluginRepoSourceKind::Git if r.url.starts_with("file://") => {
                        halod_shared::types::PluginRepoLocation::LocalGit
                    }
                    crate::config::PluginRepoSourceKind::Git => {
                        halod_shared::types::PluginRepoLocation::RemoteGit
                    }
                },
                signature: observed_repo_signatures
                    .get(&r.slug)
                    .filter(|(sha, _)| sha != &r.locked_sha)
                    .map(|(_, status)| status.clone())
                    .unwrap_or_else(|| active_repo_signature_status(r)),
                signing_key_fingerprint: r
                    .trusted_key
                    .as_ref()
                    .and_then(|key| halod_plugin_signing::signing_key_fingerprint(key).ok()),
                compatibility: repo_compatibility
                    .get(&r.slug)
                    .cloned()
                    .unwrap_or(RepoCompatibilityStatus::Compatible),
            })
            .collect(),
        skipped: app.registry.skipped(),
        recommendations: app.registry.recommendations(),
        updates: app.plugin_update_status.lock().await.clone(),
        repo_updates: app.plugin_repo_update_status.lock().await.clone(),
    }
}

pub async fn produce(
    app: &Arc<AppState>,
    cfg: &Config,
    requested: &[Topic],
) -> Vec<(String, BusValue)> {
    let needs_devices = requested.iter().any(|topic| {
        matches!(
            topic,
            Topic::Device(_)
                | Topic::Devices
                | Topic::Cooling
                | Topic::Lighting
                | Topic::Lcd
                | Topic::Plugins
        )
    });
    let DevicesSnapshot {
        devices,
        fan_curves,
        placed_zones,
        lcd_templates,
        lcd_template_params,
    } = if needs_devices {
        app.snapshot_devices(cfg).await
    } else {
        DevicesSnapshot::default()
    };
    let mut records = Vec::new();
    for requested_topic in requested {
        match requested_topic {
            Topic::Discovery => records.push((
                topic::DISCOVERY.into(),
                BusValue::Discovery(produce_discovery(app).await),
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
                BusValue::Profiles(produce_profiles(cfg)),
            )),
            Topic::Cooling => records.push((
                topic::COOLING.into(),
                BusValue::Cooling(produce_cooling(app, cfg, fan_curves.clone()).await),
            )),
            Topic::Lighting => records.push((
                topic::LIGHTING.into(),
                BusValue::Lighting(produce_lighting(app, cfg, placed_zones.clone()).await),
            )),
            Topic::Lcd => records.push((
                topic::LCD.into(),
                BusValue::Lcd(
                    produce_lcd(app, cfg, lcd_templates.clone(), lcd_template_params.clone()).await,
                ),
            )),
            Topic::Gui => records.push((topic::GUI.into(), BusValue::Gui(produce_gui(cfg)))),
            Topic::Health => {
                records.push((topic::HEALTH.into(), BusValue::Health(produce_health(app))))
            }
            Topic::ProcessIcons => records.push((
                topic::PROCESS_ICONS.into(),
                BusValue::ProcessIcons(produce_process_icons(cfg)),
            )),
            Topic::Plugins => records.push((
                topic::PLUGINS.into(),
                BusValue::Plugins(produce_plugins(app, cfg, &devices).await),
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
    use crate::{
        config::{Config, PluginRepoRecord},
        drivers::Device,
        test_support::MockDevice,
    };

    fn repo_record(slug: &str) -> PluginRepoRecord {
        PluginRepoRecord {
            url: format!("https://example.com/{slug}.git"),
            slug: slug.to_owned(),
            repository_id: None,
            trusted_key: None,
            source_kind: crate::config::PluginRepoSourceKind::Git,
            branch: None,
            locked_sha: String::new(),
            active_revision: None,
            active_source: crate::config::PluginRevisionSource::Managed,
            previous_verified_sha: None,
            last_sync: None,
        }
    }

    #[test]
    fn third_party_repositories_are_reported_as_unsigned() {
        assert_eq!(
            active_repo_signature_status(&repo_record("community")),
            RepoSignatureStatus::Unsigned
        );
    }

    #[test]
    fn unavailable_official_repository_reports_verification_failure() {
        let status =
            active_repo_signature_status(&repo_record(crate::constants::OFFICIAL_PLUGIN_REPO_SLUG));
        assert!(matches!(status, RepoSignatureStatus::Invalid { .. }));
    }

    #[tokio::test]
    async fn producer_surfaces_a_rejected_remote_signature() {
        let app = Arc::new(AppState::new(Config::default()));
        let mut record = repo_record(crate::constants::OFFICIAL_PLUGIN_REPO_SLUG);
        record.locked_sha = "active".to_owned();
        let mut cfg = app.config.read().await.clone();
        cfg.plugins.repos.push(record);
        app.repo_signature_status.lock().await.insert(
            crate::constants::OFFICIAL_PLUGIN_REPO_SLUG.to_owned(),
            (
                "remote".to_owned(),
                RepoSignatureStatus::Invalid {
                    reason: "repository.sig is missing".to_owned(),
                },
            ),
        );
        app.repo_compatibility_status.lock().await.insert(
            crate::constants::OFFICIAL_PLUGIN_REPO_SLUG.to_owned(),
            RepoCompatibilityStatus::Incompatible {
                reason: "requires Halo >=0.4.0".to_owned(),
            },
        );

        let records = produce(&app, &cfg, ALL_TOPICS).await;
        let plugins = records
            .into_iter()
            .find_map(|(_, value)| match value {
                BusValue::Plugins(value) => Some(value),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            plugins.repos[0].signature,
            RepoSignatureStatus::Invalid {
                reason: "repository.sig is missing".to_owned(),
            }
        );
        assert_eq!(
            plugins.repos[0].compatibility,
            RepoCompatibilityStatus::Incompatible {
                reason: "requires Halo >=0.4.0".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn produce_empty_state() {
        let app = Arc::new(AppState::new(Config::default()));
        let mut cfg = app.config.read().await.clone();
        cfg.cooling.fan_failsafe_duty = 42;
        cfg.rgb.canvas_fps = 33;
        cfg.lcd.fps = 12;
        cfg.gui.language = "it".into();
        let records = produce(&app, &cfg, ALL_TOPICS).await;
        assert!(!records
            .iter()
            .any(|(_, value)| matches!(value, BusValue::Device(_))));
        assert!(records.iter().any(
            |(_, value)| matches!(value, BusValue::Cooling(v) if v.config.fan_failsafe_duty == 42)
        ));
        assert!(records
            .iter()
            .any(|(_, value)| matches!(value, BusValue::Lighting(v) if v.config.canvas_fps == 33)));
        assert!(records
            .iter()
            .any(|(_, value)| matches!(value, BusValue::Lcd(v) if v.config.fps == 12)));
        assert!(records
            .iter()
            .any(|(_, value)| matches!(value, BusValue::Gui(v) if v.language == "it")));
        assert!(records
            .iter()
            .any(|(_, value)| matches!(value, BusValue::Profiles(v) if v.active == "default")));
    }

    #[tokio::test]
    async fn produce_with_one_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev: Arc<dyn Device> = Arc::new(
            MockDevice::new("test_device")
                .with_name("Test Fan")
                .with_vendor("Acme")
                .with_model("Fan 3000")
                .with_fan()
                .with_rgb(),
        );
        app.device_registry.write().await.push(dev);
        let cfg = app.config.read().await.clone();
        let records = produce(&app, &cfg, ALL_TOPICS).await;
        let device = records
            .into_iter()
            .find_map(|(_, value)| match value {
                BusValue::Device(value) => Some(value),
                _ => None,
            })
            .unwrap();
        assert_eq!(device.id, "test_device");
        assert_eq!(device.name, "Test Fan");
        assert_eq!(device.vendor, "Acme");
    }
}
