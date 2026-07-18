// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{Config, PlacedZone};
use crate::cooling::config::FanCurveRecord;
use crate::registry::snapshot::DevicesSnapshot;
use crate::state::AppState;
use halod_shared::types::{
    AppState as WireAppState, CoolingState, DiscoveryStatus, HealthCheckState,
    LightingOverviewState, LcdState, PluginRepoInfo, PluginsState, ProfileState,
    RepoCompatibilityStatus, RepoSignatureStatus, StateDelta, WireDevice,
};

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Domain {
    Discovery,
    Devices,
    Profiles,
    Cooling,
    Lighting,
    Lcd,
    Gui,
    Health,
    ProcessIcons,
    Plugins,
}

impl Domain {
    pub fn uses_device_pass(self) -> bool {
        matches!(
            self,
            Domain::Devices | Domain::Cooling | Domain::Lighting | Domain::Lcd | Domain::Plugins
        )
    }
}

async fn serialize_discovery(app: &Arc<AppState>) -> DiscoveryStatus {
    app.discovery.lock().await.clone()
}

fn serialize_profiles(cfg: &Config) -> ProfileState {
    ProfileState {
        active: cfg.active_profile.clone(),
        available: cfg.profile_names(),
        app_rules: cfg.app_rules.clone(),
        overrides: cfg.profile_overrides(),
    }
}

fn serialize_gui(cfg: &Config) -> halod_shared::types::GuiConfig {
    cfg.gui.clone()
}

fn serialize_health(app: &Arc<AppState>) -> HealthCheckState {
    HealthCheckState {
        focus_watcher_supported: app.focus.supported(),
        ffmpeg_available: crate::lcd::engine::video::ffmpeg_available(),
    }
}

fn serialize_process_icons(cfg: &Config) -> HashMap<String, String> {
    let mut names: Vec<String> = cfg
        .app_rules
        .iter()
        .flat_map(|r| r.process_names.iter().cloned())
        .collect();
    names.sort();
    names.dedup();
    crate::profiles::running_apps::resolve_process_icons(&names)
}

async fn serialize_cooling(
    app: &Arc<AppState>,
    cfg: &Config,
    fan_curves: Vec<(String, String, FanCurveRecord)>,
) -> CoolingState {
    let mut cooling = app.cooling.snapshot(fan_curves).await;
    cooling.config = cfg.cooling.clone();
    cooling
}

async fn serialize_lighting(
    app: &Arc<AppState>,
    cfg: &Config,
    placed_zones: Vec<PlacedZone>,
) -> LightingOverviewState {
    let mut lighting = app.lighting.snapshot(&app.registry, cfg, placed_zones).await;
    lighting.config = cfg.rgb.clone();
    lighting
}

async fn serialize_lcd(
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

async fn serialize_plugins(
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
    }
}

pub async fn build_full(app: &Arc<AppState>, cfg: &Config) -> WireAppState {
    let DevicesSnapshot {
        devices,
        fan_curves,
        placed_zones,
        lcd_templates,
        lcd_template_params,
    } = app.snapshot_devices(cfg).await;
    let plugins = serialize_plugins(app, cfg, &devices).await;
    WireAppState {
        discovery: serialize_discovery(app).await,
        devices,
        profiles: serialize_profiles(cfg),
        cooling: serialize_cooling(app, cfg, fan_curves).await,
        lighting: serialize_lighting(app, cfg, placed_zones).await,
        lcd: serialize_lcd(app, cfg, lcd_templates, lcd_template_params).await,
        gui: serialize_gui(cfg),
        config_dir: crate::config::config_dir().display().to_string(),
        health: serialize_health(app),
        process_icons: serialize_process_icons(cfg),
        plugins,
    }
}

