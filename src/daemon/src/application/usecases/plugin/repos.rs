// SPDX-License-Identifier: GPL-3.0-or-later
//! Managing immutable plugin release sources and imported release archives.

use crate::domain::events::ChangeSink as _;

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::json;

use crate::application::ipc::ClientHandle;
use crate::application::state::AppState;
use crate::config::PluginRepoRecord;
use crate::domain::plugin::repo;

use halod_shared::types::RepoUpdateStatus;

use super::plugins::{apply_repo_plugins, purge_plugin_state, sanitize_slug};

/// RFC 3339 timestamp for `PluginRepoRecord::last_sync`.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn trust_for_record(record: &PluginRepoRecord) -> repo::RepositoryTrust {
    if record.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG {
        repo::RepositoryTrust::Official
    } else if let Some(key) = &record.trusted_key {
        repo::RepositoryTrust::Pinned(key.clone())
    } else {
        repo::RepositoryTrust::Unsigned
    }
}

async fn package_disk_hash(
    repo_dir: &std::path::Path,
    subpath: &std::path::Path,
) -> Option<String> {
    let package = repo_dir.join(subpath);
    tokio::task::spawn_blocking(move || repo::package_hash(&package))
        .await
        .ok()?
        .ok()
}

/// Register an immutable GitHub plugin release source.
pub async fn add_repo(url: String, app: Arc<AppState>) -> Result<()> {
    let slug = sanitize_slug(&url);
    if slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG {
        anyhow::bail!("slug '{slug}' is reserved for the official plugin repository");
    }
    {
        let cfg = app.config.read().await;
        if cfg.plugins.repos.iter().any(|r| r.slug == slug) {
            anyhow::bail!("a repo with slug '{slug}' is already registered");
        }
    }
    if url.starts_with("https://github.com/") {
        {
            let mut cfg = app.config.write().await;
            cfg.plugins.repos.push(PluginRepoRecord {
                url: url.clone(),
                slug: slug.clone(),
                repository_id: None,
                trusted_key: None,
                source_kind: crate::config::PluginRepoSourceKind::Release,
                release_tag: None,
                release_policy: crate::config::PluginReleasePolicy::Latest,
                active_revision: None,
                active_source: crate::config::PluginRevisionSource::Managed,
                previous_release_tag: None,
                last_sync: None,
            });
        }
        app.request_config_save();
        if let Err(error) = follow_latest_release(slug.clone(), app.clone()).await {
            app.config
                .write()
                .await
                .plugins
                .repos
                .retain(|record| record.slug != slug);
            app.request_config_save();
            return Err(error);
        }
        return Ok(());
    }
    anyhow::bail!("only immutable GitHub release sources are supported");
}

/// Remove an unregistered or failed-add repository tree away from Tokio's
/// worker threads. A missing path is already clean.
async fn remove_repo_tree(path: std::path::PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || match std::fs::remove_dir_all(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    })
    .await
    .context("repository cleanup task panicked")?
}

const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

fn extract_repository_archive(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> Result<()> {
    use std::io::Read as _;

    let file = std::fs::File::open(source)
        .with_context(|| format!("opening repository archive {}", source.display()))?;
    let gzip = source
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".tar.gz") || name.ends_with(".tgz"));
    let reader: Box<dyn std::io::Read> = if gzip {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = tar::Archive::new(reader);
    let mut total = 0_u64;
    for entry in archive.entries().context("reading repository archive")? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path.is_absolute()
            || path.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            })
        {
            anyhow::bail!(
                "repository archive contains an unsafe path: {}",
                path.display()
            );
        }
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(destination.join(&path))?;
            continue;
        }
        if !kind.is_file() {
            anyhow::bail!(
                "repository archive contains a link or special file: {}",
                path.display()
            );
        }
        total = total
            .checked_add(entry.size())
            .filter(|total| *total <= MAX_ARCHIVE_BYTES)
            .ok_or_else(|| anyhow::anyhow!("repository archive exceeds 512 MiB"))?;
        let target = destination.join(&path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = std::fs::File::create(&target)?;
        std::io::copy(&mut entry.by_ref().take(MAX_ARCHIVE_BYTES + 1), &mut output)?;
    }
    Ok(())
}

