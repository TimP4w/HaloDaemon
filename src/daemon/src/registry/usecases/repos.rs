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

use super::plugins::{
    mark_pending_and_broadcast, purge_plugin_state, reload_registry, sanitize_slug,
};

/// RFC 3339 timestamp for `PluginRepoRecord::last_sync`.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Register a git-repo plugin source: clone it, pin `locked_sha`, persist, and rediscover.
pub async fn add_repo(url: String, branch: Option<String>, app: Arc<AppState>) -> Result<()> {
    let slug = sanitize_slug(&url);
    if slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG {
        anyhow::bail!("slug '{slug}' is reserved for the official plugin repository");
    }
    {
        let cfg = app.config.read().await;
        if cfg.plugin_repos.iter().any(|r| r.slug == slug) {
            anyhow::bail!("a repo with slug '{slug}' is already registered");
        }
    }

    let dest = crate::config::plugin_repos_dir().join(&slug);
    let locked_sha = {
        let url = url.clone();
        let dest = dest.clone();
        let branch = branch.clone();
        tokio::task::spawn_blocking(move || repo::clone(&url, &dest, branch.as_deref()))
            .await
            .context("clone task panicked")??
    };

    {
        let mut cfg = app.config.write().await;
        cfg.plugin_repos.push(PluginRepoRecord {
            url,
            slug,
            branch,
            locked_sha,
            last_sync: Some(now_rfc3339()),
        });
    }
    app.request_config_save();
    reload_registry(&app).await;
    mark_pending_and_broadcast(&app).await;
    Ok(())
}

/// Unregister a git-repo plugin source: purge its plugin ids, delete its clone dir, persist, and rediscover.
/// The official repo cannot be removed — only its content can be updated.
pub async fn remove_repo(slug: String, app: Arc<AppState>) -> Result<()> {
    if slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG {
        anyhow::bail!("the official plugin repository cannot be removed");
    }
    let repo_dir = crate::config::plugin_repos_dir().join(&slug);
    for id in crate::drivers::plugins::repo_plugin_ids(&repo_dir) {
        purge_plugin_state(&id, &app).await;
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
        cfg.plugin_repos.retain(|r| r.slug != slug);
    }
    app.request_config_save();
    reload_registry(&app).await;
    mark_pending_and_broadcast(&app).await;
    Ok(())
}

