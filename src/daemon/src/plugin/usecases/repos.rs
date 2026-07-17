// SPDX-License-Identifier: GPL-3.0-or-later
//! Managing registered git-repo plugin sources: add, remove, check for updates, and update.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::json;

use crate::config::PluginRepoRecord;
use crate::ipc::ClientHandle;
use crate::plugin::repo;
use crate::state::AppState;

use halod_shared::types::RepoUpdateStatus;

use super::plugins::{apply_repo_plugins, purge_plugin_state, sanitize_slug};

/// RFC 3339 timestamp for `PluginRepoRecord::last_sync`.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn repo_cloned(dir: &std::path::Path) -> bool {
    dir.join(".git").exists()
}

/// The direct development source replaces the official checkout for the life
/// of this process. Never fetch, update, or otherwise mutate the inactive
/// official repository while that override is selected.
async fn official_repo_is_overridden(app: &AppState) -> bool {
    #[cfg(feature = "dev-plugin-repo")]
    {
        return app.development_plugin_repo.read().await.is_some();
    }
    #[cfg(not(feature = "dev-plugin-repo"))]
    {
        let _ = app;
        false
    }
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

async fn record_signature(
    app: &AppState,
    slug: &str,
    repo_dir: &std::path::Path,
    sha: &str,
    trust: &repo::RepositoryTrust,
) {
    if matches!(trust, repo::RepositoryTrust::Unsigned) {
        return;
    }
    let verification = {
        let dir = repo_dir.to_owned();
        let sha = sha.to_owned();
        let trust = trust.clone();
        tokio::task::spawn_blocking(move || {
            repo::verify_repository_signature_at_commit(&dir, &sha, &trust)
        })
        .await
    };
    let status = match verification {
        Ok(Ok(())) => halod_shared::types::RepoSignatureStatus::Verified,
        Ok(Err(error)) => halod_shared::types::RepoSignatureStatus::Invalid {
            reason: format!("{error:#}"),
        },
        Err(error) => halod_shared::types::RepoSignatureStatus::Invalid {
            reason: format!("signature verification task failed: {error}"),
        },
    };
    app.repo_signature_status
        .lock()
        .await
        .insert(slug.to_owned(), (sha.to_owned(), status));
}

async fn record_tip_compatibility(
    app: &AppState,
    slug: &str,
    repo_dir: &std::path::Path,
    sha: &str,
) {
    let result = {
        let dir = repo_dir.to_owned();
        let sha = sha.to_owned();
        tokio::task::spawn_blocking(move || {
            repo::validate_repository_compatibility_at_commit(&dir, &sha)
        })
        .await
    };
    let status = match result {
        Ok(Ok(())) => halod_shared::types::RepoCompatibilityStatus::Compatible,
        Ok(Err(error)) => halod_shared::types::RepoCompatibilityStatus::Incompatible {
            reason: format!("{error:#}"),
        },
        Err(error) => halod_shared::types::RepoCompatibilityStatus::Incompatible {
            reason: format!("compatibility check task failed: {error}"),
        },
    };
    app.repo_compatibility_status
        .lock()
        .await
        .insert(slug.to_owned(), status);
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

/// Build and validate an immutable executable revision from fetched Git
/// objects. The active checkout is never modified; only a fully validated
/// staging directory is renamed into `revisions/<sha>`.
fn materialize_revision(
    repo_dir: &std::path::Path,
    sha: &str,
    trust: &repo::RepositoryTrust,
) -> Result<repo::RepositoryManifest> {
    let revisions = repo_dir.join("revisions");
    let final_dir = revisions.join(sha);
    let validate = |dir: &std::path::Path| -> Result<repo::RepositoryManifest> {
        let manifest = repo::read_repository_manifest(dir)?;
        if !matches!(trust, repo::RepositoryTrust::Unsigned) {
            let yaml = std::fs::read(dir.join("repository.yaml"))?;
            let signature = std::fs::read(dir.join("repository.sig"))?;
            match trust {
                repo::RepositoryTrust::Official => {
                    repo::verify_official_repository_signature(dir)?;
                }
                repo::RepositoryTrust::Pinned(key) => {
                    halod_plugin_signing::verify_advertised_signature(&yaml, &signature, key)?;
                }
                repo::RepositoryTrust::Unsigned => unreachable!(),
            }
        }
        Ok(manifest)
    };
    if final_dir.is_dir() {
        if let Ok(manifest) = validate(&final_dir) {
            return Ok(manifest);
        }
        log::warn!(
            "rebuilding corrupted repository revision {} from fetched Git objects",
            final_dir.display()
        );
    }
    std::fs::create_dir_all(&revisions)
        .with_context(|| format!("creating revision store {}", revisions.display()))?;
    let staging = revisions.join(format!(".{sha}.staging-{}", uuid::Uuid::new_v4()));
    repo::materialize_commit(repo_dir, sha, &staging)?;
    let manifest = match validate(&staging) {
        Ok(manifest) => manifest,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error).context("validating materialized repository revision");
        }
    };
    if final_dir.exists() {
        let backup = revisions.join(format!(".{sha}.corrupt-{}", uuid::Uuid::new_v4()));
        std::fs::rename(&final_dir, &backup)
            .with_context(|| format!("quarantining corrupted revision {}", final_dir.display()))?;
        if let Err(error) = std::fs::rename(&staging, &final_dir) {
            let _ = std::fs::rename(&backup, &final_dir);
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error).with_context(|| {
                format!(
                    "activating rebuilt revision {} from {}",
                    final_dir.display(),
                    staging.display()
                )
            });
        }
        if let Err(error) = std::fs::remove_dir_all(&backup) {
            log::warn!(
                "removing quarantined repository revision {}: {error}",
                backup.display()
            );
        }
    } else {
        std::fs::rename(&staging, &final_dir).with_context(|| {
            format!(
                "activating immutable revision {} from {}",
                final_dir.display(),
                staging.display()
            )
        })?;
    }
    Ok(manifest)
}

