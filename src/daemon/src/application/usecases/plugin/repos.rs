// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};
use halod_plugin_signing::package::validate_identifier;
use halod_plugin_signing::{CatalogError, CatalogPackage, HOST_PLUGIN_API};
use halod_shared::types::PluginRepoLocation;

use crate::application::state::AppState;
use crate::config::{PluginRepoRecord, PluginRepoSource};
use crate::domain::events::{Change, ChangeSink as _};
use crate::domain::plugin::catalog::{self, RepoCatalog};
use crate::domain::plugin::repo::RepositoryTrust;
use crate::util::backoff::{schedule, Retry};

use super::plugins::{purge_plugin_state, reconcile_plugins, reload_registry};
use crate::application::usecases::registry::runtime;
use crate::constants::OFFICIAL_PLUGIN_REPO_SLUG;

const REFRESH_ATTEMPTS: u32 = 4;

fn slug_for(text: &str) -> String {
    let mut slug = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(64).collect();
    if slug.is_empty() {
        "repo".to_owned()
    } else {
        slug
    }
}

fn url_of(record: &PluginRepoRecord) -> Option<&str> {
    match &record.source {
        PluginRepoSource::Url(url) => Some(url),
        PluginRepoSource::Local(_) => None,
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .context("plugin repository task panicked")?
}

async fn record(app: &AppState, slug: &str) -> Result<PluginRepoRecord> {
    app.config
        .read()
        .await
        .plugins
        .repos
        .iter()
        .find(|record| record.slug == slug)
        .cloned()
        .ok_or_else(|| anyhow!("unknown plugin repository '{slug}'"))
}

async fn remember(app: &AppState, slug: &str, catalog: halod_plugin_signing::Catalog) {
    app.plugin_catalogs.lock().await.insert(
        slug.to_owned(),
        RepoCatalog {
            catalog,
            last_sync: Some(chrono::Utc::now().to_rfc3339()),
        },
    );
}

pub async fn add_repo(source: PluginRepoLocation, app: Arc<AppState>) -> Result<()> {
    let (slug, source) = match source {
        PluginRepoLocation::Url(url) => {
            halod_plugin_signing::fetch::https_url(&url)?;
            let name = url
                .trim_start_matches("https://")
                .trim_end_matches("/catalog.yaml");
            (slug_for(name), PluginRepoSource::Url(url))
        }
        PluginRepoLocation::Local(path) => {
            let dir = std::fs::canonicalize(&path).with_context(|| format!("opening {path}"))?;
            if !dir.is_dir() {
                bail!("{} is not a directory", dir.display());
            }
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("folder");
            (
                slug_for(&format!("local-{name}")),
                PluginRepoSource::Local(dir),
            )
        }
    };
    {
        let cfg = app.config.read().await;
        if slug == OFFICIAL_PLUGIN_REPO_SLUG
            || cfg
                .plugins
                .repos
                .iter()
                .any(|record| record.slug == slug || record.source == source)
        {
            bail!("this plugin repository is already added");
        }
    }
    let mut record = PluginRepoRecord {
        slug: slug.clone(),
        source,
        trusted_key: None,
    };
    if let Some(url) = url_of(&record).map(str::to_owned) {
        let fetch_slug = slug.clone();
        let catalog =
            blocking(move || catalog::fetch(&fetch_slug, &url, &RepositoryTrust::FirstUse)).await?;
        record.trusted_key = catalog.signing_key.clone();
        remember(&app, &slug, catalog).await;
    }
    let local = url_of(&record).is_none();
    app.config.write().await.plugins.repos.push(record);
    app.request_config_save();
    if local {
        reload_registry(&app).await;
        app.record_change(Change::PluginTopology).await;
    } else {
        app.record_change(Change::PluginData).await;
    }
    Ok(())
}

pub async fn remove_repo(slug: String, app: Arc<AppState>) -> Result<()> {
    if slug == OFFICIAL_PLUGIN_REPO_SLUG {
        bail!("the official plugin repository cannot be removed");
    }
    let record = record(&app, &slug).await?;
    let ids = app.registry.plugins_from_repo(&slug);
    for id in &ids {
        purge_plugin_state(id, &app).await;
    }
    if url_of(&record).is_some() {
        let dir = catalog::repo_dir(&slug);
        blocking(move || remove_dir(&dir)).await?;
    }
    app.config
        .write()
        .await
        .plugins
        .repos
        .retain(|r| r.slug != slug);
    app.request_config_save();
    app.plugin_catalogs.lock().await.remove(&slug);
    app.repo_refresh_retry.clear(&slug);
    if ids.is_empty() {
        reload_registry(&app).await;
        app.record_change(Change::PluginData).await;
    } else {
        reconcile_plugins(&app, &ids).await;
    }
    Ok(())
}

async fn refresh_one(app: &Arc<AppState>, slug: &str) -> Result<()> {
    let record = record(app, slug).await?;
    let Some(url) = url_of(&record).map(str::to_owned) else {
        return Ok(());
    };
    let trust = RepositoryTrust::for_record(&record);
    let fetch_slug = slug.to_owned();
    let catalog = blocking(move || catalog::fetch(&fetch_slug, &url, &trust)).await?;
    if record.trusted_key.is_none() && catalog.signing_key.is_some() {
        let mut cfg = app.config.write().await;
        if let Some(stored) = cfg.plugins.repos.iter_mut().find(|r| r.slug == slug) {
            stored.trusted_key = catalog.signing_key.clone();
        }
        app.request_config_save();
    }
    remember(app, slug, catalog).await;
    Ok(())
}

async fn remote_slugs(app: &AppState) -> Vec<String> {
    app.config
        .read()
        .await
        .plugins
        .repos
        .iter()
        .filter(|record| url_of(record).is_some())
        .map(|record| record.slug.clone())
        .collect()
}

pub async fn refresh_repos(app: Arc<AppState>) -> Result<()> {
    let mut failures = Vec::new();
    for slug in remote_slugs(&app).await {
        match refresh_one(&app, &slug).await {
            Ok(()) => app.repo_refresh_retry.clear(&slug),
            Err(error) => failures.push(format!("{slug}: {error:#}")),
        }
    }
    app.record_change(Change::PluginData).await;
    if !failures.is_empty() {
        bail!(
            "refreshing plugin repositories failed: {}",
            failures.join("; ")
        );
    }
    Ok(())
}

pub fn refresh_in_background(app: Arc<AppState>) {
    tokio::spawn(async move {
        app.discovery.lock().await.checking_updates = true;
        runtime::topology_changed(&app).await;
        for slug in remote_slugs(&app).await {
            if let Err(error) = refresh_one(&app, &slug).await {
                log::warn!("refreshing plugin repository '{slug}': {error:#}");
                let retry_app = Arc::clone(&app);
                let retry_slug = slug.clone();
                schedule(
                    &app.repo_refresh_retry,
                    &slug,
                    REFRESH_ATTEMPTS,
                    move |_| {
                        let app = Arc::clone(&retry_app);
                        let slug = retry_slug.clone();
                        async move {
                            match refresh_one(&app, &slug).await {
                                Ok(()) => {
                                    app.record_change(Change::PluginData).await;
                                    Retry::Stop
                                }
                                Err(_) => Retry::Again,
                            }
                        }
                    },
                );
            }
        }
        app.discovery.lock().await.checking_updates = false;
        runtime::topology_changed(&app).await;
        app.record_change(Change::PluginData).await;
    });
}

async fn selected(app: &Arc<AppState>, slug: &str, id: &str) -> Result<CatalogPackage> {
    if !app.plugin_catalogs.lock().await.contains_key(slug) {
        refresh_one(app, slug).await?;
    }
    let catalogs = app.plugin_catalogs.lock().await;
    let entry = catalogs
        .get(slug)
        .and_then(|repo| {
            repo.catalog
                .select(HOST_PLUGIN_API)
                .get(id)
                .copied()
                .cloned()
        })
        .ok_or_else(|| anyhow!("'{slug}' does not provide plugin '{id}'"))?;
    if !entry.api.is_compatible_with(HOST_PLUGIN_API) {
        return Err(CatalogError::ApiIncompatible {
            required: entry.api,
            supported: HOST_PLUGIN_API,
        }
        .into());
    }
    Ok(entry)
}

pub async fn install_plugin(repo: String, id: String, app: Arc<AppState>) -> Result<()> {
    let record = record(&app, &repo).await?;
    let url = url_of(&record)
        .ok_or_else(|| anyhow!("plugins in a local folder are not installed"))?
        .to_owned();
    let entry = selected(&app, &repo, &id).await?;
    let stage_slug = repo.clone();
    let staging = blocking(move || catalog::stage(&stage_slug, &url, &entry)).await?;
    install_staged(&app, &repo, &id, staging).await
}

async fn install_staged(app: &Arc<AppState>, repo: &str, id: &str, staging: PathBuf) -> Result<()> {
    let mut copies: Vec<(PathBuf, PathBuf)> = remote_slugs(app)
        .await
        .into_iter()
        .filter(|slug| *slug != repo)
        .map(|slug| {
            let dir = catalog::repo_dir(&slug);
            (dir.join(id), dir.join(format!(".{id}.old")))
        })
        .collect();
    let target = catalog::repo_dir(repo).join(id);
    let backup = catalog::repo_dir(repo).join(format!(".{id}.old"));
    copies.insert(0, (target.clone(), backup));
    let swap_copies = copies.clone();
    blocking(move || {
        swap_in(&staging, &swap_copies[0].0, &swap_copies[0].1)?;
        swap_copies[1..]
            .iter()
            .try_for_each(|(dir, backup)| set_aside(dir, backup))
    })
    .await?;

    reload_registry(app).await;
    if let Some(problem) = app.registry.load_problem(&target) {
        blocking(move || {
            copies
                .iter()
                .try_for_each(|(dir, backup)| swap_back(dir, backup))
        })
        .await?;
        reload_registry(app).await;
        reconcile_plugins(app, &[id.to_owned()]).await;
        bail!("{id} failed to load: {problem}");
    }
    blocking(move || copies.iter().try_for_each(|(_, backup)| remove_dir(backup))).await?;
    reconcile_plugins(app, &[id.to_owned()]).await;
    Ok(())
}

pub async fn uninstall_plugin(id: String, app: Arc<AppState>) -> Result<()> {
    validate_identifier("plugin id", &id)?;
    let dirs: Vec<PathBuf> = remote_slugs(&app)
        .await
        .into_iter()
        .map(|slug| catalog::repo_dir(&slug).join(&id))
        .filter(|dir| dir.is_dir())
        .collect();
    if dirs.is_empty() {
        bail!("plugin '{id}' is not installed");
    }
    purge_plugin_state(&id, &app).await;
    blocking(move || dirs.iter().try_for_each(|dir| remove_dir(dir))).await?;
    reconcile_plugins(&app, std::slice::from_ref(&id)).await;
    Ok(())
}

pub async fn update_all_plugins(app: Arc<AppState>) -> Result<()> {
    let updates: Vec<(String, String)> = {
        let catalogs = app.plugin_catalogs.lock().await;
        app.registry
            .installed()
            .into_iter()
            .filter(|(slug, id, version)| {
                catalogs.get(slug).is_some_and(|repo| {
                    repo.catalog
                        .select(HOST_PLUGIN_API)
                        .get(id.as_str())
                        .is_some_and(|entry| {
                            entry.api.is_compatible_with(HOST_PLUGIN_API)
                                && entry.version > *version
                        })
                })
            })
            .map(|(slug, id, _)| (slug, id))
            .collect()
    };
    let mut failures = Vec::new();
    for (slug, id) in updates {
        if let Err(error) = install_plugin(slug, id.clone(), Arc::clone(&app)).await {
            failures.push(format!("{id}: {error:#}"));
        }
    }
    if !failures.is_empty() {
        bail!("updating plugins failed: {}", failures.join("; "));
    }
    Ok(())
}

fn swap_in(staging: &Path, target: &Path, backup: &Path) -> Result<()> {
    remove_dir(backup)?;
    if target.exists() {
        std::fs::rename(target, backup)?;
    }
    if let Err(error) = std::fs::rename(staging, target) {
        if backup.exists() {
            std::fs::rename(backup, target)?;
        }
        let _ = std::fs::remove_dir_all(staging);
        return Err(error).with_context(|| format!("installing into {}", target.display()));
    }
    Ok(())
}

fn set_aside(dir: &Path, backup: &Path) -> Result<()> {
    remove_dir(backup)?;
    if dir.exists() {
        std::fs::rename(dir, backup)?;
    }
    Ok(())
}

fn swap_back(target: &Path, backup: &Path) -> Result<()> {
    remove_dir(target)?;
    if backup.exists() {
        std::fs::rename(backup, target)?;
    }
    Ok(())
}

fn remove_dir(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_lowercase_path_safe_and_bounded() {
        assert_eq!(
            slug_for("TimP4w.github.io/HaloDaemon-plugins"),
            "timp4w-github-io-halodaemon-plugins"
        );
        assert_eq!(slug_for("../.."), "repo");
        assert!(slug_for(&"a".repeat(200)).len() <= 64);
    }

    async fn with_remote_repo(app: &AppState, slug: &str) {
        app.config
            .write()
            .await
            .plugins
            .repos
            .push(PluginRepoRecord {
                slug: slug.to_owned(),
                source: PluginRepoSource::Url(format!("https://{slug}.example/catalog.yaml")),
                trusted_key: None,
            });
    }

    fn installed_from(app: &AppState, id: &str) -> Option<String> {
        app.registry
            .installed()
            .into_iter()
            .find(|(_, installed, _)| installed == id)
            .map(|(slug, _, _)| slug)
    }

    fn staged(slug: &str, id: &str, version: &str) -> PathBuf {
        let staging = catalog::repo_dir(slug).join(format!(".{id}.staging-{version}"));
        crate::test_support::write_plugin_package(&staging, id, version, "3.0");
        staging.join(id)
    }

    #[tokio::test]
    async fn uninstall_rejects_ids_that_leave_the_plugin_store() {
        crate::test_support::with_tmp_config(|app| async move {
            with_remote_repo(&app, "official").await;
            with_remote_repo(&app, "community").await;
            let kept = [
                catalog::repo_dir("official").join("demo"),
                crate::config::plugins_dir().join("x"),
            ];
            for dir in &kept {
                std::fs::create_dir_all(dir).unwrap();
            }
            for id in ["../x", "../../official", "../official", "", "."] {
                assert!(uninstall_plugin(id.to_owned(), Arc::clone(&app))
                    .await
                    .is_err());
            }
            assert!(kept.iter().all(|dir| dir.is_dir()));
            assert!(catalog::repo_dir("official").is_dir());
        })
        .await;
    }

    fn installed_version(app: &AppState, id: &str) -> Option<String> {
        app.registry
            .installed()
            .into_iter()
            .find(|(_, installed, _)| installed == id)
            .map(|(_, _, version)| version.to_string())
    }

    #[tokio::test]
    async fn installing_from_another_repo_moves_the_plugin_there() {
        crate::test_support::with_tmp_config(|app| async move {
            with_remote_repo(&app, "official").await;
            with_remote_repo(&app, "community").await;
            install_staged(
                &app,
                "official",
                "demo",
                staged("official", "demo", "1.0.0"),
            )
            .await
            .unwrap();
            install_staged(
                &app,
                "community",
                "demo",
                staged("community", "demo", "2.0.0"),
            )
            .await
            .unwrap();
            assert_eq!(installed_from(&app, "demo").as_deref(), Some("community"));
            assert_eq!(installed_version(&app, "demo").as_deref(), Some("2.0.0"));
            assert!(!catalog::repo_dir("official").join("demo").exists());
            assert!(!catalog::repo_dir("official").join(".demo.old").exists());
        })
        .await;
    }

    #[tokio::test]
    async fn first_install_leaves_the_plugin_disabled_until_confirmed() {
        crate::test_support::with_tmp_config(|app| async move {
            with_remote_repo(&app, "official").await;
            install_staged(
                &app,
                "official",
                "demo",
                staged("official", "demo", "1.0.0"),
            )
            .await
            .unwrap();
            assert_eq!(installed_from(&app, "demo").as_deref(), Some("official"));
            assert!(!app
                .config
                .read()
                .await
                .plugins
                .enabled
                .contains(&"demo".to_owned()));
        })
        .await;
    }

    fn dir_with(path: &Path, marker: &str) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("marker"), marker).unwrap();
    }

    fn marker(path: &Path) -> String {
        std::fs::read_to_string(path.join("marker")).unwrap()
    }

    #[test]
    fn swap_replaces_the_target_and_keeps_a_backup() {
        let root = tempfile::tempdir().unwrap();
        let (staging, target, backup) = (
            root.path().join(".demo.staging"),
            root.path().join("demo"),
            root.path().join(".demo.old"),
        );
        dir_with(&target, "old");
        dir_with(&staging, "new");

        swap_in(&staging, &target, &backup).unwrap();
        assert_eq!(marker(&target), "new");
        assert_eq!(marker(&backup), "old");
        assert!(!staging.exists());

        swap_back(&target, &backup).unwrap();
        assert_eq!(marker(&target), "old");
        assert!(!backup.exists());
    }

    #[test]
    fn a_failed_swap_leaves_the_old_install_in_place() {
        let root = tempfile::tempdir().unwrap();
        let (target, backup) = (root.path().join("demo"), root.path().join(".demo.old"));
        dir_with(&target, "old");

        assert!(swap_in(&root.path().join("missing"), &target, &backup).is_err());
        assert_eq!(marker(&target), "old");
        assert!(!backup.exists());
    }
}
