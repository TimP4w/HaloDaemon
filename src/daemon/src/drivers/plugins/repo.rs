// SPDX-License-Identifier: GPL-3.0-or-later
//! Thin `git2` wrapper for git-repo plugin sources: pure git in, `Result` out, no daemon/registry knowledge.

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use git2::build::RepoBuilder;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The first official plugin repository signing key. The signing private key
/// is intentionally never present in this repository.
/// Embedded public keys are intentionally keyed by stable release key id so a
/// later Halo build can accept a rotated official key without treating the
/// repository URL as a trust signal.
/// TODO: move to constants
const OFFICIAL_PUBLIC_KEYS: &[(&str, &str)] = &[(
    "halodaemon-official-2026",
    "tjbwm5X4f70e+soVNV1AfRyb/TtnEsNNl+93YMO6IhQ=",
)];
const REPOSITORY_SCHEMA: u32 = 1;
const PLUGIN_API: u32 = 1;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RepositoryManifest {
    pub schema: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub compatibility: RepositoryCompatibility,
    pub packages: Vec<RepositoryPackage>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RepositoryCompatibility {
    pub halod: String,
    pub plugin_api: u32,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RepositoryPackage {
    pub id: String,
    pub path: PathBuf,
    pub version: String,
    pub sha256: String,
}

#[derive(Debug, serde::Deserialize)]
struct RepositorySignature {
    schema: u32,
    algorithm: String,
    key_id: String,
    signature: String,
}

/// Parse and structurally validate a repository manifest from a working tree.
/// This deliberately does not require a signature, so it can also validate
/// third-party repositories and the explicit development override.
pub fn read_repository_manifest(repo_dir: &Path) -> Result<RepositoryManifest> {
    let path = repo_dir.join("repository.yaml");
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: RepositoryManifest =
        serde_yaml::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    validate_repository_manifest(repo_dir, &manifest)?;
    Ok(manifest)
}

/// Read the repository index directly from a fetched commit without changing
/// the working tree.  This is used to advertise an explicit update before the
/// user accepts it; the complete on-disk validation still runs after checkout.
pub fn read_repository_manifest_at_commit(
    repo_dir: &Path,
    sha: &str,
) -> Result<RepositoryManifest> {
    let repo = git2::Repository::open(repo_dir)
        .with_context(|| format!("opening repo at {}", repo_dir.display()))?;
    let oid = git2::Oid::from_str(sha).with_context(|| format!("parsing sha '{sha}'"))?;
    let tree = repo
        .find_commit(oid)
        .with_context(|| format!("commit '{sha}' not found"))?
        .tree()
        .context("reading commit tree")?;
    let bytes = read_blob_at(&repo, &tree, Path::new("repository.yaml"))?;
    let manifest: RepositoryManifest =
        serde_yaml::from_slice(&bytes).context("parsing repository.yaml from commit")?;
    validate_repository_index(&manifest)?;
    Ok(manifest)
}

/// Verify the official detached signature directly from fetched Git objects.
/// This is intentionally lighter than [`verify_official_repository`]: update
/// discovery must not touch the active checkout, while installation performs
/// the complete package-digest validation after materializing the revision.
pub fn verify_official_repository_at_commit(
    repo_dir: &Path,
    sha: &str,
) -> Result<RepositoryManifest> {
    let repo = git2::Repository::open(repo_dir)
        .with_context(|| format!("opening repo at {}", repo_dir.display()))?;
    let oid = git2::Oid::from_str(sha).with_context(|| format!("parsing sha '{sha}'"))?;
    let tree = repo
        .find_commit(oid)
        .with_context(|| format!("commit '{sha}' not found"))?
        .tree()
        .context("reading commit tree")?;
    let yaml = read_blob_at(&repo, &tree, Path::new("repository.yaml"))?;
    let manifest: RepositoryManifest =
        serde_yaml::from_slice(&yaml).context("parsing repository.yaml from commit")?;
    validate_repository_index(&manifest)?;
    let signature = read_blob_at(&repo, &tree, Path::new("repository.sig"))?;
    verify_official_signature(&yaml, &signature)?;
    Ok(manifest)
}

/// Verify the official detached signature and every package digest in a
/// checked-out repository. The signature covers exact `repository.yaml` bytes,
/// matching the standalone `repo-sign` tool.
pub fn verify_official_repository(repo_dir: &Path) -> Result<RepositoryManifest> {
    let yaml_path = repo_dir.join("repository.yaml");
    let yaml =
        std::fs::read(&yaml_path).with_context(|| format!("reading {}", yaml_path.display()))?;
    let manifest: RepositoryManifest = serde_yaml::from_slice(&yaml)
        .with_context(|| format!("parsing {}", yaml_path.display()))?;
    validate_repository_manifest(repo_dir, &manifest)?;

    let sig_path = repo_dir.join("repository.sig");
    let sig_bytes =
        std::fs::read(&sig_path).with_context(|| format!("reading {}", sig_path.display()))?;
    verify_official_signature(&yaml, &sig_bytes)
        .with_context(|| format!("verifying {}", sig_path.display()))?;

    for package in &manifest.packages {
        let actual = package_hash(&repo_dir.join(&package.path))?;
        if !actual.eq_ignore_ascii_case(&package.sha256) {
            bail!(
                "package '{}' hash mismatch: expected {}, got {}",
                package.id,
                package.sha256,
                actual
            );
        }
    }
    Ok(manifest)
}

/// Verify a detached official signature over the exact repository YAML bytes.
/// Keeping this byte-oriented makes verification identical for a checkout and
/// a fetched object, and avoids accidentally reserializing YAML before
/// signature validation.
fn verify_official_signature(yaml: &[u8], sig_bytes: &[u8]) -> Result<()> {
    let signature: RepositorySignature =
        serde_yaml::from_slice(sig_bytes).context("parsing repository signature")?;
    if signature.schema != REPOSITORY_SCHEMA {
        bail!(
            "unsupported repository signature schema {}",
            signature.schema
        );
    }
    if signature.algorithm != "ed25519" {
        bail!(
            "unsupported repository signature algorithm '{}'",
            signature.algorithm
        );
    }
    let key_b64 = OFFICIAL_PUBLIC_KEYS
        .iter()
        .find_map(|(key_id, key)| (*key_id == signature.key_id).then_some(*key))
        .ok_or_else(|| {
            anyhow!(
                "unknown official repository signing key '{}'",
                signature.key_id
            )
        })?;
    let public: [u8; 32] = B64
        .decode(key_b64)
        .context("decoding embedded official plugin signing key")?
        .try_into()
        .map_err(|_| anyhow!("embedded official plugin signing key is not 32 bytes"))?;
    let key =
        VerifyingKey::from_bytes(&public).context("constructing official plugin verifying key")?;
    let raw_signature: [u8; 64] = B64
        .decode(signature.signature.trim())
        .context("decoding repository signature")?
        .try_into()
        .map_err(|_| anyhow!("repository signature is not 64 bytes"))?;
    key.verify(yaml, &Signature::from_bytes(&raw_signature))
        .map_err(|_| anyhow!("repository signature does not match repository.yaml"))
}

fn validate_repository_manifest(repo_dir: &Path, manifest: &RepositoryManifest) -> Result<()> {
    validate_repository_index(manifest)?;

    for package in &manifest.packages {
        let dir = repo_dir.join(&package.path);
        if !dir.is_dir() {
            bail!(
                "repository package '{}' directory is missing: {}",
                package.id,
                dir.display()
            );
        }
        reject_symlinks(&dir)
            .with_context(|| format!("checking package '{}' for symlinks", package.id))?;
        let meta: MetaEntryVersion = serde_yaml::from_slice(
            &std::fs::read(dir.join("plugin.yaml"))
                .with_context(|| format!("reading package '{}' manifest", package.id))?,
        )
        .with_context(|| format!("parsing package '{}' manifest", package.id))?;
        if meta.id.as_deref() != Some(package.id.as_str()) {
            bail!(
                "repository package '{}' does not match its plugin.yaml id",
                package.id
            );
        }
        if meta.version.as_deref().unwrap_or_default() != package.version {
            bail!(
                "repository package '{}' does not match its plugin.yaml version",
                package.id
            );
        }
    }
    Ok(())
}

/// Checks fields that are meaningful before the commit is checked out.
fn validate_repository_index(manifest: &RepositoryManifest) -> Result<()> {
    if manifest.schema != REPOSITORY_SCHEMA {
        bail!("unsupported repository manifest schema {}", manifest.schema);
    }
    if manifest.id.trim().is_empty()
        || manifest.name.trim().is_empty()
        || manifest.version.trim().is_empty()
    {
        bail!("repository manifest id, name, and version must be non-empty");
    }
    if manifest.compatibility.plugin_api != PLUGIN_API {
        bail!(
            "repository requires plugin API {}, but Halo supports {}",
            manifest.compatibility.plugin_api,
            PLUGIN_API
        );
    }
    let requirement = semver::VersionReq::parse(&manifest.compatibility.halod)
        .context("parsing repository compatibility.halod")?;
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .context("parsing Halo package version")?;
    if !requirement.matches(&current) {
        bail!(
            "repository requires Halo '{}', current Halo is {}",
            manifest.compatibility.halod,
            current
        );
    }

    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    let mut previous_id: Option<&str> = None;
    for package in &manifest.packages {
        if package.id.trim().is_empty() || package.version.trim().is_empty() {
            bail!("repository package id and version must be non-empty");
        }
        if !ids.insert(&package.id) {
            bail!(
                "repository package id '{}' is declared more than once",
                package.id
            );
        }
        if let Some(previous) = previous_id {
            if previous >= package.id.as_str() {
                bail!(
                    "repository packages must be sorted by id: '{}' precedes '{}'",
                    previous,
                    package.id
                );
            }
        }
        previous_id = Some(&package.id);
        if package.path.is_absolute()
            || package.path.as_os_str().is_empty()
            || package.path.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            bail!(
                "repository package '{}' has an invalid path '{}'",
                package.id,
                package.path.display()
            );
        }
        if !paths.insert(&package.path) {
            bail!(
                "repository package path '{}' is declared more than once",
                package.path.display()
            );
        }
        if package.sha256.len() != 64 || !package.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!(
                "repository package '{}' has an invalid sha256 digest",
                package.id
            );
        }
    }
    Ok(())
}