/// Register a git-repo plugin source: clone it, pin `locked_sha`, persist, and rediscover.
pub async fn add_repo(url: String, branch: Option<String>, app: Arc<AppState>) -> Result<()> {
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
    let dest = crate::config::plugin_repos_dir().join(&slug);
    remove_repo_tree(dest.clone()).await?;
    let staging = dest.with_file_name(format!(".{}.adding-{}", slug, uuid::Uuid::new_v4()));
    let preflight = async {
        let tip_sha = {
            let url = url.clone();
            let staging = staging.clone();
            let branch = branch.clone();
            tokio::task::spawn_blocking(move || repo::clone(&url, &staging, branch.as_deref()))
                .await
                .context("clone task panicked")??
        };
        record_tip_compatibility(&app, &slug, &staging, &tip_sha).await;
        let trusted_key = {
            let staging = staging.clone();
            let tip_sha = tip_sha.clone();
            tokio::task::spawn_blocking(move || {
                repo::advertised_signing_key_at_commit(&staging, &tip_sha)
            })
            .await
            .context("repository signing-key inspection task panicked")??
        };
        let trust = trusted_key
            .clone()
            .map(repo::RepositoryTrust::Pinned)
            .unwrap_or_default();
        record_signature(&app, &slug, &staging, &tip_sha, &trust).await;

        let compatible = {
            let staging = staging.clone();
            let tip_sha = tip_sha.clone();
            let trust = trust.clone();
            tokio::task::spawn_blocking(move || {
                repo::latest_compatible_revision(&staging, &tip_sha, &trust)
            })
            .await
            .context("repository history scan task panicked")??
            .ok_or_else(|| {
                anyhow::anyhow!("repository has no revision compatible with this Halo")
            })?
        };
        let locked_sha = compatible.sha;
        let manifest = {
            let staging = staging.clone();
            let sha = locked_sha.clone();
            let trust = trust.clone();
            tokio::task::spawn_blocking(move || materialize_revision(&staging, &sha, &trust))
                .await
                .context("repository manifest validation task panicked")??
        };
        Ok::<_, anyhow::Error>((tip_sha, trusted_key, locked_sha, manifest))
    }
    .await;
    let (_tip_sha, trusted_key, locked_sha, manifest) = match preflight {
        Ok(ready) => ready,
        Err(error) => {
            if let Err(cleanup_error) = remove_repo_tree(staging).await {
                log::warn!("cleaning failed repository add for '{slug}': {cleanup_error:#}");
            }
            app.repo_signature_status.lock().await.remove(&slug);
            app.repo_compatibility_status.lock().await.remove(&slug);
            return Err(error);
        }
    };
    let staging_for_activation = staging.clone();
    let dest_for_activation = dest.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || {
        std::fs::rename(&staging_for_activation, &dest_for_activation)
            .with_context(|| format!("activating repository {}", dest_for_activation.display()))
    })
    .await
    .context("repository activation task panicked")?
    {
        let _ = remove_repo_tree(staging).await;
        return Err(error);
    }
    let packages = manifest.packages;
    let active_revision = locked_sha.clone();
    let plugin_ids: Vec<String> = packages.iter().map(|package| package.id.clone()).collect();

    {
        let mut cfg = app.config.write().await;
        cfg.plugins.repos.push(PluginRepoRecord {
            url,
            slug,
            repository_id: Some(manifest.id.clone()),
            trusted_key,
            source_kind: crate::config::PluginRepoSourceKind::Git,
            branch,
            locked_sha,
            active_revision: Some(active_revision),
            active_source: crate::config::PluginRevisionSource::Managed,
            previous_verified_sha: None,
            last_sync: Some(now_rfc3339()),
        });
        for package in &packages {
            cfg.plugins
                .installed_hashes
                .insert(package.id.clone(), package.sha256.clone());
        }
    }
    app.request_config_save();
    apply_repo_plugins(app, plugin_ids).await?;
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
    if extracted.join("repository.yaml").is_file() {
        return Ok(extracted.to_owned());
    }
    let mut candidates = std::fs::read_dir(extracted)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join("repository.yaml").is_file());
    let root = candidates
        .next()
        .ok_or_else(|| anyhow::anyhow!("archive does not contain repository.yaml"))?;
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
    Ok(format!("{:x}", hasher.finalize()))
}

