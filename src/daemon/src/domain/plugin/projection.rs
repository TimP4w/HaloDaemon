// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;

use halod_plugin_signing::HOST_PLUGIN_API;
use halod_shared::types::{
    AvailablePlugin, PluginRepoInfo, PluginRepoLocation, PluginsState, RepoTrust, WireDevice,
};

use super::catalog::RepoCatalog;
use crate::application::bus::data_bus::SnapshotStatus;
use crate::application::state::AppState;
use crate::config::{Config, PluginRepoRecord, PluginRepoSource};
use crate::constants::OFFICIAL_PLUGIN_REPO_SLUG;

fn repo_info(record: &PluginRepoRecord, catalog: Option<&RepoCatalog>) -> PluginRepoInfo {
    let official = record.slug == OFFICIAL_PLUGIN_REPO_SLUG;
    let signing_key = catalog
        .and_then(|repo| repo.catalog.signing_key.as_ref())
        .or(record.trusted_key.as_ref());
    PluginRepoInfo {
        slug: record.slug.clone(),
        location: match &record.source {
            PluginRepoSource::Url(url) => PluginRepoLocation::Url(url.clone()),
            PluginRepoSource::Local(dir) => PluginRepoLocation::Local(dir.display().to_string()),
        },
        official,
        trust: match (official, signing_key) {
            (true, _) => RepoTrust::Official,
            (false, Some(_)) => RepoTrust::Signed,
            (false, None) => RepoTrust::Unsigned,
        },
        fingerprint: signing_key
            .and_then(|key| halod_plugin_signing::catalog::signing_key_fingerprint(key).ok()),
        last_sync: catalog.and_then(|repo| repo.last_sync.clone()),
        commit: catalog.and_then(|repo| repo.catalog.commit.clone()),
    }
}

fn available(catalogs: &HashMap<String, RepoCatalog>) -> Vec<AvailablePlugin> {
    let mut available: Vec<AvailablePlugin> = catalogs
        .iter()
        .flat_map(|(slug, repo)| {
            repo.catalog
                .select(HOST_PLUGIN_API)
                .into_values()
                .map(move |entry| AvailablePlugin {
                    repo: slug.clone(),
                    id: entry.id.clone(),
                    version: entry.version.to_string(),
                    api: entry.api.to_string(),
                    compatible: entry.api.is_compatible_with(HOST_PLUGIN_API),
                })
        })
        .collect();
    available.sort_by(|a, b| (&a.repo, &a.id).cmp(&(&b.repo, &b.id)));
    available
}

pub async fn project(app: &AppState, cfg: &Config, devices: &[WireDevice]) -> PluginsState {
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
                        SnapshotStatus::Fresh => "fresh",
                        SnapshotStatus::Stale => "stale",
                        SnapshotStatus::Unavailable => "unavailable",
                    }
                    .to_owned(),
                    updated_at: snapshot.published_at,
                },
            )
            .collect();
    }
    let catalogs = app.plugin_catalogs.lock().await;
    PluginsState {
        plugins,
        repos: cfg
            .plugins
            .repos
            .iter()
            .map(|record| repo_info(record, catalogs.get(&record.slug)))
            .collect(),
        available: available(&catalogs),
        skipped: app.registry.skipped(),
        recommendations: app.registry.recommendations(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_plugin_signing::{Catalog, CatalogPackage};

    fn entry(id: &str, version: &str, api: &str) -> CatalogPackage {
        CatalogPackage {
            id: id.into(),
            version: version.parse().unwrap(),
            api: api.parse().unwrap(),
            path: format!("packages/{id}-{version}.tar.gz"),
            sha256: "a".repeat(64),
            size: 1,
        }
    }

    fn repo(packages: Vec<CatalogPackage>) -> RepoCatalog {
        RepoCatalog {
            catalog: Catalog {
                schema: 1,
                name: "Test".into(),
                commit: Some("abc".into()),
                published: 1,
                signing_key: None,
                packages,
            },
            last_sync: None,
        }
    }

    #[test]
    fn available_lists_the_best_entry_per_repo_and_flags_incompatible_ones() {
        let catalogs = HashMap::from([
            (
                "official".to_owned(),
                repo(vec![
                    entry("demo", "1.0.0", "3.0"),
                    entry("demo", "2.0.0", "3.9"),
                ]),
            ),
            (
                "other".to_owned(),
                repo(vec![entry("future", "1.0.0", "9.0")]),
            ),
        ]);
        let listed = available(&catalogs);
        assert_eq!(listed.len(), 2);
        assert_eq!(
            (listed[0].version.as_str(), listed[0].compatible),
            ("1.0.0", true)
        );
        assert_eq!(
            (listed[1].id.as_str(), listed[1].compatible),
            ("future", false)
        );
    }

    #[test]
    fn repository_trust_follows_the_signing_key() {
        let record = PluginRepoRecord {
            slug: "community".into(),
            source: PluginRepoSource::Url("https://x/catalog.yaml".into()),
            trusted_key: None,
        };
        assert_eq!(repo_info(&record, None).trust, RepoTrust::Unsigned);
        let official = PluginRepoRecord {
            slug: crate::constants::OFFICIAL_PLUGIN_REPO_SLUG.into(),
            ..record
        };
        let info = repo_info(&official, Some(&repo(vec![])));
        assert_eq!(
            (info.trust, info.commit.as_deref()),
            (RepoTrust::Official, Some("abc"))
        );
    }
}
