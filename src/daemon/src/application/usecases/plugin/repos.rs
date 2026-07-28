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

use halod_shared::types::{PluginUpdateStatus, RepoUpdateStatus};

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

/// Advance `record` to `revision`, retiring the currently selected tag to
/// `previous_release_tag`. Returns the retired tag so pruning retains it.
fn advance_active_release(record: &mut PluginRepoRecord, revision: &str) -> Option<String> {
    record.previous_release_tag = record.release_tag.clone();
    record.release_tag = Some(revision.to_owned());
    record.active_revision = Some(revision.to_owned());
    record.last_sync = Some(now_rfc3339());
    record.previous_release_tag.clone()
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

fn github_owner_project(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("github.com") {
        return None;
    }
    let mut parts = parsed.path_segments()?.filter(|part| !part.is_empty());
    let owner = parts.next()?;
    let project = parts.next()?.trim_end_matches(".git");
    if parts.next().is_some()
        || !owner.bytes().all(github_name_byte)
        || !project.bytes().all(github_name_byte)
    {
        return None;
    }
    Some((owner.to_owned(), project.to_owned()))
}

fn github_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

/// Register an immutable GitHub plugin release source.
pub async fn add_repo(url: String, app: Arc<AppState>) -> Result<()> {
    let (owner, project) = github_owner_project(&url)
        .ok_or_else(|| anyhow::anyhow!("only immutable GitHub release sources are supported"))?;
    let slug = sanitize_slug(&format!("{owner}-{project}"));
    if slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG {
        anyhow::bail!("slug '{slug}' is reserved for the official plugin repository");
    }
    {
        let cfg = app.config.read().await;
        if cfg.plugins.repos.iter().any(|r| r.slug == slug) {
            anyhow::bail!("a repo with slug '{slug}' is already registered");
        }
    }
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
    Ok(())
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
    app.invalidate_signature_status(&slug).await;

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
    let previous;
    {
        let mut cfg = app.config.write().await;
        let configured = cfg
            .plugins
            .repos
            .iter_mut()
            .find(|candidate| candidate.slug == record.slug)
            .ok_or_else(|| anyhow::anyhow!("repository disappeared during restore"))?;
        previous = advance_active_release(configured, &revision);
        for package in manifest.packages {
            cfg.plugins
                .installed_hashes
                .insert(package.id, package.sha256);
        }
    }
    app.request_config_save();
    app.invalidate_signature_status(&record.slug).await;
    apply_repo_plugins(app.clone(), plugin_ids).await?;
    broadcast_plugin_updates(&app, Some(&record.slug)).await;
    prune_release_revisions(destination, revision, previous).await;
    Ok(())
}

async fn prune_release_revisions(
    root: std::path::PathBuf,
    active: String,
    previous: Option<String>,
) {
    let for_log = root.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut keep: Vec<&str> = vec![active.as_str()];
        if let Some(previous) = previous.as_deref() {
            keep.push(previous);
        }
        repo::prune_revisions(&root, &keep)
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => log::warn!(
            "pruning old plugin revisions under {}: {error:#}",
            for_log.display()
        ),
        Err(error) => log::warn!(
            "revision prune task for {} panicked: {error}",
            for_log.display()
        ),
    }
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

/// Validate the release metadata that gates both advertising and installation.
fn inspect_release(
    record: &PluginRepoRecord,
    release: &crate::domain::plugin::release_source::PublishedRelease,
) -> Result<(
    repo::RepositoryManifest,
    Vec<u8>,
    Vec<u8>,
    Option<halod_plugin_signing::RepositorySigningKey>,
)> {
    let trust = trust_for_record(record);
    let (manifest, bytes, signature) =
        crate::domain::plugin::release_source::inspect(release, &trust)?;
    let advertised = manifest.signing_key.clone();
    if matches!(trust, repo::RepositoryTrust::Unsigned) {
        if let Some(key) = &advertised {
            halod_plugin_signing::verify_advertised_signature(&bytes, &signature, key)?;
        }
    }
    // The archive descriptor is transport metadata and must be present
    // in every network release, even though legacy embedded packs omit it.
    if manifest.archive.is_none() {
        anyhow::bail!("network release has no archive descriptor");
    }
    if record.trusted_key.is_some() && record.trusted_key != advertised {
        anyhow::bail!("release signing key changed after first installation");
    }
    if let Some(expected) = &record.repository_id {
        if expected != &manifest.id {
            anyhow::bail!(
                "plugin source '{}' changed identity from '{}' to '{}'",
                record.slug,
                expected,
                manifest.id
            );
        }
    }
    Ok((manifest, bytes, signature, advertised))
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

    let inspect_record = record.clone();
    let release_to_inspect = published.clone();
    let (manifest, manifest_bytes, signature_bytes, trusted_key) =
        tokio::task::spawn_blocking(move || inspect_release(&inspect_record, &release_to_inspect))
            .await
            .context("release inspection task panicked")??;

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

    let reused_existing_final;
    if final_dir.exists() {
        let final_valid = repo::read_repository_manifest(&final_dir).is_ok()
            && repo::verify_repository_signature(&final_dir, &trust_for_record(&record)).is_ok();
        if final_valid {
            remove_repo_tree(staging).await?;
            reused_existing_final = true;
        } else {
            let corrupt =
                final_dir.with_file_name(format!(".{}.corrupt-{}", tag, uuid::Uuid::new_v4()));
            std::fs::rename(&final_dir, &corrupt)?;
            if let Err(error) = std::fs::rename(&staging, &final_dir) {
                let _ = std::fs::rename(&corrupt, &final_dir);
                return Err(error).context("replacing corrupted plugin release");
            }
            let _ = remove_repo_tree(corrupt).await;
            reused_existing_final = false;
        }
    } else {
        std::fs::create_dir_all(final_dir.parent().expect("release revision has a parent"))?;
        std::fs::rename(&staging, &final_dir)
            .with_context(|| format!("activating plugin release '{}' for '{}'", tag, slug))?;
        reused_existing_final = false;
    }
    let plugin_ids = manifest
        .packages
        .iter()
        .map(|package| package.id.clone())
        .collect::<Vec<_>>();
    let network_packages: Vec<(String, String)> = manifest
        .packages
        .iter()
        .map(|package| (package.id.clone(), package.sha256.clone()))
        .collect();
    let disk_packages = reused_existing_final
        .then(|| repo::read_repository_index(&final_dir).ok())
        .flatten()
        .map(|index| {
            index
                .packages
                .into_iter()
                .map(|package| (package.id, package.sha256))
                .collect::<Vec<_>>()
        });
    let installed_packages =
        hashes_to_install(reused_existing_final, disk_packages, network_packages);
    let previous;
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
        previous = advance_active_release(configured, &tag);
        configured.release_policy = if pin {
            crate::config::PluginReleasePolicy::Pinned(tag.clone())
        } else {
            crate::config::PluginReleasePolicy::Latest
        };
        configured.active_source = crate::config::PluginRevisionSource::Managed;
        for (id, sha256) in &installed_packages {
            cfg.plugins
                .installed_hashes
                .insert(id.clone(), sha256.clone());
        }
    }
    app.request_config_save();
    app.invalidate_signature_status(&slug).await;
    let settled = app.plugin_update_plan.lock().await.settle_repo(&slug, &tag);
    if settled {
        app.record_change(crate::domain::events::Change::PluginData)
            .await;
    }
    apply_repo_plugins(app.clone(), plugin_ids).await?;
    broadcast_plugin_updates(&app, Some(&slug)).await;
    prune_release_revisions(root, tag, previous).await;
    Ok(())
}

