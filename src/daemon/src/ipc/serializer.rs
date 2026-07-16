// SPDX-License-Identifier: GPL-3.0-or-later
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::state::AppState;
use halod_shared::types::{
    AppState as WireAppState, HealthCheckState, PluginRepoInfo, PluginsState, ProfileState,
    RepoCompatibilityStatus, RepoSignatureStatus,
};

fn active_repo_signature_status(record: &crate::config::PluginRepoRecord) -> RepoSignatureStatus {
    if record.slug != crate::constants::OFFICIAL_PLUGIN_REPO_SLUG {
        return RepoSignatureStatus::Unsigned;
    }

    let root = crate::config::plugin_repos_dir().join(&record.slug);
    let result = if !record.locked_sha.is_empty() && root.join(".git").is_dir() {
        crate::drivers::plugins::repo::verify_official_repository_signature_at_commit(
            &root,
            &record.locked_sha,
        )
    } else if record.active_revision.is_some() {
        crate::drivers::plugins::repo::verify_official_repository_signature(
            &crate::drivers::plugins::repo::active_revision_dir(record),
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

pub async fn serialize_state(
    app: &Arc<AppState>,
    cfg: crate::config::Config,
    process_icons: HashMap<String, String>,
) -> Value {
    let disc = app.discovery.lock().await.clone();
    let snap = app.snapshot_devices(&cfg).await;
    // The daemon persists each domain's config separately; the wire form nests
    // it under the matching State struct, so inject it here (the domain
    // snapshots stay config-free).
    let mut cooling = app.cooling.snapshot(snap.fan_curves).await;
    cooling.config = cfg.cooling.clone();
    let mut lighting = app
        .lighting
        .snapshot(&app.registry, &cfg, snap.placed_zones)
        .await;
    lighting.config = cfg.rgb.clone();
    let mut lcd = app
        .lcd
        .snapshot(&app.registry, snap.lcd_templates, snap.lcd_template_params)
        .await;
    lcd.config = cfg.lcd.clone();
    let observed_repo_signatures = app.repo_signature_status.lock().await.clone();
    let repo_compatibility = app.repo_compatibility_status.lock().await.clone();

    let wire = WireAppState {
        discovery: disc,
        devices: snap.devices,
        profiles: ProfileState {
            active: cfg.active_profile.clone(),
            available: cfg.profile_names(),
            app_rules: cfg.app_rules.clone(),
            overrides: cfg.profile_overrides(),
        },
        cooling,
        lighting,
        lcd,
        gui: cfg.gui.clone(),
        config_dir: crate::config::config_dir().display().to_string(),
        health: HealthCheckState {
            focus_watcher_supported: app.focus.supported(),
            ffmpeg_available: crate::lcd::engine::video::ffmpeg_available(),
        },
        process_icons,
        plugins: PluginsState {
            plugins: app.registry.list(app.secret_store.as_ref()),
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
                    signature: observed_repo_signatures
                        .get(&r.slug)
                        .filter(|(sha, _)| sha != &r.locked_sha)
                        .map(|(_, status)| status.clone())
                        .unwrap_or_else(|| active_repo_signature_status(r)),
                    compatibility: repo_compatibility
                        .get(&r.slug)
                        .cloned()
                        .unwrap_or(RepoCompatibilityStatus::Compatible),
                })
                .collect(),
            skipped: app.registry.skipped(),
            recommendations: app.registry.recommendations(),
        },
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

        let value = serialize_state(&app, cfg, HashMap::new()).await;
        let wire: halod_shared::types::AppState = serde_json::from_value(value).unwrap();
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
        let value = serialize_state(&app, cfg, HashMap::new()).await;
        let wire: halod_shared::types::AppState = serde_json::from_value(value).unwrap();
        assert_eq!(wire.devices.len(), 0);
        // The per-domain config is injected into each nested State struct.
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
        let value = serialize_state(&app, cfg, HashMap::new()).await;
        let wire: halod_shared::types::AppState = serde_json::from_value(value).unwrap();
        assert_eq!(wire.devices.len(), 1);
        assert_eq!(wire.devices[0].id, "test_device");
        assert_eq!(wire.devices[0].name, "Test Fan");
        assert_eq!(wire.devices[0].vendor, "Acme");
    }
}
