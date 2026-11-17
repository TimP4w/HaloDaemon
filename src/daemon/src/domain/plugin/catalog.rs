// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use halod_plugin_signing::catalog::{CATALOG_FILE, MAX_CATALOG_BYTES};
use halod_plugin_signing::fetch::{https_get, https_url, package_url};
use halod_plugin_signing::package::{extract_archive, read_package_meta, MAX_ARCHIVE_BYTES};
use halod_plugin_signing::{open_catalog, Catalog, CatalogError, CatalogPackage};

use super::repo::RepositoryTrust;
use crate::config;
use crate::util::fs;

#[derive(Debug, Clone)]
pub struct RepoCatalog {
    pub catalog: Catalog,
    pub last_sync: Option<String>,
}

pub fn repo_dir(slug: &str) -> PathBuf {
    config::plugins_dir().join(slug)
}

pub fn cached(slug: &str, trust: &RepositoryTrust) -> Option<Catalog> {
    let bytes = std::fs::read(repo_dir(slug).join(CATALOG_FILE)).ok()?;
    let keys = trust.pinned_keys();
    open_catalog(&bytes, keys.as_deref()).ok()
}

pub fn fetch(slug: &str, url: &str, trust: &RepositoryTrust) -> Result<Catalog> {
    accept(
        slug,
        &https_get(&https_url(url)?, MAX_CATALOG_BYTES)?,
        trust,
    )
}

fn accept(slug: &str, bytes: &[u8], trust: &RepositoryTrust) -> Result<Catalog> {
    let keys = trust.pinned_keys();
    let catalog = open_catalog(bytes, keys.as_deref())?;
    if let Some(previous) = cached(slug, trust) {
        if catalog.published < previous.published {
            return Err(CatalogError::Stale {
                cached: previous.published,
                fetched: catalog.published,
            }
            .into());
        }
    }
    let dir = repo_dir(slug);
    std::fs::create_dir_all(&dir)?;
    fs::atomic_write(&dir.join(CATALOG_FILE), bytes)?;
    Ok(catalog)
}

pub fn stage(slug: &str, url: &str, entry: &CatalogPackage) -> Result<PathBuf> {
    stage_bytes(
        slug,
        entry,
        &https_get(&package_url(url, &entry.path)?, MAX_ARCHIVE_BYTES)?,
    )
}

fn stage_bytes(slug: &str, entry: &CatalogPackage, bytes: &[u8]) -> Result<PathBuf> {
    entry.verify(bytes)?;
    let staging = repo_dir(slug).join(format!(".{}.staging-{}", entry.id, uuid::Uuid::new_v4()));
    let checked = extract_archive(bytes, &staging).and_then(|()| {
        let meta = read_package_meta(&staging)?;
        if meta.id != entry.id || meta.version != entry.version || meta.api != entry.api {
            bail!(
                "package '{}' does not match its catalog entry ({} {} api {})",
                entry.id,
                meta.id,
                meta.version,
                meta.api
            );
        }
        Ok(())
    });
    if let Err(error) = checked {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(staging)
}

pub fn remove_leftovers(root: &Path) -> Result<()> {
    let Ok(slugs) = std::fs::read_dir(root) else {
        return Ok(());
    };
    for slug in slugs.flatten().filter(|entry| entry.path().is_dir()) {
        for entry in std::fs::read_dir(slug.path())?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                let path = entry.path();
                let interrupted_swap = name
                    .strip_prefix('.')
                    .and_then(|rest| rest.strip_suffix(".old"))
                    .map(|id| slug.path().join(id))
                    .filter(|target| !target.exists());
                if let Some(target) = interrupted_swap {
                    std::fs::rename(&path, &target)
                        .with_context(|| format!("restoring {}", target.display()))?;
                    continue;
                }
                std::fs::remove_dir_all(&path)
                    .or_else(|_| std::fs::remove_file(&path))
                    .with_context(|| format!("removing {}", path.display()))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leftovers_are_removed_but_installs_are_kept() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("official");
        for dir in ["demo", ".demo.staging-1", ".demo.old"] {
            std::fs::create_dir_all(repo.join(dir)).unwrap();
        }
        std::fs::write(repo.join("catalog.yaml"), "x").unwrap();

        remove_leftovers(root.path()).unwrap();

        let mut names: Vec<_> = std::fs::read_dir(&repo)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["catalog.yaml", "demo"]);
    }

    fn unsigned(published: u64) -> Vec<u8> {
        Catalog {
            schema: halod_plugin_signing::catalog::CATALOG_SCHEMA,
            name: "Test".into(),
            commit: None,
            published,
            signing_key: None,
            packages: Vec::new(),
        }
        .to_bytes()
    }

    #[test]
    fn an_older_catalog_is_rejected_and_the_cache_kept() {
        let _config = crate::test_support::tmp_config_dir();
        accept("community", &unsigned(200), &RepositoryTrust::FirstUse).unwrap();
        let error = accept("community", &unsigned(100), &RepositoryTrust::FirstUse).unwrap_err();
        assert_eq!(
            error.downcast_ref::<CatalogError>(),
            Some(&CatalogError::Stale {
                cached: 200,
                fetched: 100
            })
        );
        let kept = cached("community", &RepositoryTrust::FirstUse).unwrap();
        assert_eq!(kept.published, 200);
    }

    #[test]
    fn staging_rejects_a_package_that_disagrees_with_its_entry_and_cleans_up() {
        let _config = crate::test_support::tmp_config_dir();
        let source = tempfile::tempdir().unwrap();
        let package =
            crate::test_support::write_plugin_package(source.path(), "demo", "1.0.0", "3.0");
        let archive = halod_plugin_signing::package::write_archive(&package).unwrap();
        let entry = |version: &str, api: &str| CatalogPackage {
            id: "demo".into(),
            version: version.parse().unwrap(),
            api: api.parse().unwrap(),
            path: "packages/demo.tar.gz".into(),
            sha256: halod_plugin_signing::sha256_hex(&archive),
            size: archive.len() as u64,
        };
        for mismatched in [entry("2.0.0", "3.0"), entry("1.0.0", "3.1")] {
            let error = stage_bytes("official", &mismatched, &archive).unwrap_err();
            assert!(error
                .to_string()
                .contains("does not match its catalog entry"));
        }
        let leftovers = std::fs::read_dir(repo_dir("official"))
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(leftovers, 0);
        assert!(stage_bytes("official", &entry("1.0.0", "3.0"), &archive)
            .unwrap()
            .join("plugin.yaml")
            .is_file());
    }

    #[test]
    fn a_backup_without_its_install_is_restored_not_removed() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("official");
        std::fs::create_dir_all(repo.join(".demo.old")).unwrap();
        std::fs::write(repo.join(".demo.old").join("plugin.yaml"), "old").unwrap();
        std::fs::create_dir_all(repo.join(".demo.staging-1")).unwrap();

        remove_leftovers(root.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.join("demo").join("plugin.yaml")).unwrap(),
            "old"
        );
        assert!(!repo.join(".demo.old").exists());
        assert!(!repo.join(".demo.staging-1").exists());
    }
}