fn hashes_to_install(
    reused_existing_final: bool,
    disk_packages: Option<Vec<(String, String)>>,
    network_packages: Vec<(String, String)>,
) -> Vec<(String, String)> {
    match (reused_existing_final, disk_packages) {
        (true, Some(disk)) => disk,
        _ => network_packages,
    }
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
    app.plugin_update_plan.lock().await.purge_slug(&slug);
    app.invalidate_signature_status(&slug).await;
    app.record_change(crate::domain::events::Change::PluginData)
        .await;
    apply_repo_plugins(app, plugin_ids).await?;
    Ok(())
}

/// Compare every registered source with its selected release.
async fn compute_repo_updates(
    app: &Arc<AppState>,
) -> (Vec<RepoUpdateStatus>, Vec<PluginUpdateStatus>) {
    let repos = app.config.read().await.plugins.repos.clone();
    let mut out = Vec::with_capacity(repos.len());
    let mut plugins = Vec::new();
    for r in repos {
        if r.source_kind != crate::config::PluginRepoSourceKind::Release {
            continue;
        }
        let source = r.url.clone();
        let releases = match tokio::task::spawn_blocking(move || {
            crate::domain::plugin::release_source::list(&source)
        })
        .await
        {
            Ok(Ok(releases)) => releases,
            Ok(Err(error)) => {
                log::warn!("checking releases for '{}': {error:#}", r.slug);
                continue;
            }
            Err(error) => {
                log::warn!("release-list task for '{}' panicked: {error}", r.slug);
                continue;
            }
        };
        let pin = match &r.release_policy {
            crate::config::PluginReleasePolicy::Latest => None,
            crate::config::PluginReleasePolicy::Pinned(tag) => Some(tag.as_str()),
        };
        let Some(selected_index) = releases
            .iter()
            .position(|release| pin.map_or(!release.prerelease, |tag| release.tag == tag))
        else {
            log::warn!("selected plugin release for '{}' was not found", r.slug);
            continue;
        };
        let installed_tag = r.release_tag.as_deref();
        let candidate = match pin {
            Some(_) if installed_tag == Some(releases[selected_index].tag.as_str()) => releases
                [..selected_index]
                .iter()
                .find(|release| !release.prerelease),
            _ if selected_release_is_update(installed_tag, &releases[selected_index].tag) => {
                Some(&releases[selected_index])
            }
            _ => None,
        };
        let inspected = match candidate {
            Some(release) => inspect_update_candidate(&r, release).await,
            None => None,
        };
        let available_tag = inspected.as_ref().map(|(tag, _)| tag.clone());
        if let Some((tag, manifest)) = inspected {
            log::info!(
                "plugin repository '{}' update available: {} → {tag}",
                r.slug,
                r.release_tag.as_deref().unwrap_or("none")
            );
            if pin.is_none() {
                plugins.extend(remote_package_updates(&r, manifest).await);
            }
        } else {
            log::debug!("plugin repository '{}' is up to date", r.slug);
        }
        out.push(RepoUpdateStatus {
            installed_tag: r.release_tag.clone().unwrap_or_default(),
            slug: r.slug.clone(),
            available_tag,
            pinned: pin.is_some(),
        });
    }
    (out, plugins)
}