fn archive_repository_root(extracted: &std::path::Path) -> Result<std::path::PathBuf> {
    if extracted.join("release.yaml").is_file() {
        return Ok(extracted.to_owned());
    }
    let mut candidates = std::fs::read_dir(extracted)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join("release.yaml").is_file());
    let root = candidates
        .next()
        .ok_or_else(|| anyhow::anyhow!("archive does not contain release.yaml"))?;
    if candidates.next().is_some() {
        anyhow::bail!("archive contains more than one repository");
    }
    Ok(root)
}

fn archive_sha256(source: &std::path::Path) -> Result<String> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;
    let mut file = std::fs::File::open(source)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub async fn import_local_repo(source_path: String, app: Arc<AppState>) -> Result<()> {
    let source = std::fs::canonicalize(&source_path)
        .with_context(|| format!("resolving local repository source {source_path}"))?;
    if source.is_dir() {
        anyhow::bail!("select a release archive; source repository folders are not installable");
    }

    std::fs::create_dir_all(crate::config::config_dir())?;
    let temporary = tempfile::tempdir_in(crate::config::config_dir())?;
    let extracted = temporary.path().join("repository");
    std::fs::create_dir_all(&extracted)?;
    let source_for_task = source.clone();
    let extracted_for_task = extracted.clone();
    tokio::task::spawn_blocking(move || {
        extract_repository_archive(&source_for_task, &extracted_for_task)
    })
    .await
    .context("repository archive extraction task panicked")??;
    let root = archive_repository_root(&extracted)?;
    let manifest = repo::read_repository_manifest(&root)?;
    let trusted_key = repo::advertised_signing_key(&root)?;
    let trust = trusted_key
        .clone()
        .map(repo::RepositoryTrust::Pinned)
        .unwrap_or_default();
    if !matches!(trust, repo::RepositoryTrust::Unsigned) {
        repo::verify_repository_signature(&root, &trust)?;
    }
    let slug = sanitize_slug(&manifest.id);
    if slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG {
        anyhow::bail!("archive repository id '{}' is reserved", manifest.id);
    }
    if app
        .config
        .read()
        .await
        .plugins
        .repos
        .iter()
        .any(|record| record.slug == slug || record.repository_id.as_deref() == Some(&manifest.id))
    {
        anyhow::bail!("repository '{}' is already registered", manifest.id);
    }
    let revision = archive_sha256(&source)?;
    let destination = crate::config::plugin_repos_dir().join(&slug);
    let final_revision = destination.join("revisions").join(&revision);
    std::fs::create_dir_all(final_revision.parent().expect("revision has parent"))?;
    std::fs::rename(&root, &final_revision)?;

    let plugin_ids: Vec<_> = manifest
        .packages
        .iter()
        .map(|package| package.id.clone())
        .collect();
    {
        let mut cfg = app.config.write().await;
        cfg.plugins.repos.push(PluginRepoRecord {
            url: source.display().to_string(),
            slug,
            repository_id: Some(manifest.id),
            trusted_key,
            source_kind: crate::config::PluginRepoSourceKind::Archive,
            release_tag: Some(revision.clone()),
            release_policy: crate::config::PluginReleasePolicy::Latest,
            active_revision: Some(revision),
            active_source: crate::config::PluginRevisionSource::Managed,
            previous_release_tag: None,
            last_sync: Some(now_rfc3339()),
        });
        for package in manifest.packages {
            cfg.plugins
                .installed_hashes
                .insert(package.id, package.sha256);
        }
    }
    app.request_config_save();
    apply_repo_plugins(app, plugin_ids).await
}

