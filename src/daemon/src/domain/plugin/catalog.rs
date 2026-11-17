// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use halod_plugin_signing::catalog::{CATALOG_FILE, MAX_CATALOG_BYTES};
use halod_plugin_signing::declarations::read_declarations;
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
    accept(slug, &download(url)?, trust)
}

pub fn download(url: &str) -> Result<Vec<u8>> {
    https_get(&https_url(url)?, MAX_CATALOG_BYTES)
}

pub fn accept(slug: &str, bytes: &[u8], trust: &RepositoryTrust) -> Result<Catalog> {
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
        let declared = read_declarations(&std::fs::read(staging.join("plugin.yaml"))?)?;
        if declared.name != entry.name
            || declared.matches != entry.matches
            || declared.uses != entry.uses
        {
            bail!(
                "package '{}' declares different uses or matches than its catalog entry",
                entry.id
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
        let declared =
            read_declarations(&std::fs::read(package.join("plugin.yaml")).unwrap()).unwrap();
        let entry = |version: &str, api: &str| CatalogPackage {
            id: "demo".parse().unwrap(),
            name: declared.name.clone(),
            version: version.parse().unwrap(),
            api: api.parse().unwrap(),
            path: "packages/demo.tar.gz".into(),
            sha256: halod_plugin_signing::sha256_hex(&archive),
            size: archive.len() as u64,
            matches: declared.matches.clone(),
            uses: declared.uses.clone(),
        };
        for mismatched in [entry("2.0.0", "3.0"), entry("1.0.0", "3.1")] {
            let error = stage_bytes("official", &mismatched, &archive).unwrap_err();
            assert!(error
                .to_string()
                .contains("does not match its catalog entry"));
        }
        let mut understated = entry("1.0.0", "3.0");
        understated.uses = Default::default();
        let error = stage_bytes("official", &understated, &archive).unwrap_err();
        assert!(error
            .to_string()
            .contains("declares different uses or matches"));
        let leftovers = std::fs::read_dir(repo_dir("official"))
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(leftovers, 0);
        assert!(stage_bytes("official", &entry("1.0.0", "3.0"), &archive)
            .unwrap()
            .join("plugin.yaml")
            .is_file());
    }

    fn device_yaml(kind: u8, a: u16, b: u16) -> String {
        let matcher = match kind % 5 {
            0 => format!("hid: {{ vid: {}, pid: {} }}", a.max(1), b.max(1)),
            1 => "hid: { any: true }".to_owned(),
            2 => format!(
                "usb: {{ vid: {}, pids: [{}, {}], interface: {} }}",
                a.max(1),
                b.max(1),
                a.wrapping_add(b).max(1),
                b % 4
            ),
            3 => format!("command: tool{}", a % 3),
            _ => format!(
                "smbus: {{ bus: gpu, addresses: [0x67], pci_match: [{{ vendor: {a}, sub_vendor: {b} }}] }}"
            ),
        };
        format!("  - vendor: X\n    model: M{a}\n    match: {{ {matcher} }}\n")
    }

    proptest::proptest! {
        #[test]
        fn builder_declarations_equal_the_daemon_manifest(
            name in proptest::option::of("[A-Za-z][A-Za-z ]{0,11}"),
            devices in proptest::collection::vec((0u8..5, 0u16..0x200, 0u16..0x200), 0..4),
            transports in proptest::collection::btree_set(0usize..6, 0..4),
            commands in proptest::collection::vec("tool[0-2]", 0..3),
            consumes in proptest::collection::vec("(gen|other|host)\\.[a-z]{1,4}", 0..3),
        ) {
            let mut yaml = String::new();
            if let Some(name) = &name {
                yaml.push_str(&format!("name: '{name}'\n"));
            }
            if !devices.is_empty() {
                yaml.push_str("devices:\n");
                for (kind, a, b) in &devices {
                    yaml.push_str(&device_yaml(*kind, *a, *b));
                }
            }
            if !transports.is_empty() || !commands.is_empty() {
                yaml.push_str("transports:\n");
            }
            for index in &transports {
                yaml.push_str(["  tcp: {}\n", "  hid: {}\n", "  usb: {}\n", "  hwmon: {}\n", "  audio_routing: {}\n", "  lpcio: {}\n"][*index]);
            }
            if !commands.is_empty() {
                yaml.push_str(&format!("  command: {{ commands: [{}] }}\n", commands.join(", ")));
            }
            if !consumes.is_empty() {
                yaml.push_str(&format!("consumes: [{}]\n", consumes.join(", ")));
            }
            let repo = tempfile::tempdir().unwrap();
            let dir = repo.path().join("gen");
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("main.lua"), "return {}\n").unwrap();
            std::fs::write(
                dir.join("plugin.yaml"),
                format!("id: gen\nversion: 1.0.0\napi: '3.0'\n{yaml}"),
            )
            .unwrap();

            let out = tempfile::tempdir().unwrap();
            let built = halod_plugin_signing::catalog::build_catalog(
                halod_plugin_signing::catalog::BuildInput {
                    repo_dir: repo.path(),
                    out_dir: out.path(),
                    name: "Test",
                    commit: None,
                    published: 1,
                    previous: None,
                },
                |_| unreachable!(),
            )
            .unwrap();
            let manifest = super::super::manifest::build_manifest_from_dir(&dir).unwrap();
            let entry = &built.packages[0];
            proptest::prop_assert_eq!(&entry.name, &manifest.display_name());
            proptest::prop_assert_eq!(&entry.matches, &manifest.declarations.matches);
            proptest::prop_assert_eq!(&entry.uses, &manifest.declarations.uses);
        }
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
