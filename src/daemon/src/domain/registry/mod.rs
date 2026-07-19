// SPDX-License-Identifier: GPL-3.0-or-later
pub mod identity;
pub mod model;
pub mod observers;
pub mod state;
pub mod usecases;

use anyhow::Result;
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::device::Device;
use halod_shared::types::VisibilityState;

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

/// Initialize devices using an optional development checkout in place of the
/// managed official repository. The override is process-local and never
/// persists in config.
pub async fn initialize_app_state(
    app: Arc<AppState>,
    #[cfg(feature = "dev-plugin-repo")] dev_plugin_repo: Option<std::path::PathBuf>,
) {
    #[cfg(feature = "dev-plugin-repo")]
    if let Some(dir) = &dev_plugin_repo {
        log::warn!("Using development plugin repository at {}", dir.display());
    }
    ensure_official_repo(&app).await;
    #[cfg(not(feature = "dev-plugin-repo"))]
    let dev_plugin_repo: Option<std::path::PathBuf> = None;
    #[cfg(feature = "dev-plugin-repo")]
    {
        *app.development_plugin_repo.write().await = dev_plugin_repo.clone();
    }
    let mut discovered_hashes = Vec::new();
    {
        let cfg = app.config.read().await;
        let repo_sources = crate::domain::plugin::repo_plugin_sources(&cfg.plugins.repos);
        let official_dir = cfg
            .plugins
            .repos
            .iter()
            .find(|repo| repo.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG)
            .map(crate::domain::plugin::repo::active_revision_dir)
            .unwrap_or_else(|| {
                crate::config::plugin_repos_dir().join(crate::constants::OFFICIAL_PLUGIN_REPO_SLUG)
            });
        if official_dir.is_dir() {
            match crate::domain::plugin::repo::verify_official_repository(&official_dir) {
                Ok(manifest) => discovered_hashes.extend(
                    manifest
                        .packages
                        .into_iter()
                        .map(|package| (package.id, package.sha256)),
                ),
                Err(e) => {
                    log::warn!("official plugin repository failed validation: {e:#}");
                }
            }
        }
        app.registry.load_all_with_priority_repo(
            &crate::config::plugins_dir(),
            dev_plugin_repo.as_deref(),
            &repo_sources,
        );
        app.registry.replace_policy(&cfg.plugins);
    }
    if !discovered_hashes.is_empty() {
        let mut cfg = app.config.write().await;
        for (id, hash) in discovered_hashes {
            cfg.plugins.installed_hashes.entry(id).or_insert(hash);
        }
        app.registry.replace_policy(&cfg.plugins);
        app.request_config_save();
    }
    // Surface packages whose active revision differs from its installed digest.
    // This is dirty-update metadata, not a consent or activation gate, and it
    // runs without network access. The dev repo is not a config record, so it is
    // never a subject here.
    crate::domain::plugin::usecases::repos::quarantine_changed_plugins(app.clone()).await;
    start_update_check(app.clone()).await;
    observers::discovery::discover_devices(app.clone()).await;
    seed_known_devices(app.clone()).await;
    usecases::chain::restore_saved_chains(app.clone()).await;
    crate::domain::profiles::usecases::profiles::load_active_profile(app.clone()).await;
}

/// Seed the non-removable official plugin repo record if absent, and clone it
/// if its clone directory is missing. Official plugins still go through the
/// normal consent/permission flow — only the repo *record* is protected from
/// removal (see `usecases::repos::remove_repo`). A clone failure (e.g. no
/// network on first launch) is logged and must never fail boot: the daemon
/// simply has no official plugins until a later successful clone.
pub(crate) async fn ensure_official_repo(app: &Arc<AppState>) {
    ensure_official_repo_from(app, crate::constants::OFFICIAL_PLUGIN_REPO_URL).await;
}