async fn refresh_archive_repo(record: PluginRepoRecord, app: Arc<AppState>) -> Result<()> {
    let source = std::path::PathBuf::from(&record.url);
    std::fs::create_dir_all(crate::config::config_dir())?;
    let temporary = tempfile::tempdir_in(crate::config::config_dir())?;
    let extracted = temporary.path().join("repository");
    std::fs::create_dir_all(&extracted)?;
    let source_for_task = source.clone();
    let extracted_for_task = extracted.clone();
    tokio::task::spawn_blocking(move || {
        extract_repository_archive(&source_for_task, &extracted_for_task)
    })
    .await
    .context("repository archive extraction task panicked")??;
    let root = archive_repository_root(&extracted)?;
    let manifest = repo::read_repository_manifest(&root)?;
    if record.repository_id.as_deref() != Some(&manifest.id) {
        anyhow::bail!("imported archive changed repository identity");
    }
    if manifest.signing_key != record.trusted_key {
        anyhow::bail!("repository signing key changed after first import");
    }
    let trust = trust_for_record(&record);
    if !matches!(trust, repo::RepositoryTrust::Unsigned) {
        repo::verify_repository_signature(&root, &trust)?;
    }
    let revision = archive_sha256(&source)?;
    let destination = crate::config::plugin_repos_dir().join(&record.slug);
    let final_revision = destination.join("revisions").join(&revision);
    if final_revision.exists() {
        let backup = destination
            .join("revisions")
            .join(format!(".{revision}.corrupt-{}", uuid::Uuid::new_v4()));
        std::fs::rename(&final_revision, &backup)?;
        if let Err(error) = std::fs::rename(&root, &final_revision) {
            let _ = std::fs::rename(&backup, &final_revision);
            return Err(error).context("activating restored archive repository");
        }
        let _ = std::fs::remove_dir_all(backup);
    } else {
        std::fs::create_dir_all(final_revision.parent().expect("revision has parent"))?;
        std::fs::rename(&root, &final_revision)?;
    }
    let plugin_ids: Vec<_> = manifest
        .packages
        .iter()
        .map(|package| package.id.clone())
        .collect();
    {
        let mut cfg = app.config.write().await;
        let configured = cfg
            .plugins
            .repos
            .iter_mut()
            .find(|candidate| candidate.slug == record.slug)
            .ok_or_else(|| anyhow::anyhow!("repository disappeared during restore"))?;
        configured.release_tag = Some(revision.clone());
        configured.active_revision = Some(revision);
        configured.last_sync = Some(now_rfc3339());
        for package in manifest.packages {
            cfg.plugins
                .installed_hashes
                .insert(package.id, package.sha256);
        }
    }
    app.request_config_save();
    apply_repo_plugins(app, plugin_ids).await
}

/// List immutable releases for the add-source picker. GitHub supplies ordering
/// and tags only; the signed release manifest remains the content authority.
pub async fn list_releases(url: String, client: ClientHandle) -> Result<()> {
    let source = url.clone();
    let releases =
        tokio::task::spawn_blocking(move || crate::domain::plugin::release_source::list(&source))
            .await
            .context("release-list task panicked")??;
    client.send_json(&json!({
        "type": "plugin_releases",
        "url": url,
        "releases": releases.into_iter().map(|release| json!({
            "tag": release.tag,
            "prerelease": release.prerelease,
            "published_at": release.published_at,
        })).collect::<Vec<_>>(),
    }));
    Ok(())
}

