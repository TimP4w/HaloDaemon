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

/// A plugin's checked-out baseline hash from the repo's git index; `None` if unread.
async fn plugin_index_content(dir: &std::path::Path, subpath: &std::path::Path) -> Option<String> {
    let dir = dir.to_path_buf();
    let subpath = subpath.to_path_buf();
    match tokio::task::spawn_blocking(move || repo::index_plugin_content(&dir, &subpath)).await {
        Ok(Ok(hash)) => Some(hash),
        _ => None,
    }
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
) -> (Vec<halod_shared::types::PluginUpdateStatus>, Vec<String>) {
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

    let plugins = app.registry.list(&*app.secret_store);
    let mut out = Vec::new();
    let mut reached = Vec::new();
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
        reached.push(r.slug.clone());

        for p in plugins
            .iter()
            .filter(|p| matches!(&p.source, PluginSource::Repo { slug } if *slug == r.slug))
        {
            let Some((_, subpath)) = app.registry.repo_location_for(&p.id) else {
                continue;
            };
            let result = {
                let dir = dir.clone();
                let sha = remote_sha.clone();
                let subpath = subpath.clone();
                tokio::task::spawn_blocking(move || {
                    repo::remote_plugin_content(&dir, &sha, &subpath)
                })
                .await
            };
            match result {
                Ok(Ok((remote_hash, remote_version))) => {
                    let local_hash = app.registry.content_hash_for(&p.id);
                    // Compare the checked-out baseline (not the live file) to the
                    // remote, so a local edit isn't mistaken for an update.
                    let index_hash = plugin_index_content(&dir, &subpath).await;
                    let update_available = match &index_hash {
                        Some(ih) => *ih != remote_hash,
                        None => local_hash.as_deref() != Some(remote_hash.as_str()),
                    };
                    let on_disk_changed = match (&local_hash, &index_hash) {
                        (Some(local), Some(index)) => local != index,
                        _ => false,
                    };
                    out.push(PluginUpdateStatus {
                        plugin_id: p.id.clone(),
                        slug: r.slug.clone(),
                        update_available,
                        on_disk_changed,
                        current_version: p.version.clone(),
                        available_version: remote_version,
                    });
                }
                Ok(Err(e)) => log::warn!("reading remote content for plugin '{}': {e:#}", p.id),
                Err(e) => log::warn!("remote-content task for plugin '{}' panicked: {e:#}", p.id),
            }
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
            .plugin_repos
            .iter_mut()
            .filter(|r| slugs.contains(&r.slug))
        {
            r.last_sync = Some(now_rfc3339());
        }
    }
    app.request_config_save();
    crate::ipc::broadcast_state(app).await;
}

/// Check registered repos' plugins for updates and reply with a `plugin_updates` frame.
/// `slug` scopes the check to one repo; `None` checks every repo.
pub async fn check_plugin_updates(
    slug: Option<String>,
    app: Arc<AppState>,
    client: ClientHandle,
) -> Result<()> {
    let (statuses, reached) = compute_plugin_updates(&app, slug.as_deref()).await;
    // A check contacts the remote, so it counts as a sync — stamp the reached
    // repos even when nothing was behind, so "LAST SYNC" isn't stuck at "never".
    touch_last_sync(&app, &reached).await;
    *app.plugin_update_status.lock().await = statuses.clone();
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
    let slug = update_plugin_inner(plugin_id, &app).await?;
    // The plugin now matches its remote tip, so its "update available" flag has
    // gone stale in every client — recompute and push a fresh frame so the
    // update banner disappears.
    broadcast_plugin_updates(&app, Some(&slug)).await;
    Ok(())
}

