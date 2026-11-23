// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};
use halod_plugin_signing::declarations::{read_declarations, PackageDeclarations};
use halod_plugin_signing::{CatalogError, CatalogPackage, HOST_PLUGIN_API};
use halod_shared::types::{NotificationCode, PluginRepoLocation, UpdateScope};

use crate::application::notifications;
use crate::application::state::AppState;
use crate::config::{PluginRepoRecord, PluginRepoSource};
use crate::domain::events::{Change, ChangeSink as _};
use crate::domain::plugin::catalog::{self, RepoCatalog};
use crate::domain::plugin::repo::RepositoryTrust;
use crate::util::backoff::{schedule, Retry};

use super::plugins;
use super::plugins::{
    purge_plugin_state, reconcile_plugins, refresh_recommendations, reload_registry,
};
use crate::application::usecases::registry::runtime;
use crate::constants::OFFICIAL_PLUGIN_REPO_SLUG;
use halod_shared::ids::PluginId;

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

async fn record_of(app: &AppState, slug: &str) -> Result<PluginRepoRecord> {
    record(app, slug).await
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

async fn remember(app: &Arc<AppState>, slug: &str, catalog: halod_plugin_signing::Catalog) {
    app.plugin_catalogs.lock().await.insert(
        slug.to_owned(),
        RepoCatalog {
            catalog,
            last_sync: Some(jiff::Timestamp::now().to_string()),
        },
    );
    refresh_recommendations(app).await;
}

pub async fn add_repo(source: PluginRepoLocation, app: Arc<AppState>) -> Result<()> {
    let _store = app.plugin_store.lock().await;
    let (slug, source) = match source {
        PluginRepoLocation::Url(url) => {
            halod_plugin_signing::fetch::https_url(&url)?;
            let name = url
                .trim_start_matches("https://")
                .trim_end_matches("/catalog.yaml");
            (slug_for(name), PluginRepoSource::Url(url))
        }
        PluginRepoLocation::Local(path) => {
            let dir = blocking(move || {
                let dir =
                    std::fs::canonicalize(&path).with_context(|| format!("opening {path}"))?;
                if !dir.is_dir() {
                    bail!("{} is not a directory", dir.display());
                }
                Ok(dir)
            })
            .await?;
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
    let _store = app.plugin_store.lock().await;
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
    let bytes = blocking(move || catalog::download(&url)).await?;
    let _store = app.plugin_store.lock().await;
    let Ok(record) = record_of(app, slug).await else {
        return Ok(());
    };
    let trust = RepositoryTrust::for_record(&record);
    let accept_slug = slug.to_owned();
    let catalog = blocking(move || catalog::accept(&accept_slug, &bytes, &trust)).await?;
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

async fn selected(app: &Arc<AppState>, slug: &str, id: &PluginId) -> Result<CatalogPackage> {
    if !app.plugin_catalogs.lock().await.contains_key(slug) {
        refresh_one(app, slug).await?;
    }
    let catalogs = app.plugin_catalogs.lock().await;
    let entry = catalogs
        .get(slug)
        .and_then(|repo| {
            repo.catalog
                .select(HOST_PLUGIN_API)
                .get(id.as_str())
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

pub async fn install_plugin(
    repo: String,
    id: PluginId,
    permissions_confirmed: bool,
    app: Arc<AppState>,
) -> Result<()> {
    let staging = fetch_and_stage(&app, &repo, &id).await?;
    install_staged(&app, &repo, &id, staging, permissions_confirmed).await
}

async fn fetch_and_stage(app: &Arc<AppState>, repo: &str, id: &PluginId) -> Result<PathBuf> {
    let record = record(app, repo).await?;
    let url = url_of(&record)
        .ok_or_else(|| anyhow!("plugins in a local folder are not installed"))?
        .to_owned();
    let entry = selected(app, repo, id).await?;
    let stage_slug = repo.to_owned();
    blocking(move || catalog::stage(&stage_slug, &url, &entry)).await
}

async fn install_staged(
    app: &Arc<AppState>,
    repo: &str,
    id: &PluginId,
    staging: PathBuf,
    permissions_confirmed: bool,
) -> Result<()> {
    let _store = app.plugin_store.lock().await;
    swap_and_load(app, repo, id, staging, permissions_confirmed).await
}

fn update_scope(previous: Option<&PackageDeclarations>, next: &PackageDeclarations) -> UpdateScope {
    halod_shared::types::update_scope(
        previous.map(|previous| (&previous.uses, &previous.matches)),
        (&next.uses, &next.matches),
    )
}

async fn swap_and_load(
    app: &Arc<AppState>,
    repo: &str,
    id: &PluginId,
    staging: PathBuf,
    permissions_confirmed: bool,
) -> Result<()> {
    let next = blocking({
        let manifest = staging.join("plugin.yaml");
        move || read_declarations(&std::fs::read(manifest)?)
    })
    .await?;
    let previous = app.registry.declarations_for(id);
    let scope = update_scope(previous.as_ref(), &next);
    let disabled_for_update = !permissions_confirmed
        && scope == UpdateScope::MorePermissions
        && app.config.read().await.plugins.enabled.contains(id);
    if disabled_for_update {
        plugins::set_enabled(id.clone(), false, Arc::clone(app)).await?;
    }
    let mut copies: Vec<(PathBuf, PathBuf)> = remote_slugs(app)
        .await
        .into_iter()
        .filter(|slug| *slug != repo)
        .map(|slug| {
            let dir = catalog::repo_dir(&slug);
            (dir.join(id.as_str()), dir.join(format!(".{id}.old")))
        })
        .collect();
    let target = catalog::repo_dir(repo).join(id.as_str());
    let backup = catalog::repo_dir(repo).join(format!(".{id}.old"));
    copies.insert(0, (target.clone(), backup));
    let swap_copies = copies.clone();
    blocking(move || {
        swap_in(&staging, &swap_copies[0].0, &swap_copies[0].1)?;
        if let Err(error) = swap_copies[1..]
            .iter()
            .try_for_each(|(dir, backup)| set_aside(dir, backup))
        {
            swap_back(&swap_copies[0].0, &swap_copies[0].1)?;
            return Err(error);
        }
        Ok(())
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
        if disabled_for_update {
            plugins::set_enabled(id.clone(), true, Arc::clone(app)).await?;
        } else {
            reconcile_plugins(app, std::slice::from_ref(id)).await;
        }
        bail!("{id} failed to load: {problem}");
    }
    blocking(move || copies.iter().try_for_each(|(_, backup)| remove_dir(backup))).await?;
    let notice = match scope {
        _ if disabled_for_update => Some(NotificationCode::PluginUpdateNeedsPermissions {
            plugin: id.clone(),
            name: next.name,
        }),
        UpdateScope::NewDevices(count) => Some(NotificationCode::PluginUpdateAddsDevices {
            plugin: id.clone(),
            name: next.name,
            count,
        }),
        _ => None,
    };
    if let Some(notice) = notice {
        notifications::send(app, notice).await;
    }
    reconcile_plugins(app, std::slice::from_ref(id)).await;
    Ok(())
}

pub async fn uninstall_plugin(id: PluginId, app: Arc<AppState>) -> Result<()> {
    let _store = app.plugin_store.lock().await;
    let dirs: Vec<PathBuf> = remote_slugs(&app)
        .await
        .into_iter()
        .map(|slug| catalog::repo_dir(&slug).join(id.as_str()))
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
    let _store = app.plugin_store.lock().await;
    let updates = pending_updates(app.registry.installed(), &*app.plugin_catalogs.lock().await);
    let mut failures = Vec::new();
    for (slug, id) in updates {
        let installed = match fetch_and_stage(&app, &slug, &id).await {
            Ok(staging) => swap_and_load(&app, &slug, &id, staging, false).await,
            Err(error) => Err(error),
        };
        if let Err(error) = installed {
            failures.push(format!("{id}: {error:#}"));
        }
    }
    if !failures.is_empty() {
        bail!("updating plugins failed: {}", failures.join("; "));
    }
    Ok(())
}

fn pending_updates(
    installed: Vec<(String, PluginId, semver::Version)>,
    catalogs: &std::collections::HashMap<String, RepoCatalog>,
) -> Vec<(String, PluginId)> {
    installed
        .into_iter()
        .filter(|(slug, id, version)| {
            catalogs.get(slug).is_some_and(|repo| {
                repo.catalog
                    .select(HOST_PLUGIN_API)
                    .get(id.as_str())
                    .is_some_and(|entry| {
                        entry.api.is_compatible_with(HOST_PLUGIN_API) && entry.version > *version
                    })
            })
        })
        .map(|(slug, id, _)| (slug, id))
        .collect()
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
        std::fs::create_dir_all(catalog::repo_dir(slug)).unwrap();
        let fixtures = crate::config::config_dir()
            .join("fixtures")
            .join(slug)
            .join(version);
        crate::test_support::write_plugin_package(&fixtures, id, version, "1.0")
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
                assert!(id.parse::<PluginId>().is_err(), "{id}");
            }
            assert!(uninstall_plugin("x".parse().unwrap(), Arc::clone(&app))
                .await
                .is_err());
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
                &"demo".parse().unwrap(),
                staged("official", "demo", "1.0.0"),
                false,
            )
            .await
            .unwrap();
            install_staged(
                &app,
                "community",
                &"demo".parse().unwrap(),
                staged("community", "demo", "2.0.0"),
                false,
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
    async fn concurrent_installs_of_one_plugin_both_finish_without_leftovers() {
        crate::test_support::with_tmp_config(|app| async move {
            with_remote_repo(&app, "official").await;
            install_staged(
                &app,
                "official",
                &"demo".parse().unwrap(),
                staged("official", "demo", "1.0.0"),
                false,
            )
            .await
            .unwrap();
            let demo: PluginId = "demo".parse().unwrap();
            let (first, second) = tokio::join!(
                install_staged(
                    &app,
                    "official",
                    &demo,
                    staged("official", "demo", "1.1.0"),
                    false
                ),
                install_staged(
                    &app,
                    "official",
                    &demo,
                    staged("official", "demo", "1.2.0"),
                    false
                ),
            );
            first.unwrap();
            second.unwrap();
            assert!(installed_version(&app, "demo").is_some());
            let leftovers: Vec<_> = std::fs::read_dir(catalog::repo_dir("official"))
                .unwrap()
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with('.'))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
        })
        .await;
    }

    #[tokio::test]
    async fn a_package_that_fails_to_load_rolls_back_to_the_previous_install() {
        crate::test_support::with_tmp_config(|app| async move {
            with_remote_repo(&app, "official").await;
            install_staged(
                &app,
                "official",
                &"demo".parse().unwrap(),
                staged("official", "demo", "1.0.0"),
                false,
            )
            .await
            .unwrap();
            let broken = staged("official", "demo", "2.0.0");
            std::fs::write(broken.join("plugin.yaml"), "id: demo\n").unwrap();

            let error = install_staged(&app, "official", &"demo".parse().unwrap(), broken, false)
                .await
                .unwrap_err();

            assert!(error.to_string().contains("demo failed to load"), "{error}");
            assert_eq!(installed_version(&app, "demo").as_deref(), Some("1.0.0"));
            assert!(!catalog::repo_dir("official").join(".demo.old").exists());
        })
        .await;
    }

    #[tokio::test]
    async fn adding_a_repo_rejects_plain_http_and_duplicates() {
        crate::test_support::with_tmp_config(|app| async move {
            let insecure = add_repo(
                PluginRepoLocation::Url("http://example.org/catalog.yaml".into()),
                Arc::clone(&app),
            )
            .await;
            assert!(insecure.is_err());
            let folder = tempfile::tempdir().unwrap();
            let local = || PluginRepoLocation::Local(folder.path().display().to_string());
            add_repo(local(), Arc::clone(&app)).await.unwrap();
            let duplicate = add_repo(local(), Arc::clone(&app)).await.unwrap_err();
            assert!(duplicate.to_string().contains("already added"));
            assert_eq!(app.config.read().await.plugins.repos.len(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn the_official_repo_cannot_be_removed() {
        crate::test_support::with_tmp_config(|app| async move {
            with_remote_repo(&app, OFFICIAL_PLUGIN_REPO_SLUG).await;
            assert!(
                remove_repo(OFFICIAL_PLUGIN_REPO_SLUG.to_owned(), Arc::clone(&app))
                    .await
                    .is_err()
            );
            assert_eq!(app.config.read().await.plugins.repos.len(), 1);
        })
        .await;
    }

    #[test]
    fn updates_are_only_offered_for_newer_compatible_versions() {
        let package = |id: &str, version: &str, api: &str| CatalogPackage {
            id: id.parse().unwrap(),
            name: id.into(),
            version: version.parse().unwrap(),
            api: api.parse().unwrap(),
            path: format!("packages/{id}.tar.gz"),
            sha256: "a".repeat(64),
            size: 1,
            matches: Default::default(),
            uses: Default::default(),
        };
        let catalog = halod_plugin_signing::Catalog {
            schema: halod_plugin_signing::catalog::CATALOG_SCHEMA,
            name: "Test".into(),
            commit: None,
            published: 1,
            signing_key: None,
            packages: vec![
                package("newer", "1.1.0", "1.0"),
                package("same", "1.0.0", "1.0"),
                package("older", "0.9.0", "1.0"),
                package("future", "2.0.0", "9.0"),
            ],
        };
        let catalogs = std::collections::HashMap::from([(
            "official".to_owned(),
            RepoCatalog {
                catalog,
                last_sync: None,
            },
        )]);
        let installed = ["newer", "same", "older", "future", "elsewhere"]
            .into_iter()
            .map(|id| {
                (
                    "official".to_owned(),
                    id.parse().unwrap(),
                    "1.0.0".parse().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            pending_updates(installed, &catalogs),
            [("official".to_owned(), "newer".parse().unwrap())]
        );
    }

    #[tokio::test]
    async fn first_install_leaves_the_plugin_disabled_until_confirmed() {
        crate::test_support::with_tmp_config(|app| async move {
            with_remote_repo(&app, "official").await;
            install_staged(
                &app,
                "official",
                &"demo".parse().unwrap(),
                staged("official", "demo", "1.0.0"),
                false,
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
                .contains(&"demo".parse().unwrap()));
        })
        .await;
    }

    fn staged_using(slug: &str, version: &str, transports: &str) -> PathBuf {
        staged_manifest(
            slug,
            version,
            &format!("type: integration\ntransports:\n  command:\n    commands: [nvidia-smi]\n{transports}"),
        )
    }

    fn staged_manifest(slug: &str, version: &str, body: &str) -> PathBuf {
        let package = staged(slug, "demo", version);
        std::fs::write(
            package.join("plugin.yaml"),
            format!("id: demo\nversion: {version}\napi: \"1.0\"\n{body}"),
        )
        .unwrap();
        package
    }

    const HTTP: &str = "  http:\n    origins: [https://api.example.com]\n";

    async fn install_demo(app: &Arc<AppState>, transports: &str) {
        install_demo_staged(app, staged_using("official", "1.0.0", transports)).await;
    }

    async fn install_demo_staged(app: &Arc<AppState>, staging: PathBuf) {
        with_remote_repo(app, "official").await;
        install_staged(app, "official", &"demo".parse().unwrap(), staging, false)
            .await
            .unwrap();
    }

    async fn install_enabled_demo(app: &Arc<AppState>, transports: &str) {
        install_demo(app, transports).await;
        enable_demo(app).await;
    }

    async fn enable_demo(app: &Arc<AppState>) {
        let mut cfg = app.config.write().await;
        cfg.plugins.enabled.push("demo".parse().unwrap());
        app.registry.replace_policy(&cfg.plugins);
    }

    async fn demo_enabled(app: &AppState) -> bool {
        app.config
            .read()
            .await
            .plugins
            .enabled
            .contains(&"demo".parse().unwrap())
    }

    async fn update_demo(
        app: &Arc<AppState>,
        transports: &str,
        permissions_confirmed: bool,
    ) -> Vec<NotificationCode> {
        update_demo_staged(
            app,
            staged_using("official", "1.1.0", transports),
            permissions_confirmed,
        )
        .await
    }

    async fn update_demo_staged(
        app: &Arc<AppState>,
        staging: PathBuf,
        permissions_confirmed: bool,
    ) -> Vec<NotificationCode> {
        let mut events = app.data_bus.subscribe_events();
        install_staged(
            app,
            "official",
            &"demo".parse().unwrap(),
            staging,
            permissions_confirmed,
        )
        .await
        .unwrap();
        let mut sent = Vec::new();
        while let Ok(event) = events.try_recv() {
            let halod_shared::bus::BusEventPayload::Notification(n) = event.payload;
            sent.push(n.code);
        }
        sent
    }

    fn needs_permissions(sent: &[NotificationCode]) -> bool {
        sent.iter()
            .any(|code| matches!(code, NotificationCode::PluginUpdateNeedsPermissions { .. }))
    }

    #[tokio::test]
    async fn an_update_needing_more_permissions_lands_disabled_and_notifies() {
        crate::test_support::with_tmp_config(|app| async move {
            install_enabled_demo(&app, "").await;
            let sent = update_demo(&app, HTTP, false).await;
            assert!(!demo_enabled(&app).await);
            assert!(needs_permissions(&sent), "{sent:?}");
        })
        .await;
    }

    #[tokio::test]
    async fn a_confirmed_wider_update_stays_enabled_without_a_notification() {
        crate::test_support::with_tmp_config(|app| async move {
            install_enabled_demo(&app, "").await;
            let sent = update_demo(&app, HTTP, true).await;
            assert!(demo_enabled(&app).await);
            assert!(!needs_permissions(&sent), "{sent:?}");
        })
        .await;
    }

    #[tokio::test]
    async fn a_wider_update_of_a_disabled_plugin_does_not_notify() {
        crate::test_support::with_tmp_config(|app| async move {
            install_demo(&app, "").await;
            let sent = update_demo(&app, HTTP, false).await;
            assert!(!needs_permissions(&sent), "{sent:?}");
            assert!(!demo_enabled(&app).await);
        })
        .await;
    }

    #[tokio::test]
    async fn an_update_needing_fewer_permissions_stays_enabled() {
        crate::test_support::with_tmp_config(|app| async move {
            install_enabled_demo(&app, HTTP).await;
            let sent = update_demo(&app, "", false).await;
            assert!(demo_enabled(&app).await);
            assert!(!needs_permissions(&sent));
        })
        .await;
    }

    fn device_manifest(version: &str, pids: &str) -> PathBuf {
        staged_manifest(
            "official",
            version,
            &format!("capabilities: [battery]\ndevices:\n  - vendor: Demo\n    model: One\n    type: headset\n    match:\n      hid: {{ vid: 0x1234, pids: [{pids}] }}\ntransports:\n  hid: {{ report_size: 64 }}\n"),
        )
    }

    async fn install_enabled_device_demo(app: &Arc<AppState>) {
        install_demo_staged(app, device_manifest("1.0.0", "1")).await;
        enable_demo(app).await;
    }

    fn adds_devices(sent: &[NotificationCode]) -> Option<usize> {
        sent.iter().find_map(|code| match code {
            NotificationCode::PluginUpdateAddsDevices { count, .. } => Some(*count),
            _ => None,
        })
    }

    #[tokio::test]
    async fn an_update_adding_devices_stays_enabled_and_says_so() {
        crate::test_support::with_tmp_config(|app| async move {
            install_enabled_device_demo(&app).await;
            let sent = update_demo_staged(&app, device_manifest("1.1.0", "1, 2"), false).await;
            assert!(demo_enabled(&app).await);
            assert!(!needs_permissions(&sent));
            assert_eq!(adds_devices(&sent), Some(1));
        })
        .await;
    }

    #[tokio::test]
    async fn an_unchanged_update_sends_no_notice() {
        crate::test_support::with_tmp_config(|app| async move {
            install_enabled_device_demo(&app).await;
            let sent = update_demo_staged(&app, device_manifest("1.1.0", "1"), false).await;
            assert!(demo_enabled(&app).await);
            assert!(sent.is_empty());
        })
        .await;
    }

    #[test]
    fn scope_reads_both_sides_of_the_declarations() {
        use halod_shared::types::{HidIdentity, Permission};
        let mut base = PackageDeclarations {
            name: "demo".into(),
            matches: Default::default(),
            uses: Default::default(),
        };
        base.uses.permissions = vec![Permission::Hid];
        base.matches.hid = vec![HidIdentity {
            vid: 0x1234,
            pids: vec![1],
            usage_page: None,
            usage: None,
            interface: None,
        }];
        assert_eq!(update_scope(Some(&base), &base), UpdateScope::Unchanged);
        assert_eq!(update_scope(None, &base), UpdateScope::MorePermissions);
        let mut wider_uses = base.clone();
        wider_uses.uses.permissions.push(Permission::Network);
        assert_eq!(
            update_scope(Some(&base), &wider_uses),
            UpdateScope::MorePermissions
        );
        let mut more_pids = base.clone();
        more_pids.matches.hid[0].pids.push(2);
        assert_eq!(
            update_scope(Some(&base), &more_pids),
            UpdateScope::NewDevices(1)
        );
        let mut fewer = base.clone();
        fewer.uses.permissions.clear();
        assert_eq!(update_scope(Some(&base), &fewer), UpdateScope::Unchanged);
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