/// Download, verify, and atomically activate one complete plugin release.
pub async fn install_release(
    slug: String,
    tag: String,
    pin: bool,
    app: Arc<AppState>,
) -> Result<()> {
    let record = app
        .config
        .read()
        .await
        .plugins
        .repos
        .iter()
        .find(|record| record.slug == slug)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown plugin source '{slug}'"))?;
    let source = record.url.clone();
    let wanted = tag.clone();
    let published = tokio::task::spawn_blocking(move || -> Result<_> {
        crate::domain::plugin::release_source::list(&source)?
            .into_iter()
            .find(|release| release.tag == wanted)
            .ok_or_else(|| anyhow::anyhow!("release '{wanted}' was not found"))
    })
    .await
    .context("release lookup task panicked")??;

    let initial_trust = trust_for_record(&record);
    let inspect_release = published.clone();
    let (manifest, manifest_bytes, signature_bytes, trusted_key) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let (manifest, bytes, signature) =
                crate::domain::plugin::release_source::inspect(&inspect_release, &initial_trust)?;
            let advertised = manifest.signing_key.clone();
            if matches!(initial_trust, repo::RepositoryTrust::Unsigned) {
                if let Some(key) = &advertised {
                    halod_plugin_signing::verify_advertised_signature(&bytes, &signature, key)?;
                }
            }
            // The archive descriptor is transport metadata and must be present
            // in every network release, even though legacy embedded packs omit it.
            if manifest.archive.is_none() {
                anyhow::bail!("network release has no archive descriptor");
            }
            Ok((manifest, bytes, signature, advertised))
        })
        .await
        .context("release inspection task panicked")??;

    if record.trusted_key.is_some() && record.trusted_key != trusted_key {
        anyhow::bail!("release signing key changed after first installation");
    }
    if let Some(expected) = &record.repository_id {
        if expected != &manifest.id {
            anyhow::bail!(
                "plugin source '{}' changed identity from '{}' to '{}'",
                slug,
                expected,
                manifest.id
            );
        }
    }

    let root = crate::config::plugin_repos_dir().join(&slug);
    let final_dir = crate::domain::plugin::release_source::revision_dir(&root, &tag)?;
    let staging = final_dir
        .parent()
        .expect("release revision has a parent")
        .join(format!(".{}.staging-{}", tag, uuid::Uuid::new_v4()));
    let published_for_download = published.clone();
    let manifest_for_download = manifest.clone();
    let bytes_for_download = manifest_bytes.clone();
    let signature_for_download = signature_bytes.clone();
    let staging_for_download = staging.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        crate::domain::plugin::release_source::download(
            &published_for_download,
            &manifest_for_download,
            &bytes_for_download,
            &signature_for_download,
            &staging_for_download,
        )?;
        repo::read_repository_manifest(&staging_for_download)?;
        Ok(())
    })
    .await
    .context("release download task panicked")??;

    if final_dir.exists() {
        let final_valid = repo::read_repository_manifest(&final_dir).is_ok()
            && repo::verify_repository_signature(&final_dir, &trust_for_record(&record)).is_ok();
        if final_valid {
            remove_repo_tree(staging).await?;
        } else {
            let corrupt =
                final_dir.with_file_name(format!(".{}.corrupt-{}", tag, uuid::Uuid::new_v4()));
            std::fs::rename(&final_dir, &corrupt)?;
            if let Err(error) = std::fs::rename(&staging, &final_dir) {
                let _ = std::fs::rename(&corrupt, &final_dir);
                return Err(error).context("replacing corrupted plugin release");
            }
            let _ = remove_repo_tree(corrupt).await;
        }
    } else {
        std::fs::create_dir_all(final_dir.parent().expect("release revision has a parent"))?;
        std::fs::rename(&staging, &final_dir)
            .with_context(|| format!("activating plugin release '{}' for '{}'", tag, slug))?;
    }
    let plugin_ids = manifest
        .packages
        .iter()
        .map(|package| package.id.clone())
        .collect::<Vec<_>>();
    {
        let mut cfg = app.config.write().await;
        let configured = cfg
            .plugins
            .repos
            .iter_mut()
            .find(|record| record.slug == slug)
            .ok_or_else(|| anyhow::anyhow!("plugin source disappeared during installation"))?;
        configured.repository_id.get_or_insert(manifest.id.clone());
        configured.trusted_key = configured.trusted_key.clone().or(trusted_key);
        configured.previous_release_tag = configured.release_tag.clone();
        configured.release_tag = Some(tag.clone());
        configured.release_policy = if pin {
            crate::config::PluginReleasePolicy::Pinned(tag.clone())
        } else {
            crate::config::PluginReleasePolicy::Latest
        };
        configured.active_revision = Some(tag);
        configured.active_source = crate::config::PluginRevisionSource::Managed;
        configured.last_sync = Some(now_rfc3339());
        for package in &manifest.packages {
            cfg.plugins
                .installed_hashes
                .insert(package.id.clone(), package.sha256.clone());
        }
    }
    app.request_config_save();
    apply_repo_plugins(app, plugin_ids).await
}