async fn update_plugin_inner(plugin_id: String, app: &Arc<AppState>) -> Result<String> {
    let (slug, subpath) = app
        .registry
        .repo_location_for(&plugin_id)
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
    reload_registry(app).await;
    mark_pending_and_broadcast(app).await;
    Ok(slug)
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
    for status in statuses.into_iter().filter(|s| s.update_available) {
        if let Err(e) = update_plugin_inner(status.plugin_id.clone(), &app).await {
            log::warn!("updating plugin '{}': {e:#}", status.plugin_id);
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

/// Every repo plugin whose on-disk content differs from its git-index baseline.
async fn compute_on_disk_changes(
    app: &Arc<AppState>,
) -> Vec<halod_shared::types::PluginUpdateStatus> {
    use halod_shared::types::{PluginSource, PluginUpdateStatus};
    let repos = app.config.read().await.plugin_repos.clone();
    let plugins = app.registry.list(&*app.secret_store);
    let mut out = Vec::new();
    for r in repos {
        let dir = crate::config::plugin_repos_dir().join(&r.slug);
        for p in plugins
            .iter()
            .filter(|p| matches!(&p.source, PluginSource::Repo { slug } if *slug == r.slug))
        {
            let Some((_, subpath)) = app.registry.repo_location_for(&p.id) else {
                continue;
            };
            let index_hash = plugin_index_content(&dir, &subpath).await;
            let local_hash = app.registry.content_hash_for(&p.id);
            let changed = match (&local_hash, &index_hash) {
                (Some(local), Some(index)) => local != index,
                _ => false,
            };
            if changed {
                out.push(PluginUpdateStatus {
                    plugin_id: p.id.clone(),
                    slug: r.slug.clone(),
                    update_available: false,
                    on_disk_changed: true,
                    current_version: p.version.clone(),
                    available_version: String::new(),
                });
            }
        }
    }
    out
}

/// Disable every plugin changed on disk since checkout, before discovery, so a
/// tampered plugin never activates. Re-enabling accepts the content.
pub async fn quarantine_changed_plugins(app: Arc<AppState>) {
    let statuses = compute_on_disk_changes(&app).await;
    if statuses.is_empty() {
        return;
    }

    {
        let mut cfg = app.config.write().await;
        for s in &statuses {
            if !cfg.plugins_disabled.iter().any(|x| x == &s.plugin_id) {
                cfg.plugins_disabled.push(s.plugin_id.clone());
            }
        }
        app.registry.set_disabled(&cfg.plugins_disabled);
    }
    app.request_config_save();

    for s in &statuses {
        // Suppress the ungranted notice so a permissioned plugin isn't double-alerted.
        app.registry.suppress_permission_notice(&s.plugin_id);
        log::warn!("plugin '{}' changed on disk — disabling it", s.plugin_id);
    }

    publish_plugin_updates(&app, statuses).await;
    crate::ipc::broadcast_state(&app).await;
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

    /// A `file://` URL for a local path — `add_repo`/`clone` now require an
    /// explicit scheme, so tests clone local source repos through one.
    fn file_url(path: &std::path::Path) -> String {
        format!("file://{}", path.display())
    }

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
            assert_eq!(cfg.plugin_repos.len(), 1);
            assert_eq!(cfg.plugin_repos[0].slug, slug);
            drop(cfg);

            let plugins = app.registry.list(&*app.secret_store);
            assert!(
                plugins.iter().any(|p| p.id == slug),
                "repo-sourced plugin should be discoverable after add_repo"
            );
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
                for r in cfg.plugin_repos.iter_mut() {
                    r.last_sync = Some(STALE.to_owned());
                }
            }

            touch_last_sync(&app, std::slice::from_ref(&slug)).await;

            let cfg = app.config.read().await;
            let r = cfg.plugin_repos.iter().find(|r| r.slug == slug).unwrap();
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
    async fn update_repo_advances_locked_sha_and_invalidates_stale_consent() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            init_source_repo(src.path(), &slug);

            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            // Consent to the plugin as it stands right after add_repo.
            let hash_before = app.registry.content_hash_for(&slug).unwrap();
            {
                let mut cfg = app.config.write().await;
                cfg.plugin_acknowledged
                    .insert(slug.clone(), hash_before.clone());
            }
            app.registry
                .set_acknowledged(&app.config.read().await.plugin_acknowledged);
            assert!(app.registry.content_hash_for(&slug).is_some());

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

            let hash_after = app.registry.content_hash_for(&slug).unwrap();
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
    async fn update_plugin_broadcasts_a_cleared_update_flag() {
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

            update_plugin(slug.clone(), app.clone()).await.unwrap();

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
                "update_plugin must broadcast a plugin_updates frame clearing the flag"
            );
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
            let clone_main = crate::config::plugin_repos_dir()
                .join(&slug)
                .join("main.lua");
            std::fs::write(&clone_main, "return { hacked = true }").unwrap();
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
    async fn quarantine_disables_a_tampered_plugin_and_reenabling_accepts_it() {
        crate::test_support::with_tmp_config(|app| async move {
            let src = tempfile::tempdir().unwrap();
            let slug = super::sanitize_slug(&file_url(src.path()));
            init_source_repo(src.path(), &slug);
            add_repo(file_url(src.path()), None, app.clone())
                .await
                .unwrap();

            // Tamper with the checked-out file, then reload so the manifest hash
            // reflects the edit.
            let clone_main = crate::config::plugin_repos_dir()
                .join(&slug)
                .join("main.lua");
            std::fs::write(&clone_main, "return { hacked = true }").unwrap();
            reload_registry(&app).await;

            quarantine_changed_plugins(app.clone()).await;
            assert!(
                app.config
                    .read()
                    .await
                    .plugins_disabled
                    .iter()
                    .any(|x| x == &slug),
                "a plugin changed on disk must be disabled"
            );

            // Re-enabling accepts the current content as the new baseline, so it
            // is no longer flagged (and would not be re-quarantined).
            crate::registry::usecases::plugins::set_enabled(slug.clone(), true, app.clone())
                .await
                .unwrap();
            assert!(
                compute_on_disk_changes(&app).await.is_empty(),
                "re-enabling must accept the on-disk content as the new baseline"
            );
            assert!(
                !app.config
                    .read()
                    .await
                    .plugins_disabled
                    .iter()
                    .any(|x| x == &slug),
                "re-enabling clears the disabled flag"
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
            let clone_main = crate::config::plugin_repos_dir()
                .join(&slug)
                .join("main.lua");
            std::fs::write(&clone_main, "return { hacked = true }").unwrap();
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
                cfg.plugins_disabled.push(slug.clone());
            }
            app.registry
                .set_disabled(&app.config.read().await.plugins_disabled);

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
