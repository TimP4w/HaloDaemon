// SPDX-License-Identifier: GPL-3.0-or-later
//! Thin `git2` wrapper for git-repo plugin sources: pure git in, `Result` out, no daemon/registry knowledge.

use anyhow::{anyhow, Context, Result};
use git2::{build::RepoBuilder, ResetType};
use std::path::Path;

/// Clone `url` into `dest`, checking out `branch` (or the remote default), and return the HEAD SHA.
pub fn clone(url: &str, dest: &Path, branch: Option<&str>) -> Result<String> {
    let mut builder = RepoBuilder::new();
    if let Some(b) = branch {
        builder.branch(b);
    }
    let repo = builder
        .clone(url, dest)
        .with_context(|| format!("cloning {url} into {}", dest.display()))?;
    let head = repo.head().context("clone produced no HEAD")?;
    let oid = head
        .target()
        .ok_or_else(|| anyhow!("HEAD is not a direct reference"))?;
    Ok(oid.to_string())
}

/// The name of the branch an already-cloned repo tracks: `branch`, or its current `HEAD`'s shorthand.
fn tracked_branch_name(repo: &git2::Repository, branch: Option<&str>) -> Result<String> {
    if let Some(b) = branch {
        return Ok(b.to_owned());
    }
    let head = repo.head().context("reading HEAD")?;
    head.shorthand()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("HEAD has no shorthand name"))
}

/// Fetch `origin`'s tip into remote-tracking refs and return its SHA, without touching the working tree.
pub fn fetch_remote_sha(repo_dir: &Path, branch: Option<&str>) -> Result<String> {
    let repo = git2::Repository::open(repo_dir)
        .with_context(|| format!("opening repo at {}", repo_dir.display()))?;
    let branch_name = tracked_branch_name(&repo, branch)?;
    let mut remote = repo.find_remote("origin").context("no 'origin' remote")?;
    let refspec = format!("refs/heads/{branch_name}");
    remote
        .fetch(&[&refspec], None, None)
        .with_context(|| format!("fetching {refspec} from origin"))?;
    let oid = repo
        .refname_to_id("FETCH_HEAD")
        .context("resolving FETCH_HEAD after fetch")?;
    Ok(oid.to_string())
}

/// Hard-reset the working tree at `repo_dir` to `sha` (must already be fetched or cloned).
pub fn checkout_sha(repo_dir: &Path, sha: &str) -> Result<()> {
    let repo = git2::Repository::open(repo_dir)
        .with_context(|| format!("opening repo at {}", repo_dir.display()))?;
    let oid = git2::Oid::from_str(sha).with_context(|| format!("parsing sha '{sha}'"))?;
    let commit = repo
        .find_commit(oid)
        .with_context(|| format!("commit '{sha}' not found (fetch it first)"))?;
    repo.reset(commit.as_object(), ResetType::Hard, None)
        .with_context(|| format!("resetting working tree to '{sha}'"))
}

/// Read a blob's bytes at `path` (relative to `tree`'s root).
fn read_blob_at(repo: &git2::Repository, tree: &git2::Tree, path: &Path) -> Result<Vec<u8>> {
    let entry = tree
        .get_path(path)
        .with_context(|| format!("{} not found in commit tree", path.display()))?;
    let obj = entry
        .to_object(repo)
        .with_context(|| format!("resolving {} object", path.display()))?;
    let blob = obj
        .as_blob()
        .ok_or_else(|| anyhow!("{} is not a blob", path.display()))?;
    Ok(blob.content().to_vec())
}

