// SPDX-License-Identifier: GPL-3.0-or-later
pub mod identity;
pub mod model;
pub mod observers;
pub mod state;

use anyhow::Result;
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::device::Device;
use halod_shared::types::VisibilityState;

use crate::application::usecases::profiles::lifecycle;
use crate::application::usecases::registry::chain;
use crate::domain::plugin::catalog::RepoCatalog;
use crate::domain::plugin::repo::RepositoryTrust;
pub use state::{HidTracking, HidTrackingEntry};

/// Look up the device with the given `id` and return an owned clone, holding the
/// `devices` lock only for the lookup. Error if the device is not found or is
/// disabled — every device-action usecase routes through here, so a disabled
/// device rejects all actions. Visibility changes use a raw lookup instead.
pub async fn require_device_owned_id(id: &str, app: &AppState) -> Result<Arc<dyn Device>> {
    let device = app
        .find_device_by_id(id)
        .await
        .ok_or_else(|| anyhow::anyhow!("device not found: {id}"))?;
    if device.active_state() == VisibilityState::Disabled {
        anyhow::bail!("device is disabled: {id}");
    }
    Ok(device)
}

pub async fn seed_known_devices(app: Arc<AppState>) {
    let devices = app.device_registry.read().await.clone();
    let mut cfg = app.config.write().await;
    for device in &devices {
        model::ensure_record(&mut cfg.known_devices, device.id(), Some(device.as_ref()));
    }
}

pub async fn initialize_app_state(app: Arc<AppState>) {
    ensure_official_repo(&app).await;
    let repos = app.config.read().await.plugins.repos.clone();
    let cached = tokio::task::spawn_blocking(move || {
        if let Err(error) =
            crate::domain::plugin::catalog::remove_leftovers(&crate::config::plugins_dir())
        {
            log::warn!("removing interrupted plugin installs: {error:#}");
        }
        repos
            .iter()
            .filter(|record| matches!(record.source, crate::config::PluginRepoSource::Url(_)))
            .filter_map(|record| {
                let trust = RepositoryTrust::for_record(record);
                let catalog = crate::domain::plugin::catalog::cached(&record.slug, &trust)?;
                Some((
                    record.slug.clone(),
                    RepoCatalog {
                        catalog,
                        last_sync: None,
                    },
                ))
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    app.plugin_catalogs.lock().await.extend(cached);
    {
        let cfg = app.config.read().await;
        app.registry.load_all(&cfg.plugins.repos);
        app.registry.replace_policy(&cfg.plugins);
    }
    crate::application::usecases::plugin::repos::refresh_in_background(app.clone());
    observers::discovery::discover_devices(app.clone()).await;
    seed_known_devices(app.clone()).await;
    chain::restore_saved_chains(app.clone()).await;
    lifecycle::load_active_profile(app.clone()).await;
}

/// Seed the non-removable official repository record, pointing it at the
/// current official catalog URL.
pub(crate) async fn ensure_official_repo(app: &Arc<AppState>) {
    use crate::config::{PluginRepoRecord, PluginRepoSource};
    use crate::constants::{OFFICIAL_PLUGIN_CATALOG_URL, OFFICIAL_PLUGIN_REPO_SLUG};

    let source = PluginRepoSource::Url(OFFICIAL_PLUGIN_CATALOG_URL.to_owned());
    let mut cfg = app.config.write().await;
    match cfg
        .plugins
        .repos
        .iter_mut()
        .find(|record| record.slug == OFFICIAL_PLUGIN_REPO_SLUG)
    {
        Some(record) if record.source == source => return,
        Some(record) => record.source = source,
        None => cfg.plugins.repos.insert(
            0,
            PluginRepoRecord {
                slug: OFFICIAL_PLUGIN_REPO_SLUG.to_owned(),
                source,
                trusted_key: None,
            },
        ),
    }
    app.request_config_save();
}

#[cfg(test)]
mod guard_tests {
    use super::require_device_owned_id;
    use crate::domain::device::Device;
    use crate::test_support::{app_with, MockDevice};
    use halod_shared::types::VisibilityState;
    use std::sync::Arc;

    #[tokio::test]
    async fn require_device_owned_id_rejects_disabled_device() {
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        dev.set_active_state(VisibilityState::Disabled);
        let app = app_with(vec![dev]);

        let result = require_device_owned_id("dev1", &app).await;
        let msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            msg.contains("disabled"),
            "expected disabled error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn require_device_owned_id_allows_visible_device() {
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = app_with(vec![dev]);

        assert!(require_device_owned_id("dev1", &app).await.is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_with, MockDevice};

    fn app_with_devices(ids: &[&str]) -> Arc<AppState> {
        app_with(
            ids.iter()
                .map(|id| Arc::new(MockDevice::new(id)) as _)
                .collect(),
        )
    }

    #[tokio::test]
    async fn require_device_owned_id_returns_matching_device() {
        let app = app_with_devices(&["dev_a", "dev_b"]);
        let found = require_device_owned_id("dev_b", &app).await.unwrap();
        assert_eq!(found.id(), "dev_b");
    }

    #[tokio::test]
    async fn require_device_owned_id_errors_when_not_found() {
        let app = app_with_devices(&["dev_a"]);
        let err = require_device_owned_id("dev_z", &app)
            .await
            .err()
            .expect("expected error");
        assert!(err.to_string().contains("dev_z"));
    }
}

#[cfg(test)]
mod official_repo_tests {
    use super::*;
    use crate::constants::OFFICIAL_PLUGIN_REPO_SLUG as OFFICIAL_SLUG;

    #[tokio::test]
    async fn seeding_is_idempotent_and_repairs_the_url() {
        crate::test_support::with_tmp_config(|app| async move {
            ensure_official_repo(&app).await;
            app.config.write().await.plugins.repos[0].source =
                crate::config::PluginRepoSource::Url("https://stale.example/catalog.yaml".into());
            ensure_official_repo(&app).await;

            let cfg = app.config.read().await;
            let official: Vec<_> = cfg
                .plugins
                .repos
                .iter()
                .filter(|r| r.slug == OFFICIAL_SLUG)
                .collect();
            assert_eq!(official.len(), 1);
            assert_eq!(
                official[0].source,
                crate::config::PluginRepoSource::Url(
                    crate::constants::OFFICIAL_PLUGIN_CATALOG_URL.into()
                )
            );
        })
        .await;
    }
}