/// Fetch every registered repo's remote tip and compare to `locked_sha`; a repo whose fetch fails is logged and skipped.
async fn compute_repo_updates(app: &Arc<AppState>) -> Vec<RepoUpdateStatus> {
    let repos = app.config.read().await.plugin_repos.clone();
    let mut out = Vec::with_capacity(repos.len());
    for r in repos {
        let dir = crate::config::plugin_repos_dir().join(&r.slug);
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

/// Every repo-sourced plugin (optionally scoped to one repo `slug`), each
/// compared against its repo's *freshly fetched* remote tip via a
/// content-hash read straight out of git's object database — no checkout.
/// Finer-grained than [`compute_repo_updates`]: a repo can be behind while a
/// given plugin's own two hashed files are unchanged, so this is the correct
/// signal for a per-plugin "update available" button.
async fn compute_plugin_updates(
    app: &Arc<AppState>,
    slug_filter: Option<&str>,
) -> Vec<halod_shared::types::PluginUpdateStatus> {
    use halod_shared::types::{PluginSource, PluginUpdateStatus};

    let repos: Vec<_> = app
        .config
        .read()
        .await
        .plugin_repos
        .iter()
        .filter(|r| slug_filter.is_none_or(|s| s == r.slug))
        .cloned()
        .collect();

    let plugins = crate::drivers::plugins::list(&*app.secret_store);
    let mut out = Vec::new();
    for r in repos {
        let dir = crate::config::plugin_repos_dir().join(&r.slug);
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

        for p in plugins
            .iter()
            .filter(|p| matches!(&p.source, PluginSource::Repo { slug } if *slug == r.slug))
        {
            let Some((_, subpath)) = crate::drivers::plugins::repo_location_for(&p.id) else {
                continue;
            };
            let dir = dir.clone();
            let sha = remote_sha.clone();
            let result = tokio::task::spawn_blocking(move || {
                repo::remote_plugin_content(&dir, &sha, &subpath)
            })
            .await;
            match result {
                Ok(Ok((remote_hash, remote_version))) => {
                    let local_hash = crate::drivers::plugins::content_hash_for(&p.id);
                    let update_available = local_hash.as_deref() != Some(remote_hash.as_str());
                    out.push(PluginUpdateStatus {
                        plugin_id: p.id.clone(),
                        slug: r.slug.clone(),
                        update_available,
                        current_version: p.version.clone(),
                        available_version: remote_version,
                    });
                }
                Ok(Err(e)) => log::warn!("reading remote content for plugin '{}': {e:#}", p.id),
                Err(e) => log::warn!("remote-content task for plugin '{}' panicked: {e:#}", p.id),
            }
        }
    }
    out
}

/// Check registered repos' plugins for updates and reply with a `plugin_updates` frame.
/// `slug` scopes the check to one repo; `None` checks every repo.
pub async fn check_plugin_updates(
    slug: Option<String>,
    app: Arc<AppState>,
    client: ClientHandle,
) -> Result<()> {
    let statuses = compute_plugin_updates(&app, slug.as_deref()).await;
    client.send_json(&json!({
        "type": "plugin_updates",
        "plugins": statuses,
    }));
    Ok(())
}

/// Update one plugin: fetch its repo's remote tip and check out only that
/// plugin's subtree, leaving sibling plugins in the same repo untouched.
/// Content changes, so the existing consent model re-requires approval.
pub async fn update_plugin(plugin_id: String, app: Arc<AppState>) -> Result<()> {
    let (slug, subpath) = crate::drivers::plugins::repo_location_for(&plugin_id)
        .ok_or_else(|| anyhow::anyhow!("plugin '{plugin_id}' is not repo-sourced"))?;
    let branch = {
        let cfg = app.config.read().await;
        cfg.plugin_repos
            .iter()
            .find(|r| r.slug == slug)
            .ok_or_else(|| anyhow::anyhow!("unknown plugin repo '{slug}'"))?
            .branch
            .clone()
    };

    let dir = crate::config::plugin_repos_dir().join(&slug);
    let remote_sha = {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || repo::fetch_remote_sha(&dir, branch.as_deref()))
            .await
            .context("fetch task panicked")??
    };
    {
        let dir = dir.clone();
        let sha = remote_sha.clone();
        let subpath = subpath.clone();
        tokio::task::spawn_blocking(move || repo::checkout_subtree(&dir, &sha, &subpath))
            .await
            .context("checkout task panicked")??;
    }

    {
        let mut cfg = app.config.write().await;
        if let Some(r) = cfg.plugin_repos.iter_mut().find(|r| r.slug == slug) {
            // Per-plugin updates only guarantee *this* plugin matches the tip,
            // not the whole repo — `locked_sha` is now just the latest tip
            // we've observed, not a "fully synced" marker.
            r.locked_sha = remote_sha;
            r.last_sync = Some(now_rfc3339());
        }
    }
    app.request_config_save();
    reload_registry(&app).await;
    mark_pending_and_broadcast(&app).await;
    Ok(())
}

/// Update every plugin currently flagged as having an update available, across every repo.
pub async fn update_all_plugins(app: Arc<AppState>) -> Result<()> {
    let statuses = compute_plugin_updates(&app, None).await;
    for status in statuses.into_iter().filter(|s| s.update_available) {
        if let Err(e) = update_plugin(status.plugin_id.clone(), app.clone()).await {
            log::warn!("updating plugin '{}': {e:#}", status.plugin_id);
        }
    }
    Ok(())
}

/// Check every registered repo for updates and reply to the requesting client with a `plugin_repo_updates` frame.
pub async fn check_repo_updates(app: Arc<AppState>, client: ClientHandle) -> Result<()> {
    let statuses = compute_repo_updates(&app).await;
    client.send_json(&json!({
        "type": "plugin_repo_updates",
        "repos": statuses,
    }));
    Ok(())
}

/// Fetch and check out a repo's remote tip, advance `locked_sha`, persist, and rediscover.
pub async fn update_repo(slug: String, app: Arc<AppState>) -> Result<()> {
    let record = {
        let cfg = app.config.read().await;
        cfg.plugin_repos
            .iter()
            .find(|r| r.slug == slug)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown plugin repo '{slug}'"))?
    };

    let dir = crate::config::plugin_repos_dir().join(&slug);
    let remote_sha = {
        let dir = dir.clone();
        let branch = record.branch.clone();
        tokio::task::spawn_blocking(move || repo::fetch_remote_sha(&dir, branch.as_deref()))
            .await
            .context("fetch task panicked")??
    };
    {
        let dir = dir.clone();
        let sha = remote_sha.clone();
        tokio::task::spawn_blocking(move || repo::checkout_sha(&dir, &sha))
            .await
            .context("checkout task panicked")??;
    }

    {
        let mut cfg = app.config.write().await;
        if let Some(r) = cfg.plugin_repos.iter_mut().find(|r| r.slug == slug) {
            r.locked_sha = remote_sha;
            r.last_sync = Some(now_rfc3339());
        }
    }
    app.request_config_save();
    reload_registry(&app).await;
    mark_pending_and_broadcast(&app).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::plugins::TEST_GLOBALS_LOCK;

    /// Init a local source repo at `dir`; the plugin id must equal `dir`'s file name (see `parse_manifest_from_dir`).
    fn init_source_repo(dir: &std::path::Path, id: &str) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let repo = git2::Repository::init(dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            format!(
                "id: {id}\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
        commit_all(&repo, "initial")
    }

    fn commit_all(repo: &git2::Repository, message: &str) -> String {
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
    #[allow(clippy::await_holding_lock)]
    async fn add_repo_clones_and_makes_the_plugin_discoverable() {
        let _guard = TEST_GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            // The plugin id inside must match the slug the clone dir is named after.
            let slug = super::sanitize_slug(&src.path().to_string_lossy());
            init_source_repo(src.path(), &slug);

            add_repo(src.path().to_string_lossy().into_owned(), None, app.clone())
                .await
                .unwrap();

            let cfg = app.config.read().await;
            assert_eq!(cfg.plugin_repos.len(), 1);
            assert_eq!(cfg.plugin_repos[0].slug, slug);
            drop(cfg);

            let plugins = crate::drivers::plugins::list(&*app.secret_store);
            assert!(
                plugins.iter().any(|p| p.id == slug),
                "repo-sourced plugin should be discoverable after add_repo"
            );
        })
        .await;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn check_repo_updates_reports_behind_after_a_new_upstream_commit() {
        let _guard = TEST_GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&src.path().to_string_lossy());
            let first_sha = init_source_repo(src.path(), &slug);

            add_repo(src.path().to_string_lossy().into_owned(), None, app.clone())
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
    #[allow(clippy::await_holding_lock)]
    async fn update_repo_advances_locked_sha_and_invalidates_stale_consent() {
        let _guard = TEST_GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&src.path().to_string_lossy());
            init_source_repo(src.path(), &slug);

            add_repo(src.path().to_string_lossy().into_owned(), None, app.clone())
                .await
                .unwrap();

            // Consent to the plugin as it stands right after add_repo.
            let hash_before = crate::drivers::plugins::content_hash_for(&slug).unwrap();
            {
                let mut cfg = app.config.write().await;
                cfg.plugin_acknowledged
                    .insert(slug.clone(), hash_before.clone());
            }
            crate::drivers::plugins::set_acknowledged(&app.config.read().await.plugin_acknowledged);
            assert!(crate::drivers::plugins::content_hash_for(&slug).is_some());

            // Advance the upstream repo with a content change.
            let repo = git2::Repository::open(src.path()).unwrap();
            std::fs::write(src.path().join("main.lua"), "return { extra = true }").unwrap();
            let second_sha = commit_all(&repo, "second");

            update_repo(slug.clone(), app.clone()).await.unwrap();

            let cfg = app.config.read().await;
            let record = cfg.plugin_repos.iter().find(|r| r.slug == slug).unwrap();
            assert_eq!(
                record.locked_sha, second_sha,
                "locked_sha advances to the new tip"
            );

            let hash_after = crate::drivers::plugins::content_hash_for(&slug).unwrap();
            assert_ne!(
                hash_before, hash_after,
                "content_hash must change once the script content changed"
            );
            assert_ne!(
                cfg.plugin_acknowledged.get(&slug),
                Some(&hash_after),
                "the pre-update acknowledgment must no longer match the new content hash"
            );
        })
        .await;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn remove_repo_deletes_the_clone_dir_and_purges_plugin_state() {
        let _guard = TEST_GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&src.path().to_string_lossy());
            init_source_repo(src.path(), &slug);

            add_repo(src.path().to_string_lossy().into_owned(), None, app.clone())
                .await
                .unwrap();
            {
                let mut cfg = app.config.write().await;
                cfg.plugins_disabled.push(slug.clone());
            }
            crate::drivers::plugins::set_disabled(&app.config.read().await.plugins_disabled);

            let clone_dir = crate::config::plugin_repos_dir().join(&slug);
            assert!(clone_dir.exists());

            remove_repo(slug.clone(), app.clone()).await.unwrap();

            assert!(!clone_dir.exists(), "clone directory must be removed");
            let cfg = app.config.read().await;
            assert!(cfg.plugin_repos.is_empty());
            assert!(
                !cfg.plugins_disabled.contains(&slug),
                "the removed plugin's disabled flag must be purged"
            );
            drop(cfg);
            let plugins = crate::drivers::plugins::list(&*app.secret_store);
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
