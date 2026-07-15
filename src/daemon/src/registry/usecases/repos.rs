// SPDX-License-Identifier: GPL-3.0-or-later
//! Managing registered git-repo plugin sources: add, remove, check for updates, and update.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::json;

use crate::config::PluginRepoRecord;
use crate::drivers::plugins::repo;
use crate::ipc::ClientHandle;
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
    official: bool,
) -> Result<repo::RepositoryManifest> {
    let revisions = repo_dir.join("revisions");
    let final_dir = revisions.join(sha);
    let validate = |dir: &std::path::Path| {
        if official {
            repo::verify_official_repository(dir)
        } else {
            repo::read_repository_manifest(dir)
        }
    };
    if final_dir.is_dir() {
        return validate(&final_dir);
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
    std::fs::rename(&staging, &final_dir).with_context(|| {
        format!(
            "activating immutable revision {} from {}",
            final_dir.display(),
            staging.display()
        )
    })?;
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
    let known_plugin_ids: std::collections::HashSet<String> = app
        .registry
        .list(&*app.secret_store)
        .into_iter()
        .map(|plugin| plugin.id)
        .collect();

    let dest = crate::config::plugin_repos_dir().join(&slug);
    let locked_sha = {
        let url = url.clone();
        let dest = dest.clone();
        let branch = branch.clone();
        tokio::task::spawn_blocking(move || repo::clone(&url, &dest, branch.as_deref()))
            .await
            .context("clone task panicked")??
    };

    let manifest = {
        let dest = dest.clone();
        let sha = locked_sha.clone();
        tokio::task::spawn_blocking(move || materialize_revision(&dest, &sha, false))
            .await
            .context("repository manifest validation task panicked")??
    };
    let packages = manifest.packages;
    let active_revision = locked_sha.clone();
    let plugin_ids: Vec<String> = packages.iter().map(|package| package.id.clone()).collect();

    {
        let mut cfg = app.config.write().await;
        cfg.plugins.repos.push(PluginRepoRecord {
            url,
            slug,
            repository_id: Some(manifest.id.clone()),
            branch,
            locked_sha,
            active_revision: Some(active_revision),
            previous_verified_sha: None,
            last_sync: Some(now_rfc3339()),
        });
        let _ = &known_plugin_ids;
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
    let plugin_ids = crate::drivers::plugins::repo_plugin_ids(&repo::active_revision_dir(&record));
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
        let result =
            tokio::task::spawn_blocking(move || repo::fetch_remote_sha(&dir, branch.as_deref()))
                .await;
        match result {
            Ok(Ok(remote_sha)) => {
                let behind = remote_sha != r.locked_sha;
                out.push(RepoUpdateStatus {
                    slug: r.slug,
                    locked_sha: r.locked_sha,
                    remote_sha,
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

        let official = r.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG;
        let result = {
            let dir = dir.clone();
            let remote_sha = remote_sha.clone();
            tokio::task::spawn_blocking(move || {
                if official {
                    repo::verify_official_repository_at_commit(&dir, &remote_sha)
                } else {
                    repo::read_repository_manifest_at_commit(&dir, &remote_sha)
                }
            })
            .await
        };
        match result {
            Ok(Ok(manifest)) => {
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
            Ok(Err(e)) => log::warn!("reading remote repository index for '{}': {e:#}", r.slug),
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
async fn broadcast_plugin_updates(app: &Arc<AppState>, slug_filter: Option<&str>) {
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
    let official = slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG;
    let manifest = {
        let dir = dir.clone();
        let sha = remote_sha.clone();
        tokio::task::spawn_blocking(move || materialize_revision(&dir, &sha, official))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::usecases::plugins::reload_registry;

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
        let digest = crate::drivers::plugins::repo::package_hash(&package).unwrap();
        std::fs::write(
            root.join("repository.yaml"),
            format!(
                "schema: 1\nid: test-repo\nname: Test repository\nversion: 1.0.0\ncompatibility:\n  halod: '>=0.0.0'\n  plugin_api: 1\npackages:\n  - id: {id}\n    path: plugins/{id}\n    version: {version}\n    sha256: {digest}\n"
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
            .replace("plugin_api: 1", &format!("plugin_api: {plugin_api}"));
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
            crate::registry::usecases::plugins::confirm_enable(
                slug.clone(),
                authority,
                app.clone(),
            )
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
            let manifest = crate::drivers::plugins::parse_manifest_from_dir(
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
    async fn incompatible_remote_repository_does_not_replace_active_revision() {
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
                2,
            );

            let (statuses, _) = compute_plugin_updates(&app, Some(&slug)).await;
            assert!(
                statuses.is_empty(),
                "an incompatible index is not advertised"
            );

            let error = update_repo(slug.clone(), app.clone()).await.unwrap_err();
            assert!(format!("{error:#}").contains("plugin API 2"));
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
                    branch: None,
                    locked_sha: String::new(),
                    active_revision: None,
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