/// Deterministic SHA-256 over a package's sorted relative regular-file paths,
/// file lengths, and bytes. Symlinks are rejected before hashing.
pub fn package_hash(package_dir: &Path) -> Result<String> {
    reject_symlinks(package_dir)?;
    let mut files = Vec::new();
    collect_package_files(package_dir, package_dir, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for relative in files {
        let bytes = std::fs::read(package_dir.join(&relative))
            .with_context(|| format!("reading package file {}", relative.display()))?;
        let name = relative.to_string_lossy().replace('\\', "/");
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_package_files(root: &Path, current: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(current).with_context(|| format!("reading {}", current.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!("repo contains a symlink: {}", entry.path().display());
        }
        if file_type.is_dir() {
            collect_package_files(root, &entry.path(), out)?;
        } else if file_type.is_file() {
            out.push(
                entry
                    .path()
                    .strip_prefix(root)
                    .expect("walk remains under root")
                    .to_path_buf(),
            );
        } else {
            bail!(
                "package contains a non-regular file: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

/// URL schemes a plugin repo may use. `file://` is a local clone (dev/test and
/// the official-repo bootstrap); remote sources must be `https`/`ssh`.
const ALLOWED_SCHEMES: &[&str] = &["https", "ssh", "file"];

/// Reject repo URLs on an insecure or unexpected transport before git touches
/// them. A URL must carry an explicit `scheme://`: the earlier check only fired
/// when `://` was present, so a bare path or scp-style `git@host:path` slipped
/// through *unvalidated* — an `http://` typo, or an unintended local clone from
/// a bare filesystem path, would pass. Requiring an explicit allow-listed scheme
/// closes that.
fn validate_url(url: &str) -> Result<()> {
    let Some((scheme, rest)) = url.split_once("://") else {
        bail!(
            "repo URL must start with an explicit scheme (https://, ssh://, or file://): '{url}'"
        );
    };
    if !ALLOWED_SCHEMES.contains(&scheme) {
        bail!("repo URL scheme '{scheme}://' is not allowed (use https, ssh, or file)");
    }
    if rest.is_empty() {
        bail!("repo URL has no path/host after '{scheme}://'");
    }
    Ok(())
}

/// Clone `url` into `dest`, checking out `branch` (or the remote default), and return the HEAD SHA.
pub fn clone(url: &str, dest: &Path, branch: Option<&str>) -> Result<String> {
    validate_url(url)?;
    let mut builder = RepoBuilder::new();
    if let Some(b) = branch {
        builder.branch(b);
    }
    let repo = builder
        .clone(url, dest)
        .with_context(|| format!("cloning {url} into {}", dest.display()))?;
    reject_symlinks(dest)?;
    let head = repo.head().context("clone produced no HEAD")?;
    let oid = head
        .target()
        .ok_or_else(|| anyhow!("HEAD is not a direct reference"))?;
    Ok(oid.to_string())
}

/// List a remote's branch names without cloning it (`git ls-remote --heads`),
/// sorted alphabetically. No working tree is created or touched.
pub fn list_remote_branches(url: &str) -> Result<Vec<String>> {
    validate_url(url)?;
    let mut remote = git2::Remote::create_detached(url)
        .with_context(|| format!("creating detached remote for {url}"))?;
    remote
        .connect(git2::Direction::Fetch)
        .with_context(|| format!("connecting to {url}"))?;
    let mut branches: Vec<String> = remote
        .list()
        .with_context(|| format!("listing refs on {url}"))?
        .iter()
        .filter_map(|head| head.name().strip_prefix("refs/heads/"))
        .map(str::to_owned)
        .collect();
    let _ = remote.disconnect();
    branches.sort();
    Ok(branches)
}

/// The name of the branch an already-cloned repo tracks: `branch`, or its current `HEAD`'s shorthand.
/// Falls back to origin's default branch (or "main") when HEAD is unborn — a
/// repo with zero commits yet (e.g. a freshly bootstrapped official-repo mirror).
fn tracked_branch_name(repo: &git2::Repository, branch: Option<&str>) -> Result<String> {
    if let Some(b) = branch {
        return Ok(b.to_owned());
    }
    match repo.head() {
        Ok(head) => head
            .shorthand()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("HEAD has no shorthand name")),
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => Ok(repo
            .find_reference("refs/remotes/origin/HEAD")
            .ok()
            .and_then(|r| r.symbolic_target().map(str::to_owned))
            .and_then(|t| t.strip_prefix("refs/remotes/origin/").map(str::to_owned))
            .unwrap_or_else(|| "main".to_owned())),
        Err(e) => Err(e).context("reading HEAD"),
    }
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

/// Materialize one fetched commit into a fresh regular-file directory without
/// changing the Git worktree. The caller validates the directory and atomically
/// renames it into the immutable revision store before selecting it for plugin
/// execution.
pub fn materialize_commit(repo_dir: &Path, sha: &str, dest: &Path) -> Result<()> {
    if dest.exists() {
        bail!("revision destination already exists: {}", dest.display());
    }
    let repo = git2::Repository::open(repo_dir)
        .with_context(|| format!("opening repo at {}", repo_dir.display()))?;
    let oid = git2::Oid::from_str(sha).with_context(|| format!("parsing sha '{sha}'"))?;
    let tree = repo
        .find_commit(oid)
        .with_context(|| format!("commit '{sha}' not found"))?
        .tree()
        .context("reading commit tree")?;
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    materialize_tree(&repo, &tree, dest)
}

fn materialize_tree(repo: &git2::Repository, tree: &git2::Tree<'_>, dest: &Path) -> Result<()> {
    for entry in tree {
        let name = entry
            .name()
            .ok_or_else(|| anyhow!("repository tree contains a non-UTF-8 filename"))?;
        let target = dest.join(name);
        match entry.kind() {
            Some(git2::ObjectType::Tree) => {
                std::fs::create_dir(&target)
                    .with_context(|| format!("creating {}", target.display()))?;
                let child = repo.find_tree(entry.id())?;
                materialize_tree(repo, &child, &target)?;
            }
            Some(git2::ObjectType::Blob) => {
                // A Git symlink is stored as a blob with link file mode. It
                // must never be materialized as a host symlink or followed by
                // later package validation.
                if entry.filemode() == 0o120000 {
                    bail!("repository contains a symlink: {}", target.display());
                }
                let blob = repo.find_blob(entry.id())?;
                std::fs::write(&target, blob.content())
                    .with_context(|| format!("writing {}", target.display()))?;
            }
            _ => bail!(
                "repository contains unsupported tree entry: {}",
                target.display()
            ),
        }
    }
    Ok(())
}

/// The immutable directory selected for a repository record. Legacy records
/// without an active revision retain the old checkout location until the next
/// explicit installation migrates them.
pub fn active_revision_dir(record: &crate::config::PluginRepoRecord) -> PathBuf {
    let root = crate::config::plugin_repos_dir().join(&record.slug);
    match &record.active_revision {
        Some(sha) => root.join("revisions").join(sha),
        None => root,
    }
}

/// Bail if any symlink exists under `root` (skipping `.git`). This protects
/// manually supplied development repositories as well as materialized Git
/// revisions before manifests and package hashes read their files.
pub fn reject_symlinks(root: &Path) -> Result<()> {
    for entry in std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!("repo contains a symlink: {}", entry.path().display());
        }
        if file_type.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            reject_symlinks(&entry.path())?;
        }
    }
    Ok(())
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

/// The two package fields the repository index verifies without loading or
/// executing the package itself.
#[derive(serde::Deserialize)]
struct MetaEntryVersion {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    version: Option<String>,
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
            "id: demo\ncompatibility:\n  halod: '>=0.2.0, <0.3.0'\n  plugin_api: 1\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n",
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

    /// A `file://` URL for a local path, so tests clone with an explicit,
    /// allow-listed scheme rather than a now-rejected bare path.
    fn file_url(path: &Path) -> String {
        url::Url::from_file_path(path)
            .expect("temporary repository path must be absolute")
            .into()
    }

    fn write_repository_manifest(root: &Path, digest: &str) {
        fs::write(
            root.join("repository.yaml"),
            format!(
                "schema: 1\nid: test-repo\nname: Test repository\nversion: 1.0.0\ncompatibility:\n  halod: '>=0.2.0, <0.3.0'\n  plugin_api: 1\npackages:\n  - id: demo\n    path: plugins/demo\n    version: 1.0.0\n    sha256: {digest}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn repository_manifest_accepts_an_indexed_package() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("plugins").join("demo");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("plugin.yaml"), "id: demo\nversion: 1.0.0\n").unwrap();
        fs::write(package.join("main.lua"), "return {}\n").unwrap();
        let digest = package_hash(&package).unwrap();
        write_repository_manifest(root.path(), &digest);

        let manifest = read_repository_manifest(root.path()).unwrap();
        assert_eq!(manifest.packages.len(), 1);
        assert_eq!(manifest.packages[0].id, "demo");
    }

    #[test]
    fn package_hash_changes_when_any_package_file_changes() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("package");
        fs::create_dir_all(package.join("lib")).unwrap();
        fs::write(package.join("plugin.yaml"), "id: demo\n").unwrap();
        fs::write(package.join("lib").join("protocol.lua"), "return 1\n").unwrap();
        let before = package_hash(&package).unwrap();
        fs::write(package.join("lib").join("protocol.lua"), "return 2\n").unwrap();
        let after = package_hash(&package).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn validate_url_allows_secure_and_local_sources() {
        for url in [
            "https://github.com/x/y.git",
            "ssh://git@github.com/x/y.git",
            "file:///srv/plugins/repo",
        ] {
            assert!(validate_url(url).is_ok(), "{url} should be allowed");
        }
    }

    #[test]
    fn validate_url_rejects_plaintext_unknown_and_schemeless_sources() {
        for url in [
            "http://example.com/x.git",
            "git://example.com/x.git",
            "ftp://example.com/x.git",
            // Bare path and scp-style — no explicit scheme, previously slipped through.
            "/tmp/local/repo",
            "git@github.com:x/y.git",
            "file://",
        ] {
            assert!(validate_url(url).is_err(), "{url} should be rejected");
        }
    }

    #[test]
    fn clone_checks_out_head_and_returns_its_sha() {
        let src = tempfile::tempdir().unwrap();
        let expected = init_repo_with_plugin(src.path());

        let dest = tempfile::tempdir().unwrap();
        let clone_dir = dest.path().join("clone");
        let sha = clone(&file_url(src.path()), &clone_dir, None).unwrap();

        assert_eq!(sha, expected);
        assert!(clone_dir.join("plugin.yaml").is_file());
    }

    #[test]
    fn fetch_does_not_change_the_worktree() {
        let src = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(src.path()).unwrap();
        fs::write(src.path().join("plugin.yaml"), "id: demo\n").unwrap();
        fs::write(src.path().join("main.lua"), "return {}").unwrap();
        let first_sha = commit_all(&repo, "first");

        let dest = tempfile::tempdir().unwrap();
        let clone_dir = dest.path().join("clone");
        let cloned_sha = clone(&file_url(src.path()), &clone_dir, None).unwrap();
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
    }

    #[test]
    fn list_remote_branches_returns_every_head_sorted() {
        let src = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(src.path()).unwrap();
        fs::write(src.path().join("plugin.yaml"), "id: demo\n").unwrap();
        commit_all(&repo, "initial");
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let default = repo.head().unwrap().shorthand().unwrap().to_owned();
        repo.branch("zebra", &head, false).unwrap();
        repo.branch("alpha", &head, false).unwrap();

        let branches = list_remote_branches(&file_url(src.path())).unwrap();

        let mut expected = vec![default, "alpha".to_owned(), "zebra".to_owned()];
        expected.sort();
        assert_eq!(branches, expected);
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
        clone(&file_url(src.path()), &clone_dir, Some(&default_branch)).unwrap();
        assert!(clone_dir.join("plugin.yaml").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn checkout_rejects_a_symlinked_entry() {
        // A repo whose commit carries a symlink must not land on disk for the
        // daemon to follow when reading plugin.yaml/main.lua.
        let src = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(src.path()).unwrap();
        fs::write(src.path().join("plugin.yaml"), "id: demo\n").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", src.path().join("main.lua")).unwrap();
        let sha = commit_all(&repo, "with a symlink");

        let _ = sha;
        let dest = tempfile::tempdir().unwrap();
        let clone_dir = dest.path().join("clone");
        // Clone materializes the working tree, so the guard fires here.
        let err = clone(&file_url(src.path()), &clone_dir, None)
            .expect_err("a symlinked repo must be rejected");
        assert!(err.to_string().contains("symlink"), "{err}");
    }
}