pub async fn import_local_repo(source_path: String, app: Arc<AppState>) -> Result<()> {
    let source = std::fs::canonicalize(&source_path)
        .with_context(|| format!("resolving local repository source {source_path}"))?;
    if source.is_dir() {
        let url = url::Url::from_file_path(&source)
            .map_err(|_| anyhow::anyhow!("local repository path is not a valid file URL"))?
            .into();
        return add_repo(url, None, app).await;
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
            branch: None,
            locked_sha: revision.clone(),
            active_revision: Some(revision),
            active_source: crate::config::PluginRevisionSource::Managed,
            previous_verified_sha: None,
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
        configured.locked_sha = revision.clone();
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

/// List a remote's branches without cloning and reply with a `repo_branches`
/// frame echoing `url` so the client can match it to the in-progress form.
pub async fn list_branches(url: String, client: ClientHandle) -> Result<()> {
    let branches = {
        let url = url.clone();
        tokio::task::spawn_blocking(move || repo::list_remote_branches(&url))
            .await
            .context("branch-list task panicked")??
    };
    client.send_json(&json!({
        "type": "repo_branches",
        "url": url,
        "branches": branches,
    }));
    Ok(())
}

/// Unregister a git-repo plugin source: purge its plugin ids, delete its clone dir, persist, and rediscover.
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
    // The Git worktree is object storage only.  Read the selected immutable
    // revision before removing it so a stale/mutated worktree cannot decide
    // which persisted plugin state is purged.
    let plugin_ids = crate::plugin::repo_plugin_ids(&repo::active_revision_dir(&record));
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

/// Fetch every registered repo's remote tip and compare to `locked_sha`; a repo whose fetch fails is logged and skipped.
async fn compute_repo_updates(app: &Arc<AppState>) -> Vec<RepoUpdateStatus> {
    let repos = app.config.read().await.plugins.repos.clone();
    let official_overridden = official_repo_is_overridden(app).await;
    let mut out = Vec::with_capacity(repos.len());
    for r in repos {
        if official_overridden && r.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG {
            continue;
        }
        let dir = crate::config::plugin_repos_dir().join(&r.slug);
        if !repo_cloned(&dir) {
            continue;
        }
        let branch = r.branch.clone();
        let fetch_dir = dir.clone();
        let result = tokio::task::spawn_blocking(move || {
            repo::fetch_remote_sha(&fetch_dir, branch.as_deref())
        })
        .await;
        match result {
            Ok(Ok(remote_sha)) => {
                let trust = trust_for_record(&r);
                record_signature(app, &r.slug, &dir, &remote_sha, &trust).await;
                record_tip_compatibility(app, &r.slug, &dir, &remote_sha).await;
                let compatible = {
                    let dir = dir.clone();
                    let tip_sha = remote_sha;
                    let trust = trust.clone();
                    tokio::task::spawn_blocking(move || {
                        repo::latest_compatible_revision(&dir, &tip_sha, &trust)
                    })
                    .await
                };
                let candidate_sha = match compatible {
                    Ok(Ok(Some(revision))) => revision.sha,
                    Ok(Ok(None)) => r.locked_sha.clone(),
                    Ok(Err(error)) => {
                        log::warn!("scanning repository history for '{}': {error:#}", r.slug);
                        continue;
                    }
                    Err(error) => {
                        log::warn!(
                            "repository-history task for '{}' panicked: {error:#}",
                            r.slug
                        );
                        continue;
                    }
                };
                let behind = candidate_sha != r.locked_sha;
                out.push(RepoUpdateStatus {
                    slug: r.slug,
                    locked_sha: r.locked_sha,
                    remote_sha: candidate_sha,
                    behind,
                });
            }
            Ok(Err(e)) => log::warn!("checking updates for repo '{}': {e:#}", r.slug),
            Err(e) => log::warn!("fetch task for repo '{}' panicked: {e:#}", r.slug),
        }
    }
    out
}

/// Every repository package (optionally scoped to one repo), compared to the
/// package digest recorded when the repository was last explicitly installed.
/// The remote index is read from Git objects and never changes the checkout.
async fn compute_plugin_updates(
    app: &Arc<AppState>,
    slug_filter: Option<&str>,
) -> (Vec<halod_shared::types::PluginUpdateStatus>, Vec<String>) {
    use halod_shared::types::PluginUpdateStatus;

    let policy = app.config.read().await.plugins.clone();
    let official_overridden = official_repo_is_overridden(app).await;
    let repos: Vec<_> = policy
        .repos
        .iter()
        .filter(|r| {
            slug_filter.is_none_or(|s| s == r.slug)
                && !(official_overridden && r.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG)
        })
        .cloned()
        .collect();

    let plugins = app.registry.list(&*app.secret_store);
    let mut out = Vec::new();
    let mut reached = Vec::new();
    for r in repos {
        let dir = crate::config::plugin_repos_dir().join(&r.slug);
        if !repo_cloned(&dir) {
            continue;
        }
        let branch = r.branch.clone();
        let remote_sha = {
            let dir = dir.clone();
            match tokio::task::spawn_blocking(move || {
                repo::fetch_remote_sha(&dir, branch.as_deref())
            })
            .await
            {
                Ok(Ok(sha)) => sha,
                Ok(Err(e)) => {
                    log::warn!("checking plugin updates for repo '{}': {e:#}", r.slug);
                    continue;
                }
                Err(e) => {
                    log::warn!("fetch task for repo '{}' panicked: {e:#}", r.slug);
                    continue;
                }
            }
        };
        reached.push(r.slug.clone());

        let trust = trust_for_record(&r);
        record_signature(app, &r.slug, &dir, &remote_sha, &trust).await;
        record_tip_compatibility(app, &r.slug, &dir, &remote_sha).await;
        let result = {
            let dir = dir.clone();
            let tip_sha = remote_sha.clone();
            let trust = trust.clone();
            tokio::task::spawn_blocking(move || {
                repo::latest_compatible_revision(&dir, &tip_sha, &trust)
            })
            .await
        };
        match result {
            Ok(Ok(Some(revision))) => {
                let manifest = revision.manifest;
                for package in manifest.packages {
                    let installed_hash = policy.installed_hashes.get(&package.id);
                    let active_dir = repo::active_revision_dir(&r);
                    let local_hash = package_disk_hash(&active_dir, &package.path).await;
                    let loaded = plugins.iter().find(|plugin| {
                        plugin.id == package.id
                            && matches!(
                                &plugin.source,
                                halod_shared::types::PluginSource::Repo { slug }
                                    if slug == &r.slug
                            )
                    });
                    out.push(PluginUpdateStatus {
                        plugin_id: package.id.clone(),
                        slug: r.slug.clone(),
                        update_available: installed_hash != Some(&package.sha256),
                        on_disk_changed: installed_hash
                            .is_some_and(|hash| local_hash.as_deref() != Some(hash.as_str())),
                        current_version: loaded
                            .map(|plugin| plugin.version.clone())
                            .unwrap_or_default(),
                        available_version: package.version,
                    });
                }
            }
            Ok(Ok(None)) => log::info!(
                "repository '{}' has no revision compatible with this Halo",
                r.slug
            ),
            Ok(Err(e)) => log::warn!("scanning repository history for '{}': {e:#}", r.slug),
            Err(e) => log::warn!("repository-index task for '{}' panicked: {e:#}", r.slug),
        }
    }
    (out, reached)
}

/// Stamp `last_sync` to now for every named repo (those a check actually
/// reached) and push the updated state so the GUI's "LAST SYNC" reflects it.
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
    crate::ipc::broadcast_state(app).await;
}

/// Recompute per-plugin update status (optionally scoped to one repo) and
/// broadcast it to every client, so their update banners reflect reality after
/// an update lands.
pub(crate) async fn broadcast_plugin_updates(app: &Arc<AppState>, slug_filter: Option<&str>) {
    let (statuses, reached) = compute_plugin_updates(app, slug_filter).await;
    touch_last_sync(app, &reached).await;
    publish_plugin_updates(app, statuses).await;
}

/// Cache the latest plugin-update status (so a client that connects later gets
/// it, via `ipc::plugin_updates_frame`) and broadcast it now.
pub(crate) async fn publish_plugin_updates(
    app: &Arc<AppState>,
    statuses: Vec<halod_shared::types::PluginUpdateStatus>,
) {
    let frame = json!({ "type": "plugin_updates", "plugins": statuses });
    *app.plugin_update_status.lock().await = statuses;
    crate::ipc::broadcast_json(app, &frame).await;
}

/// Update every plugin currently flagged as having an update available, across every repo.
pub async fn update_all_plugins(app: Arc<AppState>) -> Result<()> {
    let (statuses, _reached) = compute_plugin_updates(&app, None).await;
    let mut slugs = std::collections::HashSet::new();
    for status in statuses.into_iter().filter(|s| s.update_available) {
        slugs.insert(status.slug);
    }
    for slug in slugs {
        if let Err(e) = update_repo(slug.clone(), app.clone()).await {
            log::warn!("updating plugin repository '{slug}': {e:#}");
        }
    }
    broadcast_plugin_updates(&app, None).await;
    Ok(())
}

/// Background/startup update check: compute repo- and plugin-level update
/// status and broadcast both to every connected client (no requesting client).
/// Errors are logged per-repo inside the compute helpers, so this never fails.
pub async fn check_updates_broadcast(app: Arc<AppState>) {
    let repo_statuses = compute_repo_updates(&app).await;
    let reached: Vec<String> = repo_statuses.iter().map(|s| s.slug.clone()).collect();
    crate::ipc::broadcast_json(
        &app,
        &json!({
            "type": "plugin_repo_updates",
            "repos": repo_statuses,
        }),
    )
    .await;
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
        // A seeded repository whose initial clone failed has no immutable
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

/// Check every registered repo for updates and reply to the requesting client with a `plugin_repo_updates` frame.
pub async fn check_repo_updates(app: Arc<AppState>, client: ClientHandle) -> Result<()> {
    let statuses = compute_repo_updates(&app).await;
    // Each returned status is a repo we successfully reached — stamp their sync.
    let reached: Vec<String> = statuses.iter().map(|s| s.slug.clone()).collect();
    touch_last_sync(&app, &reached).await;
    client.send_json(&json!({
        "type": "plugin_repo_updates",
        "repos": statuses,
    }));
    // A repo check must also refresh the per-plugin update flags, or the plugin
    // update banners never appear until the daemon restarts.
    broadcast_plugin_updates(&app, None).await;
    Ok(())
}

/// Fetch and install a repository as one unit. The complete checkout is
/// validated before its package digests become the new installed baselines.
pub async fn update_repo(slug: String, app: Arc<AppState>) -> Result<()> {
    if slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG
        && official_repo_is_overridden(&app).await
    {
        anyhow::bail!(
            "the official plugin repository is disabled while --dev-plugin-repo is active"
        );
    }
    let record = {
        let cfg = app.config.read().await;
        cfg.plugins
            .repos
            .iter()
            .find(|r| r.slug == slug)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown plugin repo '{slug}'"))?
    };
    if record.source_kind == crate::config::PluginRepoSourceKind::Archive {
        return refresh_archive_repo(record, app).await;
    }

    let dir = crate::config::plugin_repos_dir().join(&slug);
    let old_plugin_ids = {
        let active_dir = repo::active_revision_dir(&record);
        tokio::task::spawn_blocking(move || repo::read_repository_index(&active_dir))
            .await
            .ok()
            .and_then(Result::ok)
            .map(|manifest| {
                manifest
                    .packages
                    .into_iter()
                    .map(|package| package.id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let remote_sha = {
        let dir = dir.clone();
        let branch = record.branch.clone();
        tokio::task::spawn_blocking(move || repo::fetch_remote_sha(&dir, branch.as_deref()))
            .await
            .context("fetch task panicked")??
    };
    if !official_repo_slug(&slug) {
        let dir_for_key = dir.clone();
        let sha_for_key = remote_sha.clone();
        let advertised = tokio::task::spawn_blocking(move || {
            repo::advertised_signing_key_at_commit(&dir_for_key, &sha_for_key)
        })
        .await
        .context("repository signing-key inspection task panicked")??;
        if advertised != record.trusted_key {
            anyhow::bail!("repository signing key changed after first import");
        }
    }
    let trust = trust_for_record(&record);
    let official = slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG;
    record_signature(&app, &slug, &dir, &remote_sha, &trust).await;
    record_tip_compatibility(&app, &slug, &dir, &remote_sha).await;
    let compatible = {
        let dir = dir.clone();
        let tip_sha = remote_sha;
        let trust = trust.clone();
        tokio::task::spawn_blocking(move || {
            repo::latest_compatible_revision(&dir, &tip_sha, &trust)
        })
        .await
        .context("repository history scan task panicked")??
        .ok_or_else(|| anyhow::anyhow!("repository has no revision compatible with this Halo"))?
    };
    let remote_sha = compatible.sha;
    let manifest = {
        let dir = dir.clone();
        let sha = remote_sha.clone();
        let trust = trust.clone();
        tokio::task::spawn_blocking(move || materialize_revision(&dir, &sha, &trust))
            .await
            .context("repository revision materialization task panicked")??
    };
    if let Some(expected_id) = &record.repository_id {
        if manifest.id != *expected_id {
            anyhow::bail!(
                "repository '{}' changed its identity from '{}' to '{}'",
                slug,
                expected_id,
                manifest.id
            );
        }
    }
    let plugin_ids: Vec<String> = manifest.packages.iter().map(|p| p.id.clone()).collect();
    let removed_plugin_ids: Vec<String> = old_plugin_ids
        .into_iter()
        .filter(|id| !plugin_ids.contains(id))
        .collect();

    {
        let mut cfg = app.config.write().await;
        if let Some(r) = cfg.plugins.repos.iter_mut().find(|r| r.slug == slug) {
            if r.repository_id.is_none() {
                r.repository_id = Some(manifest.id.clone());
            }
            if official && !r.locked_sha.is_empty() {
                r.previous_verified_sha = Some(r.locked_sha.clone());
            }
            r.locked_sha = remote_sha.clone();
            r.active_revision = Some(remote_sha);
            r.active_source = crate::config::PluginRevisionSource::Managed;
            r.last_sync = Some(now_rfc3339());
        }
        for package in &manifest.packages {
            cfg.plugins
                .installed_hashes
                .insert(package.id.clone(), package.sha256.clone());
        }
    }
    for plugin_id in &removed_plugin_ids {
        purge_plugin_state(plugin_id, &app).await;
    }
    app.request_config_save();
    apply_repo_plugins(app, plugin_ids).await
}

fn official_repo_slug(slug: &str) -> bool {
    slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::usecases::plugins::reload_registry;

    #[cfg(feature = "dev-plugin-repo")]
    #[tokio::test]
    async fn development_override_blocks_official_updates() {
        crate::test_support::with_tmp_config(|app| async move {
            let dev = tempfile::tempdir().unwrap();
            *app.development_plugin_repo.write().await = Some(dev.path().to_path_buf());

            let error = update_repo(crate::constants::OFFICIAL_PLUGIN_REPO_SLUG.to_owned(), app)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("--dev-plugin-repo is active"),
                "{error:#}"
            );
        })
        .await;
    }

    /// A `file://` URL for a local path — `add_repo`/`clone` now require an
    /// explicit scheme, so tests clone local source repos through one.
    fn file_url(path: &std::path::Path) -> String {
        url::Url::from_file_path(path)
            .expect("temporary repository path must be absolute")
            .into()
    }

    fn source_package(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
        dir.join("plugins").join(id)
    }

    fn refresh_repository_index(repo: &git2::Repository) {
        let root = repo.workdir().unwrap();
        let package = std::fs::read_dir(root.join("plugins"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let id = package.file_name().unwrap().to_string_lossy();
        let version = std::fs::read(package.join("plugin.yaml"))
            .ok()
            .and_then(|bytes| serde_yaml::from_slice::<serde_yaml::Value>(&bytes).ok())
            .and_then(|value| value.get("version")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| "1.0.0".to_owned());
        let digest = crate::plugin::repo::package_hash(&package).unwrap();
        std::fs::write(
            root.join("repository.yaml"),
            format!(
                "schema: 1\nid: test-repo\nname: Test repository\nversion: 1.0.0\ncompatibility:\n  halod: '>=0.0.0'\n  plugin_api: 2\npackages:\n  - id: {id}\n    path: plugins/{id}\n    version: {version}\n    sha256: {digest}\n"
            ),
        )
        .unwrap();
    }

    /// Init a current indexed repository with one package. Root-level hard
    /// links keep older mutation-oriented cases concise while the daemon loads
    /// only the indexed package under `plugins/<id>`.
    fn init_source_repo(dir: &std::path::Path, id: &str) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let repo = git2::Repository::init(dir).unwrap();
        let package = source_package(dir, id);
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("plugin.yaml"),
            format!(
                "id: {id}\nversion: 1.0.0\npermissions: [hid]\ndevices:\n  - vendor: x\n    model: y\n    match:\n      hid: {{ vid: 1, pid: 2 }}\n"
            ),
        )
        .unwrap();
        std::fs::write(package.join("main.lua"), "return {}").unwrap();
        std::fs::hard_link(package.join("plugin.yaml"), dir.join("plugin.yaml")).unwrap();
        std::fs::hard_link(package.join("main.lua"), dir.join("main.lua")).unwrap();
        commit_all(&repo, "initial")
    }

    fn commit_all(repo: &git2::Repository, message: &str) -> String {
        refresh_repository_index(repo);
        commit_tree(repo, message)
    }

    fn commit_with_repository_compatibility(
        repo: &git2::Repository,
        message: &str,
        halod: &str,
        plugin_api: u32,
    ) -> String {
        refresh_repository_index(repo);
        let path = repo.workdir().unwrap().join("repository.yaml");
        let current = std::fs::read_to_string(&path).unwrap();
        let current = current
            .replace("halod: '>=0.0.0'", &format!("halod: '{halod}'"))
            .replace("plugin_api: 2", &format!("plugin_api: {plugin_api}"));
        std::fs::write(path, current).unwrap();
        commit_tree(repo, message)
    }

    fn commit_tree(repo: &git2::Repository, message: &str) -> String {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let parents: Vec<git2::Commit> = match repo.head() {
            Ok(head) => vec![head.peel_to_commit().unwrap()],
            Err(_) => vec![],
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
            .unwrap();
        oid.to_string()
    }

    fn sign_and_commit(repo: &git2::Repository, key_id: &str, seed: [u8; 32]) -> String {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        use ed25519_dalek::{Signer as _, SigningKey};

        refresh_repository_index(repo);
        let root = repo.workdir().unwrap();
        let key = SigningKey::from_bytes(&seed);
        let mut manifest = repo::read_repository_index(root).unwrap();
        manifest.signing_key = Some(halod_plugin_signing::RepositorySigningKey {
            id: key_id.to_owned(),
            algorithm: halod_plugin_signing::SIGNATURE_ALGORITHM.to_owned(),
            public_key: B64.encode(key.verifying_key().to_bytes()),
        });
        let payload = halod_plugin_signing::canonical_index_bytes(&manifest).unwrap();
        std::fs::write(root.join("repository.yaml"), &payload).unwrap();
        let signature = key.sign(&payload).to_bytes();
        std::fs::write(
            root.join("repository.sig"),
            halod_plugin_signing::signature_bytes(key_id, &signature),
        )
        .unwrap();
        commit_tree(repo, "signed repository")
    }

    #[tokio::test]
    async fn signed_repository_pins_first_key_and_rejects_replacement() {
        crate::test_support::with_tmp_config(|app| async move {
            let source = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(source.path()));
            init_source_repo(source.path(), &slug);
            let source_repo = git2::Repository::open(source.path()).unwrap();
            sign_and_commit(&source_repo, "publisher-1", [1; 32]);

            add_repo(file_url(source.path()), None, app.clone())
                .await
                .unwrap();
            let pinned = app.config.read().await.plugins.repos[0]
                .trusted_key
                .clone()
                .unwrap();
            assert_eq!(pinned.id, "publisher-1");

            let active = {
                let cfg = app.config.read().await;
                repo::active_revision_dir(&cfg.plugins.repos[0])
            };
            std::fs::write(active.join("repository.sig"), "invalid signature").unwrap();
            reload_registry(&app).await;
            let failed = app
                .registry
                .list(app.secret_store.as_ref())
                .into_iter()
                .find(|plugin| plugin.id == slug)
                .unwrap();
            assert!(matches!(
                failed.health.issue,
                Some(halod_shared::types::PluginIssue {
                    kind: halod_shared::types::PluginIssueKind::LoadFailed,
                    ..
                })
            ));

            update_repo(slug.clone(), app.clone()).await.unwrap();
            let restored = app
                .registry
                .list(app.secret_store.as_ref())
                .into_iter()
                .find(|plugin| plugin.id == slug)
                .unwrap();
            assert!(restored.health.issue.is_none());

            std::fs::write(
                source_package(source.path(), &slug).join("main.lua"),
                "return { changed = true }",
            )
            .unwrap();
            sign_and_commit(&source_repo, "publisher-2", [2; 32]);

            let error = update_repo(slug, app).await.unwrap_err();
            assert!(
                error.to_string().contains("signing key changed"),
                "{error:#}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn failed_add_removes_clone_and_allows_retry() {
        crate::test_support::with_tmp_config(|app| async move {
            let source = tempfile::tempdir().unwrap();
            let url = file_url(source.path());
            let slug = super::sanitize_slug(&url);
            init_source_repo(source.path(), &slug);
            let source_repo = git2::Repository::open(source.path()).unwrap();
            sign_and_commit(&source_repo, "publisher-1", [1; 32]);

            // Change the signed payload without replacing repository.sig.
            let manifest = source.path().join("repository.yaml");
            let contents = std::fs::read_to_string(&manifest).unwrap();
            std::fs::write(
                &manifest,
                contents.replace("Test repository", "Tampered repository"),
            )
            .unwrap();
            commit_tree(&source_repo, "invalidate signature");

            // Simulate debris from an older failed implementation as well.
            let destination = crate::config::plugin_repos_dir().join(&slug);
            std::fs::create_dir_all(&destination).unwrap();
            std::fs::write(destination.join("stale"), "stale").unwrap();

            let error = add_repo(url.clone(), None, app.clone()).await.unwrap_err();
            assert!(error.to_string().contains("signature"), "{error:#}");
            assert!(!destination.exists(), "failed add clone must be removed");
            assert!(app.config.read().await.plugins.repos.is_empty());
            assert!(std::fs::read_dir(crate::config::plugin_repos_dir())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".adding-")));

            sign_and_commit(&source_repo, "publisher-1", [1; 32]);
            add_repo(url, None, app.clone()).await.unwrap();
            assert!(destination.is_dir());
            assert_eq!(app.config.read().await.plugins.repos.len(), 1);
        })
        .await;
    }

    fn append_repository_tar<W: std::io::Write>(source: &std::path::Path, writer: W) {
        let mut archive = tar::Builder::new(writer);
        archive
            .append_path_with_name(source.join("repository.yaml"), "repo/repository.yaml")
            .unwrap();
        if source.join("repository.sig").is_file() {
            archive
                .append_path_with_name(source.join("repository.sig"), "repo/repository.sig")
                .unwrap();
        }
        archive
            .append_dir_all("repo/plugins", source.join("plugins"))
            .unwrap();
        archive.finish().unwrap();
    }

    fn write_repository_tar(source: &std::path::Path, output: &std::path::Path) {
        append_repository_tar(source, std::fs::File::create(output).unwrap());
    }

    #[tokio::test]
    async fn imports_a_complete_local_repository_archive() {
        crate::test_support::with_tmp_config(|app| async move {
            let source = tempfile::tempdir().unwrap();
            init_source_repo(source.path(), "archive-demo");
            let archive_dir = tempfile::tempdir().unwrap();
            let archive = archive_dir.path().join("plugins.tar");
            write_repository_tar(source.path(), &archive);

            import_local_repo(archive.to_string_lossy().into_owned(), app.clone())
                .await
                .unwrap();

            let cfg = app.config.read().await;
            let record = cfg
                .plugins
                .repos
                .iter()
                .find(|record| record.repository_id.as_deref() == Some("test-repo"))
                .unwrap();
            assert_eq!(
                record.source_kind,
                crate::config::PluginRepoSourceKind::Archive
            );
            assert!(record.trusted_key.is_none());
            assert!(repo::active_revision_dir(record)
                .join("repository.yaml")
                .is_file());
        })
        .await;
    }

    #[tokio::test]
    async fn imports_a_signed_gzip_repository_archive_and_pins_its_key() {
        crate::test_support::with_tmp_config(|app| async move {
            let source = tempfile::tempdir().unwrap();
            init_source_repo(source.path(), "signed-archive");
            let repository = git2::Repository::open(source.path()).unwrap();
            sign_and_commit(&repository, "archive-publisher", [3; 32]);
            let archive_dir = tempfile::tempdir().unwrap();
            let archive = archive_dir.path().join("plugins.tar.gz");
            let encoder = flate2::write::GzEncoder::new(
                std::fs::File::create(&archive).unwrap(),
                flate2::Compression::default(),
            );
            append_repository_tar(source.path(), encoder);

            import_local_repo(archive.to_string_lossy().into_owned(), app.clone())
                .await
                .unwrap();

            let cfg = app.config.read().await;
            let record = cfg
                .plugins
                .repos
                .iter()
                .find(|record| record.repository_id.as_deref() == Some("test-repo"))
                .unwrap();
            assert_eq!(
                record.trusted_key.as_ref().map(|key| key.id.as_str()),
                Some("archive-publisher")
            );
        })
        .await;
    }

    #[test]
    fn archive_import_rejects_links() {
        let source = tempfile::tempdir().unwrap();
        let archive_path = source.path().join("bad.tar");
        let file = std::fs::File::create(&archive_path).unwrap();
        let mut archive = tar::Builder::new(file);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        archive
            .append_link(&mut header, "repo/plugin.lua", "/etc/passwd")
            .unwrap();
        archive.finish().unwrap();
        let destination = tempfile::tempdir().unwrap();

        let error = extract_repository_archive(&archive_path, destination.path()).unwrap_err();
        assert!(error.to_string().contains("link or special file"));
    }

    #[test]
    fn materialize_revision_rebuilds_a_corrupted_existing_revision() {
        let source = tempfile::tempdir().unwrap();
        let id = "demo";
        init_source_repo(source.path(), id);
        let checkout_root = tempfile::tempdir().unwrap();
        let checkout = checkout_root.path().join("checkout");
        let sha = repo::clone(&file_url(source.path()), &checkout, None).unwrap();

        materialize_revision(&checkout, &sha, &repo::RepositoryTrust::Unsigned).unwrap();
        let revision = checkout.join("revisions").join(&sha);
        std::fs::write(
            revision.join("plugins").join(id).join("main.lua"),
            "return { tampered = true }",
        )
        .unwrap();
        assert!(repo::read_repository_manifest(&revision).is_err());

        materialize_revision(&checkout, &sha, &repo::RepositoryTrust::Unsigned).unwrap();

        assert!(repo::read_repository_manifest(&revision).is_ok());
        assert_eq!(
            std::fs::read_to_string(revision.join("plugins").join(id).join("main.lua")).unwrap(),
            "return {}"
        );
    }

    #[tokio::test]
    async fn add_repo_clones_and_makes_the_plugin_discoverable() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            // The plugin id inside must match the slug the clone dir is named after.
            let slug = super::sanitize_slug(&file_url(src.path()));
            init_source_repo(src.path(), &slug);

            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            let cfg = app.config.read().await;
            assert_eq!(cfg.plugins.repos.len(), 1);
            assert_eq!(cfg.plugins.repos[0].slug, slug);
            assert!(
                !cfg.plugins.enabled.contains(&slug),
                "a plugin from a user-added repo must start disabled"
            );
            drop(cfg);

            let plugins = app.registry.list(&*app.secret_store);
            let plugin = plugins.iter().find(|p| p.id == slug);
            assert!(
                plugin.is_some(),
                "repo-sourced plugin should be discoverable after add_repo"
            );
            assert!(!plugin.unwrap().enabled);

            let authority = app.registry.authority_for(&slug).unwrap();
            crate::plugin::usecases::plugins::confirm_enable(slug.clone(), authority, app.clone())
                .await
                .unwrap();
            let plugins = app.registry.list(&*app.secret_store);
            assert!(plugins.iter().find(|p| p.id == slug).unwrap().enabled);
        })
        .await;
    }

    #[tokio::test]
    async fn add_repo_materializes_the_indexed_revision() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            let tip_sha = init_source_repo(src.path(), &slug);

            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            let cfg = app.config.read().await;
            assert_eq!(cfg.plugins.repos[0].locked_sha, tip_sha);
            assert_eq!(
                cfg.plugins.repos[0].active_revision.as_deref(),
                Some(tip_sha.as_str())
            );
            let active = repo::active_revision_dir(&cfg.plugins.repos[0]);
            drop(cfg);
            assert!(active.join("repository.yaml").is_file());
            assert!(active
                .join("plugins")
                .join(&slug)
                .join("plugin.yaml")
                .is_file());
            assert!(app
                .registry
                .list(&*app.secret_store)
                .iter()
                .any(|plugin| { plugin.id == slug && plugin.health.issue.is_none() }));
        })
        .await;
    }

    #[tokio::test]
    async fn check_repo_updates_reports_behind_after_a_new_upstream_commit() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            let first_sha = init_source_repo(src.path(), &slug);

            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            let statuses = compute_repo_updates(&app).await;
            let status = statuses.iter().find(|s| s.slug == slug).unwrap();
            assert!(!status.behind, "no upstream change yet");
            assert_eq!(status.locked_sha, first_sha);

            // Advance the upstream repo with a second commit.
            let repo = git2::Repository::open(src.path()).unwrap();
            std::fs::write(src.path().join("main.lua"), "return { extra = true }").unwrap();
            let second_sha = commit_all(&repo, "second");
            assert_ne!(first_sha, second_sha);

            let statuses = compute_repo_updates(&app).await;
            let status = statuses.iter().find(|s| s.slug == slug).unwrap();
            assert!(status.behind, "remote tip moved past locked_sha");
            assert_eq!(status.remote_sha, second_sha);
            assert_eq!(
                status.locked_sha, first_sha,
                "check must not advance locked_sha"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn signature_status_is_independent_of_repository_compatibility() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            init_source_repo(src.path(), "unsigned");
            let source = git2::Repository::open(src.path()).unwrap();
            commit_with_repository_compatibility(
                &source,
                "requires future plugin API",
                ">=999.0.0",
                999,
            );
            let slug = crate::constants::OFFICIAL_PLUGIN_REPO_SLUG.to_owned();
            let dest = crate::config::plugin_repos_dir().join(&slug);
            let locked_sha = repo::clone(&file_url(src.path()), &dest, None).unwrap();
            app.config
                .write()
                .await
                .plugins
                .repos
                .push(PluginRepoRecord {
                    url: file_url(src.path()),
                    slug: slug.clone(),
                    repository_id: None,
                    trusted_key: None,
                    source_kind: crate::config::PluginRepoSourceKind::Git,
                    branch: None,
                    locked_sha,
                    active_revision: None,
                    active_source: crate::config::PluginRevisionSource::Managed,
                    previous_verified_sha: None,
                    last_sync: None,
                });

            compute_repo_updates(&app).await;

            let statuses = app.repo_signature_status.lock().await;
            let (_, status) = statuses.get(&slug).expect("official status was recorded");
            match status {
                halod_shared::types::RepoSignatureStatus::Invalid { reason } => {
                    assert!(reason.contains("repository.sig"), "{reason}");
                    assert!(!reason.contains("plugin API"), "{reason}");
                }
                other => panic!("expected invalid signature, got {other:?}"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn touch_last_sync_advances_only_the_reached_repos() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            init_source_repo(src.path(), &slug);
            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            // Force a stale sentinel so a fresh stamp is detectable.
            const STALE: &str = "2000-01-01T00:00:00+00:00";
            {
                let mut cfg = app.config.write().await;
                for r in cfg.plugins.repos.iter_mut() {
                    r.last_sync = Some(STALE.to_owned());
                }
            }

            touch_last_sync(&app, std::slice::from_ref(&slug)).await;

            let cfg = app.config.read().await;
            let r = cfg.plugins.repos.iter().find(|r| r.slug == slug).unwrap();
            assert_ne!(
                r.last_sync.as_deref(),
                Some(STALE),
                "a reached repo's last_sync must advance on check"
            );
            assert!(r.last_sync.is_some());
        })
        .await;
    }

    #[tokio::test]
    async fn update_repo_advances_locked_sha_without_revoking_permissions() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            init_source_repo(src.path(), &slug);

            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            let hash_before = app
                .config
                .read()
                .await
                .plugins
                .installed_hashes
                .get(&slug)
                .cloned()
                .unwrap();

            // Advance the upstream repo with a content change.
            let repo = git2::Repository::open(src.path()).unwrap();
            std::fs::write(src.path().join("main.lua"), "return { extra = true }").unwrap();
            let second_sha = commit_all(&repo, "second");

            update_repo(slug.clone(), app.clone()).await.unwrap();

            let cfg = app.config.read().await;
            let record = cfg.plugins.repos.iter().find(|r| r.slug == slug).unwrap();
            assert_eq!(
                record.locked_sha, second_sha,
                "locked_sha advances to the new tip"
            );

            let hash_after = cfg.plugins.installed_hashes.get(&slug).unwrap();
            assert_ne!(
                &hash_before, hash_after,
                "content_hash must change once the script content changed"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn mutable_git_worktree_is_never_loaded_as_plugin_input() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            init_source_repo(src.path(), &slug);

            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            let worktree_package = crate::config::plugin_repos_dir()
                .join(&slug)
                .join("plugins")
                .join(&slug);
            std::fs::write(
                worktree_package.join("plugin.yaml"),
                "id: broken\nid: duplicate\n",
            )
            .unwrap();
            reload_registry(&app).await;
            assert!(
                app.registry
                    .list(&*app.secret_store)
                    .iter()
                    .any(|p| p.id == slug && p.health.issue.is_none()),
                "registry reload must continue reading the immutable active revision"
            );
            let cfg = app.config.read().await;
            let record = cfg.plugins.repos.iter().find(|r| r.slug == slug).unwrap();
            assert!(
                repo::active_revision_dir(record)
                    .join("plugins")
                    .join(&slug)
                    .join("plugin.yaml")
                    .is_file(),
                "the selected immutable package remains intact"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn update_all_plugins_broadcasts_a_cleared_update_flag() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            init_source_repo(src.path(), &slug);
            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            // Advance the upstream repo so the plugin has an update available.
            let repo = git2::Repository::open(src.path()).unwrap();
            std::fs::write(src.path().join("main.lua"), "return { extra = true }").unwrap();
            commit_all(&repo, "second");
            let (before, _) = compute_plugin_updates(&app, Some(&slug)).await;
            assert!(
                before
                    .iter()
                    .any(|s| s.plugin_id == slug && s.update_available),
                "the plugin should report an available update before updating"
            );

            // Register a client so the post-update broadcast is captured.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Arc<Vec<u8>>>(8);
            app.clients.lock().await.push(crate::ipc::ClientHandle {
                id: 0,
                tx,
                subs: Arc::default(),
            });

            update_all_plugins(app.clone()).await.unwrap();

            // Drain frames until the plugin_updates one, and assert the flag cleared.
            let mut cleared = None;
            while let Ok(frame) = rx.try_recv() {
                let msg: serde_json::Value = serde_json::from_slice(&frame[5..]).unwrap();
                if msg["type"] == "plugin_updates" {
                    cleared = msg["plugins"]
                        .as_array()
                        .and_then(|a| a.iter().find(|s| s["plugin_id"] == slug))
                        .map(|s| s["update_available"].as_bool().unwrap());
                }
            }
            assert_eq!(
                cleared,
                Some(false),
                "update_all_plugins must broadcast a plugin_updates frame clearing the flag"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn update_plugin_keeps_the_checkout_unchanged_when_validation_fails() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            let first_sha = init_source_repo(src.path(), &slug);
            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            // Publish an upstream commit whose plugin.yaml id no longer matches
            // the directory name — parse_manifest_from_dir rejects that.
            let repo = git2::Repository::open(src.path()).unwrap();
            std::fs::write(
                src.path().join("plugin.yaml"),
                "id: not-the-slug\nversion: 1.0.0\npermissions: [hid]\ndevices:\n  - vendor: x\n    model: y\n    match:\n      hid: { vid: 1, pid: 2 }\n",
            )
            .unwrap();
            let second_sha = commit_all(&repo, "broken");
            assert_ne!(first_sha, second_sha);

            let err = update_repo(slug.clone(), app.clone()).await.unwrap_err();
            assert!(
                format!("{err:#}").contains("does not match its plugin.yaml id"),
                "{err:#}"
            );

            let cfg = app.config.read().await;
            let r = cfg.plugins.repos.iter().find(|r| r.slug == slug).unwrap();
            assert_eq!(
                r.locked_sha, first_sha,
                "a failed update must not advance locked_sha"
            );
            let active = repo::active_revision_dir(r);
            drop(cfg);

            // Validation happened in staging, so the installed working tree was
            // never replaced with the invalid content.
            let manifest = crate::plugin::parse_manifest_from_dir(
                &active.join("plugins").join(&slug),
            )
            .expect("the reverted checkout must parse again");
            assert_eq!(manifest.plugin_id, slug);
        })
        .await;
    }

    #[tokio::test]
    async fn add_repo_does_not_disable_a_preexisting_plugin_id() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            init_source_repo(src.path(), &slug);

            let local_dir = crate::config::plugins_dir().join(&slug);
            std::fs::create_dir_all(&local_dir).unwrap();
            std::fs::copy(
                src.path().join("plugin.yaml"),
                local_dir.join("plugin.yaml"),
            )
            .unwrap();
            std::fs::copy(src.path().join("main.lua"), local_dir.join("main.lua")).unwrap();
            app.registry.load_all(&crate::config::plugins_dir());
            assert!(app
                .registry
                .list(&*app.secret_store)
                .iter()
                .any(|plugin| plugin.id == slug));

            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            assert!(
                !app.config.read().await.plugins.enabled.contains(&slug),
                "an id collision must not disable the pre-existing plugin owner"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn incompatible_remote_repository_keeps_latest_compatible_revision() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            let first_sha = init_source_repo(src.path(), &slug);
            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            let repo = git2::Repository::open(src.path()).unwrap();
            let tip_sha = commit_with_repository_compatibility(
                &repo,
                "requires future plugin API",
                ">=0.0.0",
                crate::plugin::PLUGIN_API + 1,
            );

            let repo_statuses = compute_repo_updates(&app).await;
            assert_eq!(repo_statuses[0].remote_sha, first_sha);
            assert!(!repo_statuses[0].behind);

            let (statuses, _) = compute_plugin_updates(&app, Some(&slug)).await;
            assert!(
                statuses.iter().all(|status| !status.update_available),
                "an incompatible tip is not advertised as an update"
            );

            update_repo(slug.clone(), app.clone()).await.unwrap();
            let config = app.config.read().await;
            let record = config
                .plugins
                .repos
                .iter()
                .find(|r| r.slug == slug)
                .unwrap();
            assert_ne!(tip_sha, first_sha);
            assert_eq!(record.locked_sha, first_sha);
            assert_eq!(record.active_revision.as_deref(), Some(first_sha.as_str()));
        })
        .await;
    }

    #[tokio::test]
    async fn add_repo_selects_latest_compatible_commit_behind_tip() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            let compatible_sha = init_source_repo(src.path(), &slug);
            let source = git2::Repository::open(src.path()).unwrap();
            let tip_sha = commit_with_repository_compatibility(
                &source,
                "requires future plugin API",
                ">=0.0.0",
                crate::plugin::PLUGIN_API + 1,
            );

            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            let config = app.config.read().await;
            let record = &config.plugins.repos[0];
            assert_ne!(tip_sha, compatible_sha);
            assert_eq!(record.locked_sha, compatible_sha);
            assert_eq!(
                record.active_revision.as_deref(),
                Some(compatible_sha.as_str())
            );
        })
        .await;
    }

    #[tokio::test]
    async fn invalid_checked_out_plugin_can_update_to_a_valid_remote_revision() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            init_source_repo(src.path(), &slug);
            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            // Simulate a previously valid plugin becoming invalid under newer
            // Halo validation while a compatible fix is available upstream.
            let record = app
                .config
                .read()
                .await
                .plugins
                .repos
                .iter()
                .find(|record| record.slug == slug)
                .cloned()
                .unwrap();
            let active_package = repo::active_revision_dir(&record)
                .join("plugins")
                .join(&slug);
            std::fs::write(
                active_package.join("plugin.yaml"),
                "id: broken\nid: duplicate\n",
            )
            .unwrap();
            reload_registry(&app).await;

            let failed = app
                .registry
                .list(&*app.secret_store)
                .into_iter()
                .find(|plugin| plugin.id == slug)
                .expect("invalid manifest remains visible for recovery");
            assert_eq!(
                failed.health.issue.map(|issue| issue.kind),
                Some(halod_shared::types::PluginIssueKind::LoadFailed)
            );

            let upstream = git2::Repository::open(src.path()).unwrap();
            std::fs::write(src.path().join("main.lua"), "return { fixed = true }").unwrap();
            commit_all(&upstream, "compatible fix");

            let (statuses, _) = compute_plugin_updates(&app, Some(&slug)).await;
            let status = statuses
                .iter()
                .find(|status| status.plugin_id == slug)
                .expect("failed repo plugin still receives update status");
            assert!(status.update_available);

            update_repo(slug.clone(), app.clone()).await.unwrap();

            let recovered = app
                .registry
                .list(&*app.secret_store)
                .into_iter()
                .find(|plugin| plugin.id == slug)
                .expect("updated plugin is loaded again");
            assert!(recovered.health.issue.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn compute_plugin_updates_flags_a_local_edit_as_changed_not_an_update() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            init_source_repo(src.path(), &slug);
            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            // No upstream change, but the checked-out working-tree file is edited.
            let record = app
                .config
                .read()
                .await
                .plugins
                .repos
                .iter()
                .find(|record| record.slug == slug)
                .cloned()
                .unwrap();
            let active_main = repo::active_revision_dir(&record)
                .join("plugins")
                .join(&slug)
                .join("main.lua");
            std::fs::write(&active_main, "return { hacked = true }").unwrap();
            reload_registry(&app).await;

            let (statuses, _) = compute_plugin_updates(&app, Some(&slug)).await;
            let s = statuses.iter().find(|s| s.plugin_id == slug).unwrap();
            assert!(
                s.on_disk_changed,
                "a local edit to the checked-out file must be flagged as changed on disk"
            );
            assert!(
                !s.update_available,
                "a local edit with no upstream change is not an available update"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn compute_on_disk_changes_detects_a_local_edit_without_a_remote() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            init_source_repo(src.path(), &slug);
            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            // Nothing changed on disk yet.
            assert!(
                compute_on_disk_changes(&app).await.is_empty(),
                "a pristine checkout reports no on-disk changes"
            );

            // Edit the checked-out file, then drop the upstream so no network is
            // reachable — the local check must still flag the change.
            let record = app
                .config
                .read()
                .await
                .plugins
                .repos
                .iter()
                .find(|record| record.slug == slug)
                .cloned()
                .unwrap();
            let active_main = repo::active_revision_dir(&record)
                .join("plugins")
                .join(&slug)
                .join("main.lua");
            std::fs::write(&active_main, "return { hacked = true }").unwrap();
            reload_registry(&app).await;
            drop(src);

            let changed = compute_on_disk_changes(&app).await;
            assert_eq!(changed.len(), 1);
            assert_eq!(changed[0].plugin_id, slug);
            assert!(changed[0].on_disk_changed);
            assert!(!changed[0].update_available);
        })
        .await;
    }

    #[tokio::test]
    async fn remove_repo_deletes_the_clone_dir_and_purges_plugin_state() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            init_source_repo(src.path(), &slug);

            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();
            {
                let mut cfg = app.config.write().await;
                cfg.plugins.enabled.push(slug.clone());
            }
            app.registry
                .replace_policy(&app.config.read().await.plugins);

            let clone_dir = crate::config::plugin_repos_dir().join(&slug);
            assert!(clone_dir.exists());

            remove_repo(slug.clone(), app.clone()).await.unwrap();

            assert!(!clone_dir.exists(), "clone directory must be removed");
            let cfg = app.config.read().await;
            assert!(cfg.plugins.repos.is_empty());
            assert!(
                !cfg.plugins.enabled.contains(&slug),
                "the removed plugin's disabled flag must be purged"
            );
            drop(cfg);
            let plugins = app.registry.list(&*app.secret_store);
            assert!(!plugins.iter().any(|p| p.id == slug));
        })
        .await;
    }

    #[tokio::test]
    async fn remove_repo_rejects_the_official_slug() {
        crate::test_support::with_tmp_config(|app| async move {
            let err = remove_repo(
                crate::constants::OFFICIAL_PLUGIN_REPO_SLUG.to_owned(),
                app.clone(),
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains("cannot be removed"));
        })
        .await;
    }

    #[tokio::test]
    async fn update_check_skips_a_repo_that_was_never_cloned() {
        crate::test_support::with_tmp_config(|app| async move {
            // Seed a repo record whose clone dir was never created (e.g. an
            // offline first launch where the initial clone failed). Background
            // update checks must skip it silently instead of trying to open a
            // missing repo and logging a fetch failure on every cycle.
            {
                let mut cfg = app.config.write().await;
                cfg.plugins.repos.push(PluginRepoRecord {
                    url: "https://example.invalid/repo".to_owned(),
                    slug: "never-cloned".to_owned(),
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
            }

            let statuses = compute_repo_updates(&app).await;
            assert!(
                !statuses.iter().any(|s| s.slug == "never-cloned"),
                "an uncloned repo must be skipped by the repo update check"
            );

            let (plugin_statuses, reached) = compute_plugin_updates(&app, None).await;
            assert!(
                !reached.iter().any(|s| s == "never-cloned"),
                "an uncloned repo must not be reported as reached"
            );
            assert!(
                plugin_statuses.iter().all(|s| s.slug != "never-cloned"),
                "an uncloned repo yields no plugin update statuses"
            );
            assert!(
                compute_on_disk_changes(&app).await.is_empty(),
                "an uncloned repo has no active revision to inspect"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn add_repo_rejects_a_slug_that_collides_with_official() {
        crate::test_support::with_tmp_config(|app| async move {
            // sanitize_slug("official") == "official" — a URL that sanitizes
            // to the reserved slug must be rejected outright.
            let err = add_repo("official".to_owned(), None, app.clone())
                .await
                .unwrap_err();
            assert!(err.to_string().contains("reserved"));
        })
        .await;
    }
}
