// SPDX-License-Identifier: GPL-3.0-or-later
//! Validation and immutable storage helpers for signed plugin releases.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::constants::OFFICIAL_PLUGIN_REPO_PUBLIC_KEYS;
pub use halod_plugin_signing::{package_hash, RepositoryManifest, RepositorySigningKey};

#[derive(Debug, Clone, Default)]
pub enum RepositoryTrust {
    Official,
    Pinned(RepositorySigningKey),
    #[default]
    Unsigned,
}

pub fn read_repository_manifest(dir: &Path) -> Result<RepositoryManifest> {
    let manifest = read_repository_index(dir)?;
    halod_plugin_signing::validate_repository_packages(dir, &manifest)?;
    Ok(manifest)
}

pub fn read_repository_index(dir: &Path) -> Result<RepositoryManifest> {
    halod_plugin_signing::read_repository_index(dir)
}

pub fn advertised_signing_key(dir: &Path) -> Result<Option<RepositorySigningKey>> {
    let yaml = std::fs::read(dir.join("release.yaml"))?;
    let manifest: RepositoryManifest = serde_yaml::from_slice(&yaml)?;
    halod_plugin_signing::validate_repository_index(&manifest)?;
    let Some(key) = manifest.signing_key else {
        return Ok(None);
    };
    let signature = std::fs::read(dir.join("release.sig"))?;
    halod_plugin_signing::verify_advertised_signature(&yaml, &signature, &key)?;
    Ok(Some(key))
}

pub fn verify_official_repository(dir: &Path) -> Result<RepositoryManifest> {
    let manifest = read_repository_manifest(dir)?;
    verify_official_repository_signature(dir)?;
    Ok(manifest)
}

pub fn verify_official_repository_signature(dir: &Path) -> Result<()> {
    verify_repository_signature(dir, &RepositoryTrust::Official)
}

pub fn verify_repository_signature(dir: &Path, trust: &RepositoryTrust) -> Result<()> {
    let manifest_path = dir.join("release.yaml");
    let signature_path = dir.join("release.sig");
    let manifest = std::fs::read(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let signature = std::fs::read(&signature_path)
        .with_context(|| format!("reading {}", signature_path.display()))?;
    match trust {
        RepositoryTrust::Official => halod_plugin_signing::verify_signature(
            &manifest,
            &signature,
            OFFICIAL_PLUGIN_REPO_PUBLIC_KEYS,
        ),
        RepositoryTrust::Pinned(key) => {
            halod_plugin_signing::verify_advertised_signature(&manifest, &signature, key)
        }
        RepositoryTrust::Unsigned => Ok(()),
    }
}

pub fn prune_revisions(root: &Path, keep: &[&str]) -> std::io::Result<()> {
    let revisions = root.join("revisions");
    let entries = match std::fs::read_dir(&revisions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        if name.starts_with('.') {
            remove_path(&path)?;
        } else if path.is_dir() && !keep.contains(&name.as_ref()) {
            std::fs::remove_dir_all(&path)?;
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// The immutable directory selected for a release source.
pub fn active_revision_dir(record: &crate::config::PluginRepoRecord) -> PathBuf {
    let root = if record.slug == crate::constants::OFFICIAL_PLUGIN_REPO_SLUG
        && record.active_source == crate::config::PluginRevisionSource::Embedded
    {
        crate::config::embedded_plugin_revisions_dir().join(&record.slug)
    } else {
        crate::config::plugin_repos_dir().join(&record.slug)
    };
    root.join("revisions").join(
        record
            .active_revision
            .as_deref()
            .filter(|revision| !revision.is_empty())
            .unwrap_or("__inactive__"),
    )
}

#[cfg(test)]
mod tests {
    use super::prune_revisions;

    #[test]
    fn prune_keeps_named_revisions_and_removes_others_and_dot_dirs() {
        let root = tempfile::tempdir().unwrap();
        let revisions = root.path().join("revisions");
        for name in ["v1", "v2", "v3", ".v2.staging-abc", ".v1.corrupt-def"] {
            std::fs::create_dir_all(revisions.join(name)).unwrap();
        }
        std::fs::write(revisions.join(".leftover-file"), b"x").unwrap();

        prune_revisions(root.path(), &["v2", "v3"]).unwrap();

        assert!(!revisions.join("v1").exists(), "unkept revision removed");
        assert!(revisions.join("v2").exists(), "kept revision retained");
        assert!(revisions.join("v3").exists(), "kept revision retained");
        assert!(
            !revisions.join(".v2.staging-abc").exists(),
            "dot-prefixed staging leftover removed"
        );
        assert!(
            !revisions.join(".v1.corrupt-def").exists(),
            "dot-prefixed corrupt leftover removed"
        );
        assert!(
            !revisions.join(".leftover-file").exists(),
            "dot-prefixed file leftover removed"
        );
    }

    #[test]
    fn prune_is_a_noop_when_the_revisions_dir_is_missing() {
        let root = tempfile::tempdir().unwrap();
        prune_revisions(root.path(), &["v1"]).unwrap();
    }
}