pub async fn follow_latest_release(slug: String, app: Arc<AppState>) -> Result<()> {
    let source = app
        .config
        .read()
        .await
        .plugins
        .repos
        .iter()
        .find(|record| record.slug == slug)
        .map(|record| record.url.clone())
        .ok_or_else(|| anyhow::anyhow!("unknown plugin source '{slug}'"))?;
    let latest = tokio::task::spawn_blocking(move || {
        crate::domain::plugin::release_source::list(&source)?
            .into_iter()
            .find(|release| !release.prerelease)
            .ok_or_else(|| anyhow::anyhow!("plugin source has no stable release"))
    })
    .await
    .context("latest-release lookup task panicked")??;
    install_release(slug, latest.tag, false, app).await
}

/// Unregister a plugin release source, purge its plugin ids, and rediscover.
/// The official repo cannot be removed — only its content can be updated.
pub async fn remove_repo(slug: String, app: Arc<AppState>) -> Result<()> {
    if slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG {
        anyhow::bail!("the official plugin repository cannot be removed");
    }
    let record = {
        let cfg = app.config.read().await;
        cfg.plugins
            .repos
            .iter()
            .find(|record| record.slug == slug)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown plugin repo '{slug}'"))?
    };
    let repo_dir = crate::config::plugin_repos_dir().join(&slug);
    // Read the selected immutable revision before removing it so stale content cannot decide
    // which persisted plugin state is purged.
    let plugin_ids = crate::domain::plugin::repo_plugin_ids(&repo::active_revision_dir(&record));
    for id in &plugin_ids {
        purge_plugin_state(id, &app).await;
    }

    match std::fs::remove_dir_all(&repo_dir) {
        Ok(()) => log::info!("Removed plugin repo {}", repo_dir.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::warn!("Plugin repo dir {} already gone", repo_dir.display());
        }
        Err(e) => return Err(e).with_context(|| format!("removing {}", repo_dir.display())),
    }

    {
        let mut cfg = app.config.write().await;
        cfg.plugins.repos.retain(|r| r.slug != slug);
    }
    app.request_config_save();
    apply_repo_plugins(app, plugin_ids).await?;
    Ok(())
}

/// Compare every registered source with its selected release.
async fn compute_repo_updates(app: &Arc<AppState>) -> Vec<RepoUpdateStatus> {
    let repos = app.config.read().await.plugins.repos.clone();
    let mut out = Vec::with_capacity(repos.len());
    for r in repos {
        if r.source_kind == crate::config::PluginRepoSourceKind::Release {
            let source = r.url.clone();
            let pinned = match &r.release_policy {
                crate::config::PluginReleasePolicy::Latest => None,
                crate::config::PluginReleasePolicy::Pinned(tag) => Some(tag.clone()),
            };
            match tokio::task::spawn_blocking(move || {
                crate::domain::plugin::release_source::list(&source)?
                    .into_iter()
                    .find(|release| {
                        pinned
                            .as_deref()
                            .map_or(!release.prerelease, |tag| release.tag == tag)
                    })
                    .ok_or_else(|| anyhow::anyhow!("selected plugin release was not found"))
            })
            .await
            {
                Ok(Ok(latest)) => out.push(RepoUpdateStatus {
                    slug: r.slug,
                    installed_tag: r.release_tag.clone().unwrap_or_default(),
                    behind: r.release_tag.as_deref() != Some(latest.tag.as_str()),
                    latest_tag: latest.tag,
                }),
                Ok(Err(error)) => {
                    log::warn!("checking releases for '{}': {error:#}", r.slug)
                }
                Err(error) => log::warn!("release-list task for '{}' panicked: {error}", r.slug),
            }
            continue;
        }
    }
    out
}