#[derive(serde::Deserialize, Default)]
struct MetaEntryVersion {
    #[serde(default)]
    entry: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

/// A plugin package's content hash + declared version at a specific commit,
/// read directly from git's object database — no working-tree checkout, so
/// this never disturbs the current on-disk state. `subpath` is the plugin's
/// path within the repo (empty for a root plugin, `plugins/<id>` for a
/// subdir one). Mirrors `PluginManifest::content_hash()`
/// (`sha256(plugin.yaml bytes || entry script bytes)`) byte-for-byte, so the
/// result can be compared directly against a loaded manifest's hash.
pub fn remote_plugin_content(
    repo_dir: &Path,
    sha: &str,
    subpath: &Path,
) -> Result<(String, String)> {
    use sha2::{Digest, Sha256};

    let repo = git2::Repository::open(repo_dir)
        .with_context(|| format!("opening repo at {}", repo_dir.display()))?;
    let oid = git2::Oid::from_str(sha).with_context(|| format!("parsing sha '{sha}'"))?;
    let commit = repo
        .find_commit(oid)
        .with_context(|| format!("commit '{sha}' not found (fetch it first)"))?;
    let tree = commit.tree().context("reading commit tree")?;

    let yaml_path = subpath.join("plugin.yaml");
    let yaml_bytes = read_blob_at(&repo, &tree, &yaml_path)?;
    let meta: MetaEntryVersion = serde_yaml::from_slice(&yaml_bytes)
        .with_context(|| format!("parsing {}", yaml_path.display()))?;
    let entry = meta.entry.as_deref().unwrap_or("main.lua");
    let entry_bytes = read_blob_at(&repo, &tree, &subpath.join(entry))?;

    let mut hasher = Sha256::new();
    hasher.update(&yaml_bytes);
    hasher.update(&entry_bytes);
    let hash = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    Ok((hash, meta.version.unwrap_or_default()))
}

/// Check out only `subpath` (one plugin's files) from `sha` into the working
/// tree, leaving every other path untouched — a per-plugin update, as opposed
/// to [`checkout_sha`]'s whole-repo reset. `subpath` empty checks out the
/// whole tree (a root-level plugin has no narrower subtree to scope to).
pub fn checkout_subtree(repo_dir: &Path, sha: &str, subpath: &Path) -> Result<()> {
    let repo = git2::Repository::open(repo_dir)
        .with_context(|| format!("opening repo at {}", repo_dir.display()))?;
    let oid = git2::Oid::from_str(sha).with_context(|| format!("parsing sha '{sha}'"))?;
    let commit = repo
        .find_commit(oid)
        .with_context(|| format!("commit '{sha}' not found (fetch it first)"))?;
    let tree = commit.tree().context("reading commit tree")?;
    let mut opts = git2::build::CheckoutBuilder::new();
    opts.force();
    if !subpath.as_os_str().is_empty() {
        opts.path(subpath);
    }
    repo.checkout_tree(tree.as_object(), Some(&mut opts))
        .with_context(|| format!("checking out '{}' at '{sha}'", subpath.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Init a repo at `dir` with an initial `plugin.yaml` + `main.lua` commit, returning its SHA.
    fn init_repo_with_plugin(dir: &Path) -> String {
        fs::create_dir_all(dir).unwrap();
        let repo = git2::Repository::init(dir).unwrap();
        fs::write(
            dir.join("plugin.yaml"),
            "id: demo\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n",
        )
        .unwrap();
        fs::write(dir.join("main.lua"), "return {}").unwrap();
        commit_all(&repo, "initial commit")
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

    #[test]
    fn clone_checks_out_head_and_returns_its_sha() {
        let src = tempfile::tempdir().unwrap();
        let expected = init_repo_with_plugin(src.path());

        let dest = tempfile::tempdir().unwrap();
        let clone_dir = dest.path().join("clone");
        let sha = clone(&src.path().to_string_lossy(), &clone_dir, None).unwrap();

        assert_eq!(sha, expected);
        assert!(clone_dir.join("plugin.yaml").is_file());
    }

    #[test]
    fn fetch_and_checkout_round_trip_a_new_commit() {
        let src = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(src.path()).unwrap();
        fs::write(src.path().join("plugin.yaml"), "id: demo\n").unwrap();
        fs::write(src.path().join("main.lua"), "return {}").unwrap();
        let first_sha = commit_all(&repo, "first");

        let dest = tempfile::tempdir().unwrap();
        let clone_dir = dest.path().join("clone");
        let cloned_sha = clone(&src.path().to_string_lossy(), &clone_dir, None).unwrap();
        assert_eq!(cloned_sha, first_sha);

        // Advance the source repo with a second commit.
        fs::write(src.path().join("main.lua"), "return { extra = true }").unwrap();
        let second_sha = commit_all(&repo, "second");
        assert_ne!(first_sha, second_sha);

        // fetch_remote_sha must not touch the clone's working tree.
        let remote_sha = fetch_remote_sha(&clone_dir, None).unwrap();
        assert_eq!(remote_sha, second_sha);
        let contents = fs::read_to_string(clone_dir.join("main.lua")).unwrap();
        assert_eq!(
            contents, "return {}",
            "fetch must not change the working tree"
        );

        checkout_sha(&clone_dir, &second_sha).unwrap();
        let contents = fs::read_to_string(clone_dir.join("main.lua")).unwrap();
        assert_eq!(contents, "return { extra = true }");
    }

    #[test]
    fn clone_with_explicit_branch_checks_it_out() {
        let src = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(src.path()).unwrap();
        fs::write(src.path().join("plugin.yaml"), "id: demo\n").unwrap();
        commit_all(&repo, "on default branch");
        let default_branch = repo.head().unwrap().shorthand().unwrap().to_owned();

        let dest = tempfile::tempdir().unwrap();
        let clone_dir = dest.path().join("clone");
        clone(
            &src.path().to_string_lossy(),
            &clone_dir,
            Some(&default_branch),
        )
        .unwrap();
        assert!(clone_dir.join("plugin.yaml").is_file());
    }

    #[test]
    fn checkout_unknown_sha_errors() {
        let src = tempfile::tempdir().unwrap();
        init_repo_with_plugin(src.path());
        let dest = tempfile::tempdir().unwrap();
        let clone_dir = dest.path().join("clone");
        clone(&src.path().to_string_lossy(), &clone_dir, None).unwrap();

        let err = checkout_sha(&clone_dir, "0000000000000000000000000000000000000f").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    /// A repo with two plugins under `plugins/<id>/`, for the per-plugin hash/checkout tests.
    fn init_repo_with_two_plugins(dir: &Path) -> String {
        fs::create_dir_all(dir).unwrap();
        let repo = git2::Repository::init(dir).unwrap();
        for id in ["alpha", "beta"] {
            let plugin_dir = dir.join("plugins").join(id);
            fs::create_dir_all(&plugin_dir).unwrap();
            fs::write(
                plugin_dir.join("plugin.yaml"),
                format!("id: {id}\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n"),
            )
            .unwrap();
            fs::write(plugin_dir.join("main.lua"), "return {}").unwrap();
        }
        commit_all(&repo, "initial commit")
    }

    #[test]
    fn remote_plugin_content_matches_the_daemon_side_content_hash() {
        use crate::drivers::plugins::{parse_manifest_from_dir, PluginManifest};

        let src = tempfile::tempdir().unwrap();
        init_repo_with_two_plugins(src.path());

        let local_manifest: PluginManifest =
            parse_manifest_from_dir(&src.path().join("plugins").join("alpha")).unwrap();

        let (remote_hash, _version) = remote_plugin_content(
            src.path(),
            &repo_head_sha(src.path()),
            Path::new("plugins/alpha"),
        )
        .unwrap();

        assert_eq!(
            remote_hash,
            local_manifest.content_hash(),
            "the git-object-read hash must be byte-identical to the on-disk content_hash"
        );
    }

    #[test]
    fn remote_plugin_content_changes_only_for_the_modified_plugin() {
        let src = tempfile::tempdir().unwrap();
        let first_sha = init_repo_with_two_plugins(src.path());
        let (alpha_before, _) =
            remote_plugin_content(src.path(), &first_sha, Path::new("plugins/alpha")).unwrap();
        let (beta_before, _) =
            remote_plugin_content(src.path(), &first_sha, Path::new("plugins/beta")).unwrap();

        fs::write(
            src.path().join("plugins").join("alpha").join("main.lua"),
            "return { extra = true }",
        )
        .unwrap();
        let repo = git2::Repository::open(src.path()).unwrap();
        let second_sha = commit_all(&repo, "change alpha only");

        let (alpha_after, _) =
            remote_plugin_content(src.path(), &second_sha, Path::new("plugins/alpha")).unwrap();
        let (beta_after, _) =
            remote_plugin_content(src.path(), &second_sha, Path::new("plugins/beta")).unwrap();

        assert_ne!(alpha_before, alpha_after, "modified plugin's hash changes");
        assert_eq!(
            beta_before, beta_after,
            "untouched sibling's hash is stable"
        );
    }

    #[test]
    fn checkout_subtree_updates_only_the_named_plugin() {
        let src = tempfile::tempdir().unwrap();
        init_repo_with_two_plugins(src.path());

        let dest = tempfile::tempdir().unwrap();
        let clone_dir = dest.path().join("clone");
        clone(&src.path().to_string_lossy(), &clone_dir, None).unwrap();

        fs::write(
            src.path().join("plugins").join("alpha").join("main.lua"),
            "return { extra = true }",
        )
        .unwrap();
        let repo = git2::Repository::open(src.path()).unwrap();
        let second_sha = commit_all(&repo, "change alpha only");

        fetch_remote_sha(&clone_dir, None).unwrap();
        checkout_subtree(&clone_dir, &second_sha, Path::new("plugins/alpha")).unwrap();

        let alpha =
            fs::read_to_string(clone_dir.join("plugins").join("alpha").join("main.lua")).unwrap();
        assert_eq!(alpha, "return { extra = true }");
        let beta =
            fs::read_to_string(clone_dir.join("plugins").join("beta").join("main.lua")).unwrap();
        assert_eq!(
            beta, "return {}",
            "checkout_subtree must not touch a sibling plugin's files"
        );
    }

    fn repo_head_sha(dir: &Path) -> String {
        let repo = git2::Repository::open(dir).unwrap();
        let head = repo.head().unwrap();
        let oid = head.target().unwrap();
        oid.to_string()
    }
}