fn selected_release_is_update(installed_tag: Option<&str>, selected_tag: &str) -> bool {
    installed_tag != Some(selected_tag)
}

/// Newest stable release published after a pinned selection.
async fn inspect_update_candidate(
    record: &PluginRepoRecord,
    candidate: &crate::domain::plugin::release_source::PublishedRelease,
) -> Option<(String, repo::RepositoryManifest)> {
    let candidate = candidate.clone();
    let tag = candidate.tag.clone();
    let slug = record.slug.clone();
    let record = record.clone();
    match tokio::task::spawn_blocking(move || inspect_release(&record, &candidate)).await {
        Ok(Ok((manifest, _, _, _))) => Some((tag, manifest)),
        Ok(Err(error)) => {
            log::info!("release '{tag}' of '{slug}' is not installable: {error:#}");
            None
        }
        Err(error) => {
            log::warn!("release inspection task for '{slug}' panicked: {error}");
            None
        }
    }
}

async fn remote_package_updates(
    record: &PluginRepoRecord,
    manifest: repo::RepositoryManifest,
) -> Vec<PluginUpdateStatus> {
    let active_dir = repo::active_revision_dir(record);
    let installed: std::collections::HashMap<String, (String, String)> =
        match tokio::task::spawn_blocking(move || repo::read_repository_index(&active_dir)).await {
            Ok(Ok(index)) => index
                .packages
                .into_iter()
                .map(|package| (package.id, (package.version, package.sha256)))
                .collect(),
            _ => std::collections::HashMap::new(),
        };

    let updates: Vec<_> = manifest
        .packages
        .into_iter()
        .filter(|package| package_has_update(installed.get(&package.id), &package.sha256))
        .map(|package| PluginUpdateStatus {
            current_version: installed
                .get(&package.id)
                .map(|(version, _)| version.clone())
                .unwrap_or_default(),
            plugin_id: package.id,
            slug: record.slug.clone(),
            update_available: true,
            on_disk_changed: false,
            available_version: package.version,
        })
        .collect();
    for update in &updates {
        log::info!(
            "plugin '{}' update available in '{}': {} → {}",
            update.plugin_id,
            record.slug,
            update.current_version,
            update.available_version
        );
    }
    updates
}