/// Every repository package (optionally scoped to one repo), compared to the
/// package digest recorded when the repository was last explicitly installed.
/// Release checks never mutate the installed revision.
async fn compute_plugin_updates(
    app: &Arc<AppState>,
    slug_filter: Option<&str>,
) -> (Vec<halod_shared::types::PluginUpdateStatus>, Vec<String>) {
    let mut statuses = compute_on_disk_changes(app).await;
    if let Some(slug) = slug_filter {
        statuses.retain(|status| status.slug == slug);
    }
    let reached = app
        .config
        .read()
        .await
        .plugins
        .repos
        .iter()
        .filter(|record| slug_filter.is_none_or(|slug| record.slug == slug))
        .map(|record| record.slug.clone())
        .collect();
    (statuses, reached)
}

async fn touch_last_sync(app: &Arc<AppState>, slugs: &[String]) {
    if slugs.is_empty() {
        return;
    }
    {
        let mut cfg = app.config.write().await;
        for r in cfg
            .plugins
            .repos
            .iter_mut()
            .filter(|r| slugs.contains(&r.slug))
        {
            r.last_sync = Some(now_rfc3339());
        }
    }
    app.request_config_save();
    app.record_change(crate::domain::events::Change::PluginTopology)
        .await;
}

/// Recompute per-plugin update status (optionally scoped to one repo) and
/// commit it to the retained plugins topic.
pub(crate) async fn broadcast_plugin_updates(app: &Arc<AppState>, slug_filter: Option<&str>) {
    let (statuses, reached) = compute_plugin_updates(app, slug_filter).await;
    touch_last_sync(app, &reached).await;
    publish_plugin_updates(app, statuses).await;
}

/// Cache and commit the latest plugin-update status.
pub(crate) async fn publish_plugin_updates(
    app: &Arc<AppState>,
    statuses: Vec<halod_shared::types::PluginUpdateStatus>,
) {
    *app.plugin_update_status.lock().await = statuses;
    app.record_change(crate::domain::events::Change::PluginData)
        .await;
}

async fn publish_repo_updates(app: &Arc<AppState>, statuses: Vec<RepoUpdateStatus>) {
    *app.plugin_repo_update_status.lock().await = statuses;
    app.record_change(crate::domain::events::Change::PluginData)
        .await;
}

/// Update every plugin currently flagged as having an update available, across every repo.
pub async fn update_all_plugins(app: Arc<AppState>) -> Result<()> {
    let (statuses, _reached) = compute_plugin_updates(&app, None).await;
    let mut slugs = std::collections::HashSet::new();
    let mut failures = Vec::new();
    for status in statuses.into_iter().filter(|s| s.update_available) {
        slugs.insert(status.slug);
    }
    for status in compute_repo_updates(&app)
        .await
        .into_iter()
        .filter(|status| status.behind)
    {
        slugs.insert(status.slug);
    }
    for slug in slugs {
        if let Err(e) = update_repo(slug.clone(), app.clone()).await {
            log::warn!("updating plugin repository '{slug}': {e:#}");
            failures.push(format!("updating plugin repository '{slug}': {e:#}"));
        }
    }
    broadcast_plugin_updates(&app, None).await;
    if !failures.is_empty() {
        anyhow::bail!(failures.join("\n"));
    }
    Ok(())
}

/// Background/startup update check: compute repo- and plugin-level update
/// status and commit both to the retained plugins record.
/// Errors are logged per-repo inside the compute helpers, so this never fails.
pub async fn check_updates_broadcast(app: Arc<AppState>) {
    let repo_statuses = compute_repo_updates(&app).await;
    let reached: Vec<String> = repo_statuses.iter().map(|s| s.slug.clone()).collect();
    publish_repo_updates(&app, repo_statuses).await;
    touch_last_sync(&app, &reached).await;

    let (mut statuses, plugin_reached) = compute_plugin_updates(&app, None).await;
    // Re-add on-disk flags for repos whose remote fetch failed (skipped above).
    for od in compute_on_disk_changes(&app).await {
        if !statuses.iter().any(|s| s.plugin_id == od.plugin_id) {
            statuses.push(od);
        }
    }
    touch_last_sync(&app, &plugin_reached).await;
    publish_plugin_updates(&app, statuses).await;
}