/// [`ensure_official_repo`] parameterized on the URL to clone, so tests can
/// exercise the offline/clone-failure path against a local, non-network URL
/// instead of the real `constants::OFFICIAL_PLUGIN_REPO_URL`.
async fn ensure_official_repo_from(app: &Arc<AppState>, url: &str) {
    use crate::config::PluginRepoRecord;
    use crate::domain::plugin::repo;

    {
        let mut cfg = app.config.write().await;
        if !cfg
            .plugins
            .repos
            .iter()
            .any(|r| r.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG)
        {
            cfg.plugins.repos.push(PluginRepoRecord {
                url: url.to_owned(),
                slug: crate::constants::OFFICIAL_PLUGIN_REPO_SLUG.to_owned(),
                repository_id: None,
                trusted_key: None,
                source_kind: crate::config::PluginRepoSourceKind::Git,
                branch: None,
                locked_sha: String::new(),
                active_revision: None,
                active_source: crate::config::PluginRevisionSource::Managed,
                previous_verified_sha: None,
                last_sync: None,
            });
            app.request_config_save();
        }
    }

    install_embedded_official_bundle(app).await;

    // Contacting GitHub requires the user's consent (asked once on first run).
    // Until granted, keep the record but download nothing.
    if app.config.read().await.gui.plugin_downloads
        != halod_shared::types::PluginDownloadConsent::Allowed
    {
        return;
    }

    let dest = crate::config::plugin_repos_dir().join(crate::constants::OFFICIAL_PLUGIN_REPO_SLUG);
    let has_active_revision = app
        .config
        .read()
        .await
        .plugins
        .repos
        .iter()
        .find(|repo| repo.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG)
        .filter(|repo| repo.active_source == crate::config::PluginRevisionSource::Managed)
        .and_then(|repo| repo.active_revision.as_deref())
        .is_some_and(|revision| !revision.is_empty());
    if has_active_revision {
        return;
    }
    let url = url.to_owned();
    let dest2 = dest.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        // libgit2 may leave a partial checkout behind after an interrupted or
        // failed first clone. It has no trusted active revision, so retry from
        // a clean generated directory instead of repeatedly failing on it.
        if dest2.exists() {
            std::fs::remove_dir_all(&dest2)?;
        }
        repo::clone(&url, &dest2, None)
    })
    .await
    {
        Ok(Ok(sha)) => {
            let sha = {
                let dest = dest.clone();
                match tokio::task::spawn_blocking(move || {
                    repo::latest_compatible_revision(&dest, &sha, &repo::RepositoryTrust::Official)
                })
                .await
                {
                    Ok(Ok(Some(revision))) => revision.sha,
                    Ok(Ok(None)) => {
                        log::warn!(
                            "official plugin repository has no revision compatible with this Halo"
                        );
                        return;
                    }
                    Ok(Err(error)) => {
                        log::warn!("scanning official plugin repository history: {error:#}");
                        return;
                    }
                    Err(error) => {
                        log::warn!("official plugin repository history task panicked: {error:#}");
                        return;
                    }
                }
            };
            let revision = dest.join("revisions").join(&sha);
            let materialized = {
                let dest = dest.clone();
                let sha = sha.clone();
                let revision = revision.clone();
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    if !revision.is_dir() {
                        let staging = revision
                            .parent()
                            .expect("revision always has a parent")
                            .join(format!(".{sha}.staging-bootstrap"));
                        std::fs::create_dir_all(
                            staging.parent().expect("staging always has a parent"),
                        )?;
                        repo::materialize_commit(&dest, &sha, &staging)?;
                        repo::verify_official_repository(&staging)?;
                        std::fs::rename(&staging, &revision)?;
                    } else {
                        repo::verify_official_repository(&revision)?;
                    }
                    Ok(())
                })
                .await
            };
            match materialized {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    log::warn!("official plugin repository validation failed: {e:#}");
                    return;
                }
                Err(e) => {
                    log::warn!("official plugin revision materialization panicked: {e:#}");
                    return;
                }
            }
            let mut cfg = app.config.write().await;
            if let Some(r) = cfg
                .plugins
                .repos
                .iter_mut()
                .find(|r| r.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG)
            {
                r.locked_sha = sha;
                r.active_revision = Some(r.locked_sha.clone());
                r.active_source = crate::config::PluginRevisionSource::Managed;
                r.previous_verified_sha = None;
                r.last_sync = Some(chrono::Utc::now().to_rfc3339());
            }
            app.request_config_save();
        }
        Ok(Err(e)) => {
            log::warn!("official plugin repo clone failed (continuing with none): {e:#}");
        }
        Err(e) => log::warn!("official plugin repo clone task panicked: {e:#}"),
    }
}

/// Materialize the release-supplied signed plugin archive before the optional
/// network checkout. It intentionally lives outside the managed Git root so a
/// first online update can still clone into that root normally.
async fn install_embedded_official_bundle(app: &Arc<AppState>) {
    let Some(bytes) = crate::embedded_plugins::BUNDLE else {
        return;
    };
    let root = crate::config::embedded_plugin_revisions_dir()
        .join(crate::constants::OFFICIAL_PLUGIN_REPO_SLUG);
    let staging = root.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<(String, String)> {
        let metadata = halod_plugin_signing::extract_bundle(bytes, &staging)?;
        let revision = root.join(&metadata.commit);
        if revision.is_dir() {
            std::fs::remove_dir_all(&staging)?;
        } else {
            crate::domain::plugin::repo::verify_official_repository(&staging)?;
            std::fs::create_dir_all(&root)?;
            std::fs::rename(&staging, &revision)?;
        }
        Ok((metadata.commit, metadata.repository_id))
    })
    .await;
    match result {
        Ok(Ok((commit, repository_id))) => {
            let mut cfg = app.config.write().await;
            if let Some(record) = cfg
                .plugins
                .repos
                .iter_mut()
                .find(|r| r.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG)
            {
                if record.active_revision.is_none()
                    || record.active_source == crate::config::PluginRevisionSource::Embedded
                {
                    record.locked_sha = commit.clone();
                    record.active_revision = Some(commit);
                    record.active_source = crate::config::PluginRevisionSource::Embedded;
                    record.repository_id = Some(repository_id);
                    app.request_config_save();
                }
            }
        }
        Ok(Err(error)) => {
            log::warn!("embedded official plugin bundle failed validation: {error:#}")
        }
        Err(error) => log::warn!("embedded official plugin bundle task panicked: {error}"),
    }
}