fn package_has_update(installed: Option<&(String, String)>, available_hash: &str) -> bool {
    installed.is_none_or(|(_, hash)| hash != available_hash)
}

/// Retained update-availability state: the single source of truth behind the
/// `updates`/`repo_updates` wire vecs. Callers mutate it under
/// `AppState::plugin_update_plan`, then commit `Change::PluginData`.
#[derive(Default)]
pub struct UpdatePlan {
    packages: Vec<PluginUpdateStatus>,
    repos: Vec<RepoUpdateStatus>,
}

impl UpdatePlan {
    pub fn packages(&self) -> Vec<PluginUpdateStatus> {
        self.packages.clone()
    }

    pub fn repos(&self) -> Vec<RepoUpdateStatus> {
        self.repos.clone()
    }

    pub fn apply_repo_check(&mut self, repos: Vec<RepoUpdateStatus>) {
        self.repos = repos;
    }

    pub fn apply_package_check(
        &mut self,
        remote: Vec<PluginUpdateStatus>,
        on_disk: Vec<PluginUpdateStatus>,
    ) {
        self.packages = remote;
        self.merge_packages(on_disk);
    }

    pub fn apply_disk_scan(&mut self, reached: &[String], statuses: Vec<PluginUpdateStatus>) {
        self.packages
            .retain(|status| !reached.contains(&status.slug));
        self.merge_packages(statuses);
    }

    /// The repo now holds the release it was asked for: nothing pending until
    /// the next check. Without this the GUI keeps offering "Update repo" after
    /// a pull. Returns false when the slug has no retained status.
    pub fn settle_repo(&mut self, slug: &str, installed_tag: &str) -> bool {
        let Some(status) = self.repos.iter_mut().find(|status| status.slug == slug) else {
            return false;
        };
        status.installed_tag = installed_tag.to_owned();
        if status.available_tag.as_deref() == Some(installed_tag) {
            status.available_tag = None;
        }
        true
    }