/// Every repo plugin whose package content differs from the digest installed
/// from its repository index. This is informational; an enabled plugin is
/// already covered by the consent modal and is not silently disabled.
async fn compute_on_disk_changes(
    app: &Arc<AppState>,
) -> Vec<halod_shared::types::PluginUpdateStatus> {
    use halod_shared::types::PluginUpdateStatus;
    let policy = app.config.read().await.plugins.clone();
    let repos = policy.repos.clone();
    let mut out = Vec::new();
    for r in repos {
        // A seeded source whose initial release install failed has no immutable
        // revision to inspect. Treat it as unavailable instead of reading the
        // deliberately nonexistent `__inactive__` sentinel path and logging a
        // misleading warning on every update pass.
        if r.active_revision.as_deref().is_none_or(str::is_empty) {
            continue;
        }
        let active_dir = repo::active_revision_dir(&r);
        let manifest = match tokio::task::spawn_blocking({
            let active_dir = active_dir.clone();
            move || repo::read_repository_index(&active_dir)
        })
        .await
        {
            Ok(Ok(manifest)) => manifest,
            Ok(Err(error)) => {
                log::warn!(
                    "reading active repository index for '{}': {error:#}",
                    r.slug
                );
                continue;
            }
            Err(error) => {
                log::warn!(
                    "active repository index task for '{}' panicked: {error:#}",
                    r.slug
                );
                continue;
            }
        };
        for package in manifest.packages {
            let local_hash = package_disk_hash(&active_dir, &package.path).await;
            let changed = policy
                .installed_hashes
                .get(&package.id)
                .is_some_and(|installed| local_hash.as_deref() != Some(installed.as_str()));
            if changed {
                out.push(PluginUpdateStatus {
                    plugin_id: package.id,
                    slug: r.slug.clone(),
                    update_available: false,
                    on_disk_changed: true,
                    current_version: package.version,
                    available_version: String::new(),
                });
            }
        }
    }
    out
}

/// Preserve a visible changed-on-disk status without a separate quarantine or
/// re-consent state. Explicit repository updates restore the indexed content.
pub async fn quarantine_changed_plugins(app: Arc<AppState>) {
    let statuses = compute_on_disk_changes(&app).await;
    if statuses.is_empty() {
        return;
    }

    for s in &statuses {
        log::warn!(
            "plugin '{}' differs from its installed package digest",
            s.plugin_id
        );
    }

    publish_plugin_updates(&app, statuses).await;
}

/// Check every registered repo for updates and commit the result.
pub async fn check_repo_updates(app: Arc<AppState>, _client: ClientHandle) -> Result<()> {
    let statuses = compute_repo_updates(&app).await;
    // Each returned status is a repo we successfully reached — stamp their sync.
    let reached: Vec<String> = statuses.iter().map(|s| s.slug.clone()).collect();
    touch_last_sync(&app, &reached).await;
    publish_repo_updates(&app, statuses).await;
    // A repo check must also refresh the per-plugin update flags, or the plugin
    // update banners never appear until the daemon restarts.
    broadcast_plugin_updates(&app, None).await;
    Ok(())
}

/// Fetch and install a repository as one unit. The complete checkout is
/// validated before its package digests become the new installed baselines.
pub async fn update_repo(slug: String, app: Arc<AppState>) -> Result<()> {
    let record = app
        .config
        .read()
        .await
        .plugins
        .repos
        .iter()
        .find(|record| record.slug == slug)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown plugin source {slug}"))?;
    match record.source_kind {
        crate::config::PluginRepoSourceKind::Archive => refresh_archive_repo(record, app).await,
        crate::config::PluginRepoSourceKind::Release => match record.release_policy {
            crate::config::PluginReleasePolicy::Latest => follow_latest_release(slug, app).await,
            crate::config::PluginReleasePolicy::Pinned(tag) => {
                install_release(slug, tag, true, app).await
            }
        },
    }
}