/// Kick off a background plugin-update check (repo- and per-plugin), flagging
/// the discovery status so the radar can show a "checking for updates" step.
/// A no-op unless the user has allowed GitHub access; spawned so it never
/// blocks device discovery or boot.
pub(crate) async fn start_update_check(app: Arc<AppState>) {
    if app.config.read().await.gui.plugin_downloads
        != halod_shared::types::PluginDownloadConsent::Allowed
    {
        return;
    }
    app.discovery.lock().await.checking_updates = true;
    crate::domain::registry::usecases::runtime::topology_changed(&app).await;
    tokio::spawn(async move {
        crate::domain::plugin::usecases::repos::check_updates_broadcast(app.clone()).await;
        app.discovery.lock().await.checking_updates = false;
        crate::domain::registry::usecases::runtime::topology_changed(&app).await;
    });
}

#[cfg(test)]
mod guard_tests {
    use super::require_device_owned_id;
    use crate::application::state::AppState;
    use crate::config::Config;
    use crate::domain::device::Device;
    use crate::test_support::MockDevice;
    use halod_shared::types::VisibilityState;
    use std::sync::Arc;

    #[tokio::test]
    async fn require_device_owned_id_rejects_disabled_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        dev.set_active_state(VisibilityState::Disabled);
        app.device_registry.write().await.push(dev);

        let result = require_device_owned_id("dev1", &app).await;
        let msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            msg.contains("disabled"),
            "expected disabled error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn require_device_owned_id_allows_visible_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        app.device_registry.write().await.push(dev);

        assert!(require_device_owned_id("dev1", &app).await.is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use async_trait::async_trait;

    struct StubDevice {
        id: &'static str,
    }

    #[async_trait]
    impl Device for StubDevice {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            "stub"
        }
        fn vendor(&self) -> &str {
            "stub"
        }
        fn model(&self) -> &str {
            "stub"
        }
        async fn initialize(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<crate::domain::device::CapabilityRef<'_>> {
            vec![]
        }
    }

    async fn app_with_devices(ids: &[&'static str]) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        let mut devices = app.device_registry.write().await;
        for id in ids {
            devices.push(Arc::new(StubDevice { id }) as Arc<dyn Device>);
        }
        drop(devices);
        app
    }

    #[tokio::test]
    async fn require_device_owned_id_returns_matching_device() {
        let app = app_with_devices(&["dev_a", "dev_b"]).await;
        let found = require_device_owned_id("dev_b", &app).await.unwrap();
        assert_eq!(found.id(), "dev_b");
    }

    #[tokio::test]
    async fn require_device_owned_id_errors_when_not_found() {
        let app = app_with_devices(&["dev_a"]).await;
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
    async fn seeds_the_record_even_when_the_clone_url_is_unreachable() {
        crate::test_support::with_tmp_config(|app| async move {
            // Cloning only happens once the user has allowed downloads.
            app.config.write().await.gui.plugin_downloads =
                halod_shared::types::PluginDownloadConsent::Allowed;
            // A local nonexistent path fails fast (no network hang) while still
            // exercising the exact "clone failed" path a bad/offline URL hits.
            ensure_official_repo_from(&app, "/nonexistent/not-a-git-repo").await;

            let cfg = app.config.read().await;
            let record = cfg
                .plugins
                .repos
                .iter()
                .find(|r| r.slug == OFFICIAL_SLUG)
                .expect("official repo record must be seeded even if the clone fails");
            assert!(
                record.locked_sha.is_empty(),
                "a failed clone must not fabricate a locked_sha"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn seeds_the_record_but_downloads_nothing_without_consent() {
        crate::test_support::with_tmp_config(|app| async move {
            // Default consent is `Unset`: the record is seeded but no clone is
            // attempted, so the clone directory must never appear.
            ensure_official_repo_from(&app, "/nonexistent/not-a-git-repo").await;

            let cfg = app.config.read().await;
            assert!(
                cfg.plugins.repos.iter().any(|r| r.slug == OFFICIAL_SLUG),
                "the official record is seeded regardless of consent"
            );
            let clone_dir = crate::config::plugin_repos_dir().join(OFFICIAL_SLUG);
            assert!(
                !clone_dir.exists(),
                "no download may happen before the user consents"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn is_idempotent_and_does_not_duplicate_the_record() {
        crate::test_support::with_tmp_config(|app| async move {
            ensure_official_repo_from(&app, "/nonexistent/not-a-git-repo").await;
            ensure_official_repo_from(&app, "/nonexistent/not-a-git-repo").await;

            let cfg = app.config.read().await;
            let count = cfg
                .plugins
                .repos
                .iter()
                .filter(|r| r.slug == OFFICIAL_SLUG)
                .count();
            assert_eq!(count, 1, "re-running must not duplicate the seeded record");
        })
        .await;
    }
}
