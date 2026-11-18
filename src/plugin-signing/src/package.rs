// SPDX-License-Identifier: GPL-3.0-or-later
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{hex_lower, PluginApi};
use halod_shared::ids::PluginId;

pub const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4096;
const TAR_SLACK: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PackageMeta {
    pub id: PluginId,
    pub version: semver::Version,
    pub api: PluginApi,
}

pub fn read_package_meta(package_dir: &Path) -> Result<PackageMeta> {
    #[derive(Deserialize)]
    struct Raw {
        id: PluginId,
        version: String,
        api: PluginApi,
    }
    let path = package_dir.join("plugin.yaml");
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let raw: Raw =
        serde_yaml::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    let version = semver::Version::parse(&raw.version)
        .with_context(|| format!("parsing plugin '{}' version", raw.id))?;
    Ok(PackageMeta {
        id: raw.id,
        version,
        api: raw.api,
    })
}

pub fn discover_packages(repo_dir: &Path) -> Result<Vec<(PathBuf, PackageMeta)>> {
    let mut packages = Vec::new();
    for entry in
        fs::read_dir(repo_dir).with_context(|| format!("reading {}", repo_dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let dir = entry.path();
        if !entry.file_type()?.is_dir() || !dir.join("plugin.yaml").is_file() {
            continue;
        }
        let meta = read_package_meta(&dir)?;
        if name.to_str() != Some(meta.id.as_str()) {
            bail!(
                "package directory '{}' does not match plugin.yaml id '{}'",
                name.to_string_lossy(),
                meta.id
            );
        }
        reject_symlinks(&dir)?;
        packages.push((dir, meta));
    }
    packages.sort_by(|left, right| left.1.id.cmp(&right.1.id));
    Ok(packages)
}

pub fn package_hash(package_dir: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    for (relative, path) in sorted_files(package_dir)? {
        let bytes = fs::read(&path).with_context(|| format!("reading package file {relative}"))?;
        let bytes = normalize_line_endings(bytes);
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hex_lower(hasher.finalize()))
}

pub fn write_archive(package_dir: &Path) -> Result<Vec<u8>> {
    write_archive_within(package_dir, MAX_ARCHIVE_BYTES)
}

fn write_archive_within(package_dir: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::best()));
    for (relative, path) in sorted_files(package_dir)? {
        let bytes = fs::read(&path).with_context(|| format!("reading package file {relative}"))?;
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        builder
            .append_data(&mut header, &relative, bytes.as_slice())
            .with_context(|| format!("archiving {relative}"))?;
    }
    let bytes = builder.into_inner()?.finish()?;
    if bytes.len() as u64 > max_bytes {
        bail!("package archive exceeds {max_bytes} bytes");
    }
    Ok(bytes)
}

pub fn extract_archive(bytes: &[u8], destination: &Path) -> Result<()> {
    extract_archive_within(bytes, destination, MAX_EXTRACTED_BYTES)
}

