// SPDX-License-Identifier: GPL-3.0-or-later
//! Immutable GitHub release transport for standalone plugins and plugin packs.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use ureq::RequestExt as _;

use super::repo::{RepositoryManifest, RepositoryTrust};

const MANIFEST_ASSET: &str = "release.yaml";
const SIGNATURE_ASSET: &str = "release.sig";
const MAX_MANIFEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedRelease {
    pub tag: String,
    pub prerelease: bool,
    pub published_at: Option<String>,
    assets: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<String>,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

fn github_project(source: &str) -> Result<(String, String)> {
    let url = url::Url::parse(source).context("parsing plugin release source URL")?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") {
        bail!("plugin release source must be an https://github.com/<owner>/<project> URL");
    }
    let mut parts = url
        .path_segments()
        .ok_or_else(|| anyhow!("release source URL has no project path"))?
        .filter(|part| !part.is_empty());
    let owner = parts.next().context("release source URL has no owner")?;
    let project = parts
        .next()
        .context("release source URL has no project")?
        .trim_end_matches(".git");
    if parts.next().is_some()
        || !owner.bytes().all(github_name_byte)
        || !project.bytes().all(github_name_byte)
    {
        bail!("release source must identify exactly one GitHub project");
    }
    Ok((owner.to_owned(), project.to_owned()))
}

fn github_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

fn http_get(url: &str, limit: usize) -> Result<Vec<u8>> {
    let agent = ureq::Agent::config_builder()
        .max_redirects(5)
        .http_status_as_error(true)
        .build()
        .into();
    let request = ureq::http::Request::builder()
        .method("GET")
        .uri(url)
        .header("User-Agent", "HaloDaemon-plugin-releases")
        .header("Accept", "application/vnd.github+json")
        .body(())?;
    let mut response = request
        .with_agent(&agent)
        .run()
        .map_err(|error| anyhow!("downloading {url}: {error}"))?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("download from {url} exceeds {limit} bytes");
    }
    Ok(bytes)
}

pub fn list(source: &str) -> Result<Vec<PublishedRelease>> {
    let (owner, project) = github_project(source)?;
    let endpoint = format!("https://api.github.com/repos/{owner}/{project}/releases?per_page=100");
    let releases: Vec<GithubRelease> =
        serde_json::from_slice(&http_get(&endpoint, MAX_MANIFEST_BYTES)?)
            .context("parsing GitHub releases response")?;
    Ok(releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let assets: HashMap<_, _> = release
                .assets
                .into_iter()
                .map(|asset| (asset.name, asset.browser_download_url))
                .collect();
            (assets.contains_key(MANIFEST_ASSET) && assets.contains_key(SIGNATURE_ASSET)).then_some(
                PublishedRelease {
                    tag: release.tag_name,
                    prerelease: release.prerelease,
                    published_at: release.published_at,
                    assets,
                },
            )
        })
        .collect())
}

pub fn inspect(
    release: &PublishedRelease,
    trust: &RepositoryTrust,
) -> Result<(RepositoryManifest, Vec<u8>, Vec<u8>)> {
    let manifest_bytes = http_get(
        release
            .assets
            .get(MANIFEST_ASSET)
            .context("release.yaml asset is missing")?,
        MAX_MANIFEST_BYTES,
    )?;
    let signature_bytes = http_get(
        release
            .assets
            .get(SIGNATURE_ASSET)
            .context("release.sig asset is missing")?,
        MAX_MANIFEST_BYTES,
    )?;
    let manifest: RepositoryManifest =
        serde_yaml::from_slice(&manifest_bytes).context("parsing release.yaml")?;
    halod_plugin_signing::validate_repository_index(&manifest)?;
    match trust {
        RepositoryTrust::Official => halod_plugin_signing::verify_signature(
            &manifest_bytes,
            &signature_bytes,
            crate::constants::OFFICIAL_PLUGIN_REPO_PUBLIC_KEYS,
        )?,
        RepositoryTrust::Pinned(key) => halod_plugin_signing::verify_advertised_signature(
            &manifest_bytes,
            &signature_bytes,
            key,
        )?,
        RepositoryTrust::Unsigned => {}
    }
    Ok((manifest, manifest_bytes, signature_bytes))
}

pub fn download(
    release: &PublishedRelease,
    manifest: &RepositoryManifest,
    manifest_bytes: &[u8],
    signature_bytes: &[u8],
    destination: &Path,
) -> Result<()> {
    let archive = manifest
        .archive
        .as_ref()
        .context("network release manifest has no archive")?;
    let url = release
        .assets
        .get(&archive.name)
        .with_context(|| format!("release asset '{}' is missing", archive.name))?;
    let bytes = http_get(url, MAX_ARCHIVE_BYTES)?;
    if bytes.len() as u64 != archive.size {
        bail!("release archive size mismatch");
    }
    let actual = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if !actual.eq_ignore_ascii_case(&archive.sha256) {
        bail!("release archive hash mismatch");
    }
    if destination.exists() {
        bail!(
            "release destination already exists: {}",
            destination.display()
        );
    }
    std::fs::create_dir_all(destination)?;
    extract_tar_gz(&bytes, destination)?;
    std::fs::write(destination.join(MANIFEST_ASSET), manifest_bytes)?;
    std::fs::write(destination.join(SIGNATURE_ASSET), signature_bytes)?;
    Ok(())
}

fn extract_tar_gz(bytes: &[u8], destination: &Path) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive
        .entries()
        .context("reading plugin release archive")?
    {
        let mut entry = entry?;
        let relative = entry.path()?.into_owned();
        if relative.is_absolute()
            || relative.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            })
        {
            bail!(
                "release archive contains unsafe path {}",
                relative.display()
            );
        }
        let kind = entry.header().entry_type();
        let target = destination.join(&relative);
        if kind.is_dir() {
            std::fs::create_dir_all(target)?;
        } else if kind.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut output = std::fs::File::create(target)?;
            std::io::copy(&mut entry, &mut output)?;
            output.flush()?;
        } else {
            bail!("release archive contains a link or special file");
        }
    }
    Ok(())
}

pub fn safe_tag(tag: &str) -> Result<String> {
    if tag.is_empty()
        || tag.len() > 128
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("release tag contains unsupported characters");
    }
    Ok(tag.to_owned())
}

pub fn revision_dir(root: &Path, tag: &str) -> Result<PathBuf> {
    Ok(root.join("revisions").join(safe_tag(tag)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_project_root_urls() {
        assert_eq!(
            github_project("https://github.com/owner/project.git").unwrap(),
            ("owner".to_owned(), "project".to_owned())
        );
        assert!(github_project("http://github.com/owner/project").is_err());
        assert!(github_project("https://example.com/owner/project").is_err());
        assert!(github_project("https://github.com/owner/project/releases").is_err());
    }

    #[test]
    fn release_tags_cannot_escape_revision_store() {
        assert_eq!(safe_tag("v2026.7.4").unwrap(), "v2026.7.4");
        assert!(safe_tag("../escape").is_err());
        assert!(safe_tag("feature/name").is_err());
    }
}