    pub fn purge_slug(&mut self, slug: &str) {
        self.packages.retain(|status| status.slug != slug);
        self.repos.retain(|status| status.slug != slug);
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    fn merge_packages(&mut self, from: Vec<PluginUpdateStatus>) {
        for status in from {
            match self
                .packages
                .iter_mut()
                .find(|existing| existing.plugin_id == status.plugin_id)
            {
                Some(existing) => {
                    existing.update_available |= status.update_available;
                    existing.on_disk_changed |= status.on_disk_changed;
                }
                None => self.packages.push(status),
            }
        }
    }
}

/// Every repository package (optionally scoped to one repo), compared to the
/// package digest recorded when the repository was last explicitly installed.
/// Release checks never mutate the installed revision.
async fn compute_plugin_updates(
    app: &Arc<AppState>,
    slug_filter: Option<&str>,
) -> (Vec<PluginUpdateStatus>, Vec<String>) {
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
async fn broadcast_plugin_updates(app: &Arc<AppState>, slug_filter: Option<&str>) {
    let (statuses, reached) = compute_plugin_updates(app, slug_filter).await;
    app.plugin_update_plan
        .lock()
        .await
        .apply_disk_scan(&reached, statuses);
    app.record_change(crate::domain::events::Change::PluginData)
        .await;
}

fn slugs_to_update(
    plugin_statuses: &[PluginUpdateStatus],
    repo_statuses: &[RepoUpdateStatus],
) -> std::collections::HashSet<String> {
    let mut slugs = std::collections::HashSet::new();
    for status in plugin_statuses
        .iter()
        .filter(|s| s.update_available || s.on_disk_changed)
    {
        slugs.insert(status.slug.clone());
    }
    for status in repo_statuses
        .iter()
        .filter(|status| status.available_tag.is_some() && !status.pinned)
    {
        slugs.insert(status.slug.clone());
    }
    slugs
}

pub async fn update_all_plugins(app: Arc<AppState>) -> Result<()> {
    let (statuses, _reached) = compute_plugin_updates(&app, None).await;
    let repo_statuses = compute_repo_updates(&app).await.0;
    let slugs = slugs_to_update(&statuses, &repo_statuses);
    let mut failures = Vec::new();
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
    if app.config.read().await.gui.plugin_downloads
        != halod_shared::types::PluginDownloadConsent::Allowed
    {
        return;
    }
    let (repo_statuses, remote_updates) = compute_repo_updates(&app).await;
    let reached: Vec<String> = repo_statuses.iter().map(|s| s.slug.clone()).collect();
    app.plugin_update_plan
        .lock()
        .await
        .apply_repo_check(repo_statuses);
    app.record_change(crate::domain::events::Change::PluginData)
        .await;
    touch_last_sync(&app, &reached).await;

    let (on_disk, _plugin_reached) = compute_plugin_updates(&app, None).await;
    app.plugin_update_plan
        .lock()
        .await
        .apply_package_check(remote_updates, on_disk);
    app.record_change(crate::domain::events::Change::PluginData)
        .await;
}

/// Every repo plugin whose package content differs from the digest installed
/// from its repository index. This is informational; an enabled plugin is
/// already covered by the consent modal and is not silently disabled.
async fn compute_on_disk_changes(app: &Arc<AppState>) -> Vec<PluginUpdateStatus> {
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

    app.plugin_update_plan
        .lock()
        .await
        .apply_package_check(Vec::new(), statuses);
    app.record_change(crate::domain::events::Change::PluginData)
        .await;
}

/// Check every registered repo for updates and commit the result.
pub async fn check_repo_updates(app: Arc<AppState>, _client: ClientHandle) -> Result<()> {
    check_updates_broadcast(app).await;
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
    let result = match record.source_kind {
        crate::config::PluginRepoSourceKind::Archive => {
            refresh_archive_repo(record, app.clone()).await
        }
        crate::config::PluginRepoSourceKind::Release => match record.release_policy {
            crate::config::PluginReleasePolicy::Latest => {
                follow_latest_release(slug.clone(), app.clone()).await
            }
            crate::config::PluginReleasePolicy::Pinned(tag) => {
                install_release(slug.clone(), tag, true, app.clone()).await
            }
        },
    };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_release_is_only_an_update_when_the_tag_changed() {
        assert!(!selected_release_is_update(Some("v2"), "v2"));
        assert!(selected_release_is_update(Some("v1"), "v2"));
        assert!(selected_release_is_update(None, "v2"));
    }

    #[test]
    fn package_update_uses_the_installed_repository_revision() {
        let installed = ("1.0.0".to_owned(), "old-hash".to_owned());
        assert!(!package_has_update(Some(&installed), "old-hash"));
        assert!(package_has_update(Some(&installed), "new-hash"));
        assert!(package_has_update(None, "new-package-hash"));
    }

    fn plugin_status(
        slug: &str,
        update_available: bool,
        on_disk_changed: bool,
    ) -> PluginUpdateStatus {
        PluginUpdateStatus {
            plugin_id: format!("{slug}-plugin"),
            slug: slug.to_owned(),
            update_available,
            on_disk_changed,
            current_version: String::new(),
            available_version: String::new(),
        }
    }

    fn repo_status(slug: &str, available_tag: Option<&str>, pinned: bool) -> RepoUpdateStatus {
        RepoUpdateStatus {
            installed_tag: "v1".to_owned(),
            slug: slug.to_owned(),
            available_tag: available_tag.map(str::to_owned),
            pinned,
        }
    }

    #[test]
    fn a_repo_url_slug_includes_the_owner() {
        let alice = github_owner_project("https://github.com/alice/plugins").unwrap();
        let bob = github_owner_project("https://github.com/bob/plugins").unwrap();
        assert_ne!(
            sanitize_slug(&format!("{}-{}", alice.0, alice.1)),
            sanitize_slug(&format!("{}-{}", bob.0, bob.1)),
            "same project under different owners must not collide"
        );
        assert_eq!(alice.1, "plugins");
    }

    #[test]
    fn only_single_project_github_urls_are_accepted() {
        assert!(github_owner_project("https://github.com/owner/project").is_some());
        assert!(github_owner_project("https://github.com/owner/project.git").is_some());
        assert!(github_owner_project("https://github.com/owner/project/releases").is_none());
        assert!(github_owner_project("https://github.com/owner").is_none());
        assert!(github_owner_project("http://github.com/owner/project").is_none());
        assert!(github_owner_project("https://example.com/owner/project").is_none());
        assert!(github_owner_project("not a url").is_none());
    }

    #[test]
    fn on_disk_only_changes_still_select_the_repo_for_update() {
        let plugins = [plugin_status("dirty", false, true)];
        let selected = slugs_to_update(&plugins, &[]);
        assert!(
            selected.contains("dirty"),
            "a changed-on-disk package must be restored by update-all"
        );
    }

    #[test]
    fn update_selection_covers_available_updates_and_unpinned_behind_repos() {
        let plugins = [
            plugin_status("has-update", true, false),
            plugin_status("clean", false, false),
        ];
        let repos = [
            repo_status("behind", Some("v2"), false),
            repo_status("pinned-behind", Some("v2"), true),
            repo_status("uptodate", None, false),
        ];
        let selected = slugs_to_update(&plugins, &repos);
        assert!(selected.contains("has-update"));
        assert!(selected.contains("behind"));
        assert!(!selected.contains("clean"));
        assert!(!selected.contains("pinned-behind"));
        assert!(!selected.contains("uptodate"));
    }

    #[test]
    fn settling_keeps_a_newer_pinned_release_but_clears_the_installed_one() {
        let mut plan = UpdatePlan::default();
        plan.apply_repo_check(vec![repo_status("pinned", Some("v3"), true)]);

        assert!(plan.settle_repo("pinned", "v2"));
        let status = plan.repos().remove(0);
        assert_eq!(status.installed_tag, "v2");
        assert_eq!(
            status.available_tag.as_deref(),
            Some("v3"),
            "a newer stable stays advertised after installing an older tag"
        );

        assert!(plan.settle_repo("pinned", "v3"));
        let status = plan.repos().remove(0);
        assert_eq!(status.installed_tag, "v3");
        assert!(
            status.available_tag.is_none(),
            "installing the advertised tag clears the flag"
        );
    }

    #[test]
    fn settling_an_unknown_slug_reports_no_change() {
        let mut plan = UpdatePlan::default();
        assert!(!plan.settle_repo("missing", "v1"));
        assert!(plan.repos().is_empty());
    }

    #[test]
    fn package_check_or_merges_remote_and_on_disk_status_per_plugin() {
        let mut plan = UpdatePlan::default();
        let mut remote = plugin_status("repo", true, false);
        remote.available_version = "2.0.0".to_owned();

        plan.apply_package_check(vec![remote], vec![plugin_status("repo", false, true)]);

        let packages = plan.packages();
        assert_eq!(packages.len(), 1, "one merged entry per plugin id");
        assert!(packages[0].update_available);
        assert!(packages[0].on_disk_changed);
        assert_eq!(
            packages[0].available_version, "2.0.0",
            "the remote status keeps its advertised version through the merge"
        );
    }

    #[test]
    fn a_scoped_disk_scan_retains_statuses_of_unreached_slugs() {
        let mut plan = UpdatePlan::default();
        plan.apply_package_check(
            vec![
                plugin_status("reached", true, false),
                plugin_status("other", true, false),
            ],
            Vec::new(),
        );

        plan.apply_disk_scan(
            &["reached".to_owned()],
            vec![plugin_status("reached", false, true)],
        );

        let packages = plan.packages();
        assert_eq!(packages.len(), 2);
        let reached = packages
            .iter()
            .find(|status| status.slug == "reached")
            .unwrap();
        assert!(
            !reached.update_available,
            "a rescanned slug drops its stale remote status"
        );
        assert!(reached.on_disk_changed);
        assert!(
            packages
                .iter()
                .any(|status| status.slug == "other" && status.update_available),
            "an unreached slug keeps its retained status"
        );
    }

    #[test]
    fn purging_a_slug_removes_both_package_and_repo_statuses() {
        let mut plan = UpdatePlan::default();
        plan.apply_repo_check(vec![
            repo_status("gone", Some("v2"), false),
            repo_status("kept", None, false),
        ]);
        plan.apply_package_check(
            vec![
                plugin_status("gone", true, false),
                plugin_status("kept", false, true),
            ],
            Vec::new(),
        );

        plan.purge_slug("gone");

        assert!(plan.packages().iter().all(|status| status.slug == "kept"));
        assert!(plan.repos().iter().all(|status| status.slug == "kept"));
        assert_eq!(plan.packages().len(), 1);
        assert_eq!(plan.repos().len(), 1);
    }

    #[test]
    fn clearing_the_plan_empties_all_update_state() {
        let mut plan = UpdatePlan::default();
        plan.apply_repo_check(vec![repo_status("repo", Some("v2"), false)]);
        plan.apply_package_check(vec![plugin_status("repo", true, false)], Vec::new());

        plan.clear();

        assert!(plan.packages().is_empty());
        assert!(plan.repos().is_empty());
    }

    #[test]
    fn reused_revision_records_disk_hashes_over_the_network_manifest() {
        let network = vec![("pkg".to_owned(), "net-hash".to_owned())];
        let disk = vec![("pkg".to_owned(), "disk-hash".to_owned())];
        assert_eq!(
            hashes_to_install(true, Some(disk.clone()), network.clone()),
            disk,
            "a reused validated revision records its on-disk digests"
        );
        assert_eq!(
            hashes_to_install(false, Some(disk), network.clone()),
            network,
            "a fresh install records the downloaded content's digests"
        );
        assert_eq!(
            hashes_to_install(true, None, network.clone()),
            network,
            "an unreadable reused index falls back to the network manifest"
        );
    }

    #[test]
    fn advancing_a_release_retires_the_active_tag_for_pruning() {
        let mut record = release_repo_record("repo");

        let previous = advance_active_release(&mut record, "v2");

        assert_eq!(previous.as_deref(), Some("v1"));
        assert_eq!(record.previous_release_tag.as_deref(), Some("v1"));
        assert_eq!(record.release_tag.as_deref(), Some("v2"));
        assert_eq!(record.active_revision.as_deref(), Some("v2"));
        assert!(record.last_sync.is_some());
    }

    fn release_repo_record(slug: &str) -> PluginRepoRecord {
        PluginRepoRecord {
            url: format!("https://github.com/example/{slug}"),
            slug: slug.to_owned(),
            repository_id: None,
            trusted_key: None,
            source_kind: crate::config::PluginRepoSourceKind::Release,
            release_tag: Some("v1".to_owned()),
            release_policy: crate::config::PluginReleasePolicy::Latest,
            active_revision: Some("v1".to_owned()),
            active_source: crate::config::PluginRevisionSource::Managed,
            previous_release_tag: None,
            last_sync: None,
        }
    }

    #[tokio::test]
    async fn broadcasting_updates_settles_stale_per_plugin_status_for_a_slug() {
        crate::test_support::with_tmp_config(|app| async move {
            app.config
                .write()
                .await
                .plugins
                .repos
                .push(release_repo_record("stale-repo"));
            app.plugin_update_plan
                .lock()
                .await
                .apply_package_check(vec![plugin_status("stale-repo", true, false)], Vec::new());

            broadcast_plugin_updates(&app, Some("stale-repo")).await;

            assert!(
                !app.plugin_update_plan
                    .lock()
                    .await
                    .packages()
                    .iter()
                    .any(|status| status.slug == "stale-repo" && status.update_available),
                "no stale update_available may survive a re-broadcast for the slug"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn removing_a_repo_purges_its_retained_update_statuses() {
        crate::test_support::with_tmp_config(|app| async move {
            app.config
                .write()
                .await
                .plugins
                .repos
                .push(release_repo_record("gone"));
            {
                let mut plan = app.plugin_update_plan.lock().await;
                plan.apply_package_check(vec![plugin_status("gone", true, false)], Vec::new());
                plan.apply_repo_check(vec![repo_status("gone", Some("v2"), false)]);
            }

            remove_repo("gone".to_owned(), app.clone()).await.unwrap();

            let plan = app.plugin_update_plan.lock().await;
            assert!(
                plan.packages().is_empty(),
                "per-plugin statuses for the removed slug must be purged"
            );
            assert!(
                plan.repos().is_empty(),
                "repo statuses for the removed slug must be purged"
            );
        })
        .await;
    }
}