fn extract_archive_within(bytes: &[u8], destination: &Path, max_extracted: u64) -> Result<()> {
    if destination.exists() {
        bail!(
            "extraction target already exists: {}",
            destination.display()
        );
    }
    fs::create_dir_all(destination)?;
    let stream = flate2::read::GzDecoder::new(bytes).take(max_extracted.saturating_add(TAR_SLACK));
    let mut archive = tar::Archive::new(stream);
    let mut extracted = 0_u64;
    for (index, entry) in archive
        .entries()
        .context("reading package archive")?
        .enumerate()
    {
        if index >= MAX_ARCHIVE_ENTRIES {
            bail!("package archive has more than {MAX_ARCHIVE_ENTRIES} entries");
        }
        let mut entry = entry?;
        let relative = entry.path()?.into_owned();
        let relative = normalized_relative_path(&relative)
            .with_context(|| format!("archive entry {}", relative.display()))?;
        let kind = entry.header().entry_type();
        let target = destination.join(&relative);
        if kind.is_dir() {
            fs::create_dir_all(target)?;
            continue;
        }
        if !kind.is_file() {
            bail!("package archive contains a link or special file: {relative}");
        }
        extracted = extracted.saturating_add(entry.size());
        if extracted > max_extracted {
            bail!("package archive expands beyond {max_extracted} bytes");
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut contents = Vec::new();
        entry.read_to_end(&mut contents)?;
        let mut output = fs::File::create_new(&target)
            .with_context(|| format!("archive entry {relative} is duplicated"))?;
        output.write_all(&contents)?;
    }
    Ok(())
}

pub fn reject_symlinks(root: &Path) -> Result<()> {
    walk_files(root).map(drop)
}

/// Wire-format relative path (`/`-separated) as it appears in archives,
/// catalogs and URLs; host separators are never accepted.
pub fn normalized_relative_path(path: &Path) -> Result<String> {
    if path.to_str().is_some_and(|raw| raw.contains('\\')) {
        bail!("invalid relative path '{}'", path.display());
    }
    host_relative_path(path)
}

/// Relative path from the host filesystem, normalized to `/` separators.
fn host_relative_path(path: &Path) -> Result<String> {
    if path.is_absolute() || path.as_os_str().is_empty() {
        bail!("invalid relative path '{}'", path.display());
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or_else(|| anyhow!("path '{}' is not UTF-8", path.display()))?,
            ),
            Component::CurDir => {}
            _ => bail!("invalid relative path '{}'", path.display()),
        }
    }
    if parts.is_empty() || parts.iter().any(|part| part.contains(['\\', ':'])) {
        bail!("invalid relative path '{}'", path.display());
    }
    Ok(parts.join("/"))
}

fn sorted_files(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut files = walk_files(root)?
        .into_iter()
        .map(|relative| Ok((host_relative_path(&relative)?, root.join(relative))))
        .collect::<Result<Vec<_>>>()?;
    files.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    Ok(files)
}

