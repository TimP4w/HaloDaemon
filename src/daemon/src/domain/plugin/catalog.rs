// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use halod_plugin_signing::catalog::{CATALOG_FILE, MAX_CATALOG_BYTES};
use halod_plugin_signing::fetch::{https_get, https_url, package_url};
use halod_plugin_signing::package::{extract_archive, read_package_meta, MAX_ARCHIVE_BYTES};
use halod_plugin_signing::{open_catalog, Catalog, CatalogError, CatalogPackage};

use super::repo::RepositoryTrust;

#[derive(Debug, Clone)]
pub struct RepoCatalog {
    pub catalog: Catalog,
    pub last_sync: Option<String>,
}

pub fn repo_dir(slug: &str) -> PathBuf {
    crate::config::plugins_dir().join(slug)
}

pub fn cached(slug: &str, trust: &RepositoryTrust) -> Option<Catalog> {
    let bytes = std::fs::read(repo_dir(slug).join(CATALOG_FILE)).ok()?;
    let keys = trust.pinned_keys();
    open_catalog(&bytes, keys.as_deref()).ok()
}

pub fn fetch(slug: &str, url: &str, trust: &RepositoryTrust) -> Result<Catalog> {
    let bytes = https_get(&https_url(url)?, MAX_CATALOG_BYTES)?;
    let keys = trust.pinned_keys();
    let catalog = open_catalog(&bytes, keys.as_deref())?;
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
    crate::util::fs::atomic_write(&dir.join(CATALOG_FILE), &bytes)?;
    Ok(catalog)
}

pub fn stage(slug: &str, url: &str, entry: &CatalogPackage) -> Result<PathBuf> {
    let bytes = https_get(&package_url(url, &entry.path)?, MAX_ARCHIVE_BYTES)?;
    entry.verify(&bytes)?;
    let staging = repo_dir(slug).join(format!(".{}.staging-{}", entry.id, uuid::Uuid::new_v4()));
    let checked = extract_archive(&bytes, &staging).and_then(|()| {
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
            if entry.file_name().to_string_lossy().starts_with('.') {
                let path = entry.path();
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
}