pub async fn build_delta(app: &Arc<AppState>, cfg: &Config, domains: &[Domain]) -> StateDelta {
    let want = |d: Domain| domains.contains(&d);
    let mut delta = StateDelta::default();

    if want(Domain::Discovery) {
        delta.discovery = Some(serialize_discovery(app).await);
    }
    if want(Domain::Profiles) {
        delta.profiles = Some(serialize_profiles(cfg));
    }
    if want(Domain::Gui) {
        delta.gui = Some(serialize_gui(cfg));
    }
    if want(Domain::Health) {
        delta.health = Some(serialize_health(app));
    }
    if want(Domain::ProcessIcons) {
        delta.process_icons = Some(serialize_process_icons(cfg));
    }

    if domains.iter().any(|d| d.uses_device_pass()) {
        let DevicesSnapshot {
            devices,
            fan_curves,
            placed_zones,
            lcd_templates,
            lcd_template_params,
        } = app.snapshot_devices(cfg).await;
        if want(Domain::Cooling) {
            delta.cooling = Some(serialize_cooling(app, cfg, fan_curves).await);
        }
        if want(Domain::Lighting) {
            delta.lighting = Some(serialize_lighting(app, cfg, placed_zones).await);
        }
        if want(Domain::Lcd) {
            delta.lcd = Some(serialize_lcd(app, cfg, lcd_templates, lcd_template_params).await);
        }
        if want(Domain::Plugins) {
            delta.plugins = Some(serialize_plugins(app, cfg, &devices).await);
        }
        if want(Domain::Devices) {
            delta.devices = Some(devices);
        }
    }

    delta
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
    async fn serialize_surfaces_a_rejected_remote_signature() {
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

        let wire = build_full(&app, &cfg).await;
        assert_eq!(
            wire.plugins.repos[0].signature,
            RepoSignatureStatus::Invalid {
                reason: "repository.sig is missing".to_owned(),
            }
        );
        assert_eq!(
            wire.plugins.repos[0].compatibility,
            RepoCompatibilityStatus::Incompatible {
                reason: "requires Halo >=0.4.0".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn serialize_empty_state() {
        let app = Arc::new(AppState::new(Config::default()));
        let mut cfg = app.config.read().await.clone();
        cfg.cooling.fan_failsafe_duty = 42;
        cfg.rgb.canvas_fps = 33;
        cfg.lcd.fps = 12;
        cfg.gui.language = "it".into();
        let wire = build_full(&app, &cfg).await;
        assert_eq!(wire.devices.len(), 0);
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
        let wire = build_full(&app, &cfg).await;
        assert_eq!(wire.devices.len(), 1);
        assert_eq!(wire.devices[0].id, "test_device");
        assert_eq!(wire.devices[0].name, "Test Fan");
        assert_eq!(wire.devices[0].vendor, "Acme");
    }

    struct CountingDevice {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Device for CountingDevice {
        fn id(&self) -> &str {
            "counting"
        }
        fn name(&self) -> &str {
            "Counting"
        }
        fn vendor(&self) -> &str {
            "test"
        }
        fn model(&self) -> &str {
            "test"
        }
        async fn initialize(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        async fn serialize(&self) -> halod_shared::types::WireDevice {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            crate::drivers::vendors::generic::devices::common::WireDeviceBuilder::from_parts(
                self.id().to_string(),
                self.name().to_string(),
                self.vendor().to_string(),
                self.model().to_string(),
            )
            .build()
        }
        fn capabilities(&self) -> Vec<crate::drivers::CapabilityRef<'_>> {
            Vec::new()
        }
    }

    #[tokio::test]
    async fn non_device_domains_skip_the_device_pass() {
        let app = Arc::new(AppState::new(Config::default()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        app.devices.write().await.push(Arc::new(CountingDevice {
            calls: calls.clone(),
        }));
        let cfg = app.config.read().await.clone();
        let delta = build_delta(&app, &cfg, &[Domain::Profiles, Domain::Gui]).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(delta.profiles.is_some());
        assert!(delta.gui.is_some());
        assert!(delta.devices.is_none());
        assert!(delta.cooling.is_none());
    }

    #[tokio::test]
    async fn multi_device_domains_run_a_single_device_pass() {
        let app = Arc::new(AppState::new(Config::default()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        app.devices.write().await.push(Arc::new(CountingDevice {
            calls: calls.clone(),
        }));
        let cfg = app.config.read().await.clone();
        let delta = build_delta(&app, &cfg, &[Domain::Devices, Domain::Cooling]).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(delta.devices.is_some());
        assert!(delta.cooling.is_some());
        assert!(delta.profiles.is_none());
    }
}