fn walk_files(root: &Path) -> Result<Vec<PathBuf>> {
    fn walk(root: &Path, current: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in
            fs::read_dir(current).with_context(|| format!("reading {}", current.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                bail!("package contains a symlink: {}", path.display());
            }
            if file_type.is_dir() {
                walk(root, &path, out)?;
            } else if file_type.is_file() {
                out.push(path.strip_prefix(root)?.to_path_buf());
            } else {
                bail!("package contains a non-regular file: {}", path.display());
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

fn normalize_line_endings(bytes: Vec<u8>) -> Vec<u8> {
    if bytes.contains(&0) {
        return bytes;
    }
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            normalized.push(b'\n');
            index += 2;
        } else {
            normalized.push(bytes[index]);
            index += 1;
        }
    }
    normalized
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn package(root: &Path, id: &str, version: &str, api: &str) -> PathBuf {
        let dir = root.join(id);
        fs::create_dir_all(dir.join("lib")).unwrap();
        fs::write(
            dir.join("plugin.yaml"),
            format!("id: {id}\nversion: {version}\napi: \"{api}\"\n"),
        )
        .unwrap();
        fs::write(dir.join("main.lua"), "return {}\n").unwrap();
        fs::write(dir.join("lib/util.lua"), "return {}\n").unwrap();
        dir
    }

    #[test]
    fn package_hash_changes_for_name_and_contents() {
        let root = tempfile::tempdir().unwrap();
        let dir = package(root.path(), "demo", "1.0.0", "1.0");
        let original = package_hash(&dir).unwrap();
        fs::write(dir.join("main.lua"), "return {x=1}\n").unwrap();
        let edited = package_hash(&dir).unwrap();
        assert_ne!(original, edited);
        fs::rename(dir.join("main.lua"), dir.join("renamed.lua")).unwrap();
        assert_ne!(edited, package_hash(&dir).unwrap());
    }

    #[test]
    fn package_hash_ignores_line_endings_but_not_binary_carriage_returns() {
        assert_eq!(normalize_line_endings(b"a\r\nb\n".to_vec()), b"a\nb\n");
        assert_eq!(normalize_line_endings(b"a\rb\n".to_vec()), b"a\rb\n");
        let binary = vec![0x00, 0x0d, 0x0a];
        assert_eq!(normalize_line_endings(binary.clone()), binary);
    }

    #[test]
    fn archives_are_deterministic_and_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let dir = package(root.path(), "demo", "1.0.0", "1.0");
        let archive = write_archive(&dir).unwrap();
        assert_eq!(archive, write_archive(&dir).unwrap());

        let out = root.path().join("out");
        extract_archive(&archive, &out).unwrap();
        assert_eq!(package_hash(&out).unwrap(), package_hash(&dir).unwrap());
    }

    #[test]
    fn extraction_refuses_an_existing_target() {
        let root = tempfile::tempdir().unwrap();
        let dir = package(root.path(), "demo", "1.0.0", "1.0");
        let archive = write_archive(&dir).unwrap();
        assert!(extract_archive(&archive, &dir).is_err());
    }

    #[test]
    fn extraction_stops_at_the_entry_cap() {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for index in 0..=MAX_ARCHIVE_ENTRIES {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            builder
                .append_data(&mut header, format!("d{index}"), std::io::empty())
                .unwrap();
        }
        let archive = builder.into_inner().unwrap().finish().unwrap();
        let out = tempfile::tempdir().unwrap();
        let error = extract_archive(&archive, &out.path().join("x")).unwrap_err();
        assert!(
            error.to_string().contains("more than 4096 entries"),
            "{error}"
        );
    }

    fn raw_archive(path: &str, kind: tar::EntryType) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(1);
        header.set_mode(0o644);
        {
            let name = &mut header.as_gnu_mut().unwrap().name;
            name[..path.len()].copy_from_slice(path.as_bytes());
        }
        header.set_cksum();
        builder.append(&header, &b"x"[..]).unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn a_long_name_entry_cannot_expand_past_the_stream_cap() {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(1);
        let long_name = "a".repeat(8 * 1024 * 1024);
        builder
            .append_data(&mut header, &long_name, &b"x"[..])
            .unwrap();
        let archive = builder.into_inner().unwrap().finish().unwrap();
        let out = tempfile::tempdir().unwrap();
        assert!(extract_archive_within(&archive, &out.path().join("x"), 4096).is_err());
    }

    #[test]
    fn an_archive_over_the_byte_cap_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = package(root.path(), "demo", "1.0.0", "1.0");
        let error = write_archive_within(&dir, 16).unwrap_err();
        assert!(error.to_string().contains("exceeds 16 bytes"), "{error}");
        assert!(write_archive_within(&dir, MAX_ARCHIVE_BYTES).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_package_directory_with_a_symlink_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = package(root.path(), "demo", "1.0.0", "1.0");
        assert!(reject_symlinks(&dir).is_ok());
        std::os::unix::fs::symlink(dir.join("plugin.yaml"), dir.join("alias.yaml")).unwrap();
        assert!(reject_symlinks(&dir).is_err());
        assert!(write_archive(&dir).is_err());
    }

    #[test]
    fn extraction_rejects_escaping_paths_and_links() {
        let root = tempfile::tempdir().unwrap();
        for (index, (path, kind, reason)) in [
            (
                "../escape.lua",
                tar::EntryType::Regular,
                "invalid relative path",
            ),
            ("/abs.lua", tar::EntryType::Regular, "invalid relative path"),
            ("link.lua", tar::EntryType::Symlink, "link or special file"),
        ]
        .into_iter()
        .enumerate()
        {
            let out = root.path().join(index.to_string());
            let error = extract_archive(&raw_archive(path, kind), &out).unwrap_err();
            assert!(format!("{error:#}").contains(reason), "{path}: {error:#}");
        }
        assert!(!root.path().join("escape.lua").exists());
    }

    #[test]
    fn relative_paths_cannot_escape_or_be_absolute() {
        assert_eq!(
            normalized_relative_path(Path::new("./packages/a.tar.gz")).unwrap(),
            "packages/a.tar.gz"
        );
        for bad in ["", "/x", "../x", "a/../b", "a\\b", "https://x", "."] {
            assert!(normalized_relative_path(Path::new(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn discovery_requires_directory_to_match_id() {
        let root = tempfile::tempdir().unwrap();
        package(root.path(), "demo", "1.0.0", "1.0");
        assert_eq!(discover_packages(root.path()).unwrap()[0].1.id, "demo");
        fs::rename(root.path().join("demo"), root.path().join("other")).unwrap();
        assert!(discover_packages(root.path()).is_err());
    }
}
