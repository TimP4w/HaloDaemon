// SPDX-License-Identifier: GPL-3.0-or-later
//! Projects plugin registry, repository, and runtime status into retained state.

#[cfg(test)]
use std::sync::Arc;

use crate::application::state::AppState;
use crate::config::{Config, PluginRepoRecord, PluginRepoSourceKind};
use halod_shared::types::{PluginRepoInfo, PluginsState, RepoSignatureStatus, WireDevice};

fn active_signature_status(record: &PluginRepoRecord) -> RepoSignatureStatus {
    let trust = if record.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG {
        super::repo::RepositoryTrust::Official
    } else if let Some(key) = &record.trusted_key {
        super::repo::RepositoryTrust::Pinned(key.clone())
    } else {
        return RepoSignatureStatus::Unsigned;
    };
    let result = if record.active_revision.is_some() {
        super::repo::verify_repository_signature(&super::repo::active_revision_dir(record), &trust)
    } else {
        return RepoSignatureStatus::Invalid {
            reason: "the repository has not been downloaded or has no active revision".to_owned(),
        };
    };
    result
        .map(|_| RepoSignatureStatus::Verified)
        .unwrap_or_else(|error| RepoSignatureStatus::Invalid {
            reason: format!("{error:#}"),
        })
}

pub async fn project(app: &AppState, cfg: &Config, devices: &[WireDevice]) -> PluginsState {
    let observed_signatures = app.repo_signature_status.lock().await.clone();
    let mut plugins = app.registry.list(app.secret_store.as_ref());
    for plugin in &mut plugins {
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
                        crate::application::bus::data_bus::SnapshotStatus::Fresh => "fresh",
                        crate::application::bus::data_bus::SnapshotStatus::Stale => "stale",
                        crate::application::bus::data_bus::SnapshotStatus::Unavailable => {
                            "unavailable"
                        }
                    }
                    .to_owned(),
                    updated_at: snapshot.published_at,
                },
            )
            .collect();
    }
    PluginsState {
        plugins,
        repos: cfg
            .plugins
            .repos
            .iter()
            .map(|record| PluginRepoInfo {
                url: record.url.clone(),
                slug: record.slug.clone(),
                repository_id: record.repository_id.clone(),
                release_tag: record.release_tag.clone(),
                release_policy: match &record.release_policy {
                    crate::config::PluginReleasePolicy::Latest => {
                        halod_shared::types::PluginReleasePolicy::Latest
                    }
                    crate::config::PluginReleasePolicy::Pinned(tag) => {
                        halod_shared::types::PluginReleasePolicy::Pinned(tag.clone())
                    }
                },
                active_revision: record.active_revision.clone(),
                previous_release_tag: record.previous_release_tag.clone(),
                last_sync: record.last_sync.clone(),
                official: record.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG,
                location: match record.source_kind {
                    PluginRepoSourceKind::Archive => {
                        halod_shared::types::PluginRepoLocation::LocalArchive
                    }
                    PluginRepoSourceKind::Release => {
                        halod_shared::types::PluginRepoLocation::RemoteRelease
                    }
                },
                signature: observed_signatures
                    .get(&record.slug)
                    .filter(|(tag, _)| Some(tag.as_str()) != record.release_tag.as_deref())
                    .map(|(_, status)| status.clone())
                    .unwrap_or_else(|| active_signature_status(record)),
                signing_key_fingerprint: record
                    .trusted_key
                    .as_ref()
                    .and_then(|key| halod_plugin_signing::signing_key_fingerprint(key).ok()),
            })
            .collect(),
        skipped: app.registry.skipped(),
        recommendations: app.registry.recommendations(),
        updates: app.plugin_update_status.lock().await.clone(),
        repo_updates: app.plugin_repo_update_status.lock().await.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_record(slug: &str) -> PluginRepoRecord {
        PluginRepoRecord {
            url: format!("https://example.com/{slug}.git"),
            slug: slug.to_owned(),
            repository_id: None,
            trusted_key: None,
            source_kind: PluginRepoSourceKind::Release,
            release_tag: None,
            release_policy: crate::config::PluginReleasePolicy::Latest,
            active_revision: None,
            active_source: crate::config::PluginRevisionSource::Managed,
            previous_release_tag: None,
            last_sync: None,
        }
    }

    #[test]
    fn third_party_repositories_are_unsigned() {
        let record = repo_record("community");
        assert_eq!(
            active_signature_status(&record),
            RepoSignatureStatus::Unsigned
        );
    }

    #[test]
    fn unavailable_official_repository_is_invalid() {
        let status =
            active_signature_status(&repo_record(crate::constants::OFFICIAL_PLUGIN_REPO_SLUG));
        assert!(matches!(status, RepoSignatureStatus::Invalid { .. }));
    }

    #[tokio::test]
    async fn rejected_remote_signature_is_projected() {
        let app = Arc::new(AppState::new(Config::default()));
        let mut record = repo_record(crate::constants::OFFICIAL_PLUGIN_REPO_SLUG);
        record.release_tag = Some("active".to_owned());
        let mut cfg = app.config.read().await.clone();
        cfg.plugins.repos.push(record);
        app.repo_signature_status.lock().await.insert(
            crate::constants::OFFICIAL_PLUGIN_REPO_SLUG.to_owned(),
            (
                "remote".to_owned(),
                RepoSignatureStatus::Invalid {
                    reason: "release.sig is missing".to_owned(),
                },
            ),
        );

        let projected = project(&app, &cfg, &[]).await;
        assert!(matches!(
            &projected.repos[0].signature,
            RepoSignatureStatus::Invalid { reason } if reason == "release.sig is missing"
        ));
    }
}
