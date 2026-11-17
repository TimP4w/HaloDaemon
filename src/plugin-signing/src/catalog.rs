// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::package::{
    discover_packages, extract_archive, normalized_relative_path, package_hash,
    validate_identifier, write_archive, MAX_ARCHIVE_BYTES,
};
use crate::{sha256_hex, PluginApi};

pub const CATALOG_FILE: &str = "catalog.yaml";
pub const CATALOG_SCHEMA: u32 = 1;
pub const MAX_CATALOG_BYTES: u64 = 2 * 1024 * 1024;
pub const SIGNATURE_ALGORITHM: &str = "ed25519";
const SIGNATURE_PREFIX: &str = "# halod-sig: ";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositorySigningKey {
    pub id: String,
    pub algorithm: String,
    pub public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub schema: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub published: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<RepositorySigningKey>,
    pub packages: Vec<CatalogPackage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogPackage {
    pub id: String,
    pub version: semver::Version,
    pub api: PluginApi,
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    Malformed(String),
    Unsigned,
    UnknownKey(String),
    SignatureMismatch,
    Stale {
        cached: u64,
        fetched: u64,
    },
    HashMismatch {
        package: String,
    },
    ApiIncompatible {
        required: PluginApi,
        supported: PluginApi,
    },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "catalog is malformed: {detail}"),
            Self::Unsigned => {
                f.write_str("catalog is unsigned but this repository requires a signature")
            }
            Self::UnknownKey(id) => write!(f, "catalog is signed by untrusted key '{id}'"),
            Self::SignatureMismatch => f.write_str("catalog signature does not match its contents"),
            Self::Stale { cached, fetched } => write!(
                f,
                "catalog published at {fetched} is older than the cached one from {cached}"
            ),
            Self::HashMismatch { package } => {
                write!(f, "package '{package}' does not match its catalog digest")
            }
            Self::ApiIncompatible {
                required,
                supported,
            } => write!(
                f,
                "plugin requires API {required}, this HaloDaemon supports {supported}"
            ),
        }
    }
}

impl std::error::Error for CatalogError {}

fn malformed(detail: impl fmt::Display) -> CatalogError {
    CatalogError::Malformed(detail.to_string())
}

impl Catalog {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_yaml::to_string(self)
            .expect("catalog serialization cannot fail")
            .into_bytes()
    }

    pub fn validate(&self) -> Result<(), CatalogError> {
        if self.schema != CATALOG_SCHEMA {
            return Err(malformed(format!("unsupported schema {}", self.schema)));
        }
        if self.name.trim().is_empty() {
            return Err(malformed("name is empty"));
        }
        if let Some(key) = &self.signing_key {
            validate_identifier("signing key id", &key.id).map_err(malformed)?;
            if key.algorithm != SIGNATURE_ALGORITHM {
                return Err(malformed(format!(
                    "unsupported algorithm '{}'",
                    key.algorithm
                )));
            }
            decode_array::<32>(&key.public_key).map_err(malformed)?;
        }
        let mut seen = HashSet::new();
        for package in &self.packages {
            validate_identifier("package id", &package.id).map_err(malformed)?;
            if normalized_relative_path(Path::new(&package.path))
                .ok()
                .as_deref()
                != Some(package.path.as_str())
            {
                return Err(malformed(format!("unsafe package path '{}'", package.path)));
            }
            if package.sha256.len() != 64
                || !package
                    .sha256
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Err(malformed(format!(
                    "package '{}' has a bad digest",
                    package.id
                )));
            }
            if package.size == 0 || package.size > MAX_ARCHIVE_BYTES {
                return Err(malformed(format!(
                    "package '{}' has a bad size",
                    package.id
                )));
            }
            if !seen.insert((&package.id, &package.version)) {
                return Err(malformed(format!(
                    "package '{}' {} is listed twice",
                    package.id, package.version
                )));
            }
        }
        Ok(())
    }

    pub fn select(&self, host: PluginApi) -> BTreeMap<&str, &CatalogPackage> {
        let mut best: BTreeMap<&str, &CatalogPackage> = BTreeMap::new();
        for package in &self.packages {
            let rank = |p: &CatalogPackage| (p.api.is_compatible_with(host), p.version.clone());
            match best.get(package.id.as_str()) {
                Some(current) if rank(current) >= rank(package) => {}
                _ => {
                    best.insert(&package.id, package);
                }
            }
        }
        best
    }
}

impl CatalogPackage {
    pub fn verify(&self, bytes: &[u8]) -> Result<(), CatalogError> {
        if bytes.len() as u64 != self.size || sha256_hex(bytes) != self.sha256 {
            return Err(CatalogError::HashMismatch {
                package: self.id.clone(),
            });
        }
        Ok(())
    }
}

pub fn open_catalog(
    bytes: &[u8],
    pinned: Option<&[(&str, &str)]>,
) -> Result<Catalog, CatalogError> {
    let (signature, body) = split_signature(bytes)?;
    let text = std::str::from_utf8(body).map_err(malformed)?;
    let catalog: Catalog = serde_yaml::from_str(text).map_err(malformed)?;
    catalog.validate()?;
    let Some((key_id, signature)) = signature else {
        if pinned.is_some() {
            return Err(CatalogError::Unsigned);
        }
        if catalog.signing_key.is_some() {
            return Err(malformed(
                "advertises a signing key but carries no signature",
            ));
        }
        return Ok(catalog);
    };
    let advertised = catalog
        .signing_key
        .as_ref()
        .filter(|key| key.id == key_id)
        .ok_or_else(|| malformed("signature key id does not match the advertised key"))?;
    let public_key = match pinned {
        Some(keys) => keys
            .iter()
            .find(|(id, _)| *id == key_id)
            .map(|(_, key)| *key)
            .filter(|key| *key == advertised.public_key)
            .ok_or_else(|| CatalogError::UnknownKey(key_id.to_owned()))?,
        None => advertised.public_key.as_str(),
    };
    let key = verifying_key(public_key)?;
    key.verify(body, &Signature::from_bytes(&signature))
        .map_err(|_| CatalogError::SignatureMismatch)?;
    Ok(catalog)
}

pub fn verifying_key(public_key: &str) -> Result<VerifyingKey, CatalogError> {
    let public = decode_array::<32>(public_key).map_err(malformed)?;
    VerifyingKey::from_bytes(&public).map_err(malformed)
}

pub fn sign_catalog(catalog: &mut Catalog, key_id: &str, key: &SigningKey) -> Vec<u8> {
    catalog.signing_key = Some(RepositorySigningKey {
        id: key_id.to_owned(),
        algorithm: SIGNATURE_ALGORITHM.to_owned(),
        public_key: B64.encode(key.verifying_key().to_bytes()),
    });
    let body = catalog.to_bytes();
    let signature = B64.encode(key.sign(&body).to_bytes());
    let mut out = format!("{SIGNATURE_PREFIX}{key_id} {signature}\n").into_bytes();
    out.extend_from_slice(&body);
    out
}

pub fn signing_key_fingerprint(key: &RepositorySigningKey) -> Result<String> {
    let public = decode_array::<32>(&key.public_key)?;
    let digest = sha256_hex(public);
    Ok(digest
        .as_bytes()
        .chunks(4)
        .map(|chunk| String::from_utf8_lossy(chunk).to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(":"))
}

fn split_signature(bytes: &[u8]) -> Result<(Option<(&str, [u8; 64])>, &[u8]), CatalogError> {
    let Some(rest) = bytes.strip_prefix(SIGNATURE_PREFIX.as_bytes()) else {
        return Ok((None, bytes));
    };
    let end = rest
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| malformed("signature line is not terminated"))?;
    let line = std::str::from_utf8(&rest[..end]).map_err(malformed)?;
    let (key_id, signature) = line
        .split_once(' ')
        .ok_or_else(|| malformed("signature line must be '<key-id> <signature>'"))?;
    let signature = decode_array::<64>(signature).map_err(malformed)?;
    Ok((Some((key_id, signature)), &rest[end + 1..]))
}

fn decode_array<const N: usize>(value: &str) -> Result<[u8; N]> {
    B64.decode(value.trim())
        .context("decoding base64")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected {N} bytes"))
}

pub struct BuildInput<'a> {
    pub repo_dir: &'a Path,
    pub out_dir: &'a Path,
    pub name: &'a str,
    pub commit: Option<String>,
    pub published: u64,
    pub previous: Option<&'a Catalog>,
}

pub fn build_catalog(
    input: BuildInput<'_>,
    fetch_previous: impl Fn(&CatalogPackage) -> Result<Vec<u8>>,
) -> Result<Catalog> {
    let previous = input.previous.map_or(&[][..], |catalog| &catalog.packages);
    let packages_dir = input.out_dir.join("packages");
    fs::create_dir_all(&packages_dir)?;
    let scratch = tempfile::tempdir()?;
    let mut packages = Vec::new();
    for (dir, meta) in discover_packages(input.repo_dir)? {
        let history: Vec<&CatalogPackage> = previous.iter().filter(|p| p.id == meta.id).collect();
        if let Some(newest) = history.iter().map(|p| &p.version).max() {
            if meta.version < *newest {
                bail!(
                    "'{}' {} is older than published {newest}",
                    meta.id,
                    meta.version
                );
            }
        }
        let (current, bytes) = match history.iter().find(|p| p.version == meta.version) {
            Some(published) => {
                let bytes = fetch_previous(published)?;
                published.verify(&bytes)?;
                let unpacked = scratch.path().join(format!("{}-{}", meta.id, meta.version));
                extract_archive(&bytes, &unpacked)?;
                if published.api != meta.api || package_hash(&unpacked)? != package_hash(&dir)? {
                    bail!(
                        "'{}' changed but its version is still {}; bump the version",
                        meta.id,
                        meta.version
                    );
                }
                ((*published).clone(), bytes)
            }
            None => {
                let bytes = write_archive(&dir)?;
                let package = CatalogPackage {
                    path: format!("packages/{}-{}.tar.gz", meta.id, meta.version),
                    sha256: sha256_hex(&bytes),
                    size: bytes.len() as u64,
                    id: meta.id.clone(),
                    version: meta.version.clone(),
                    api: meta.api,
                };
                (package, bytes)
            }
        };
        let mut kept: BTreeMap<PluginApi, &CatalogPackage> = BTreeMap::new();
        for older in history.into_iter().filter(|p| p.api != current.api) {
            if kept
                .get(&older.api)
                .is_none_or(|k| k.version < older.version)
            {
                kept.insert(older.api, older);
            }
        }
        write_package(&packages_dir, &current, &bytes)?;
        packages.push(current);
        for older in kept.into_values() {
            let bytes = fetch_previous(older)?;
            older.verify(&bytes)?;
            write_package(&packages_dir, older, &bytes)?;
            packages.push(older.clone());
        }
    }
    packages.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| b.version.cmp(&a.version)));
    let catalog = Catalog {
        schema: CATALOG_SCHEMA,
        name: input.name.to_owned(),
        commit: input.commit,
        published: input.published,
        signing_key: None,
        packages,
    };
    catalog.validate()?;
    let licenses = input.repo_dir.join("licenses.txt");
    if licenses.is_file() {
        fs::copy(&licenses, input.out_dir.join("licenses.txt"))?;
    }
    fs::write(input.out_dir.join(CATALOG_FILE), catalog.to_bytes())?;
    Ok(catalog)
}

fn write_package(packages_dir: &Path, package: &CatalogPackage, bytes: &[u8]) -> Result<()> {
    let name = Path::new(&package.path)
        .file_name()
        .context("package path has no file name")?;
    fs::write(packages_dir.join(name), bytes)?;
    Ok(())
}

pub fn verify_published(dir: &Path, trusted: &[(&str, &str)]) -> Result<Catalog> {
    let bytes = fs::read(dir.join(CATALOG_FILE))?;
    let catalog = open_catalog(&bytes, Some(trusted))?;
    for package in &catalog.packages {
        let archive = fs::read(dir.join(&package.path))
            .with_context(|| format!("reading {}", package.path))?;
        package.verify(&archive)?;
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::tests::package;

    const KEY_SEED: [u8; 32] = [7; 32];

    fn api(value: &str) -> PluginApi {
        value.parse().unwrap()
    }

    fn entry(id: &str, version: &str, api_value: &str) -> CatalogPackage {
        CatalogPackage {
            id: id.into(),
            version: version.parse().unwrap(),
            api: api(api_value),
            path: format!("packages/{id}-{version}.tar.gz"),
            sha256: "a".repeat(64),
            size: 1,
        }
    }

    fn catalog(packages: Vec<CatalogPackage>) -> Catalog {
        Catalog {
            schema: CATALOG_SCHEMA,
            name: "Test".into(),
            commit: None,
            published: 100,
            signing_key: None,
            packages,
        }
    }

    fn signed(mut catalog: Catalog) -> (Vec<u8>, String) {
        let key = SigningKey::from_bytes(&KEY_SEED);
        let bytes = sign_catalog(&mut catalog, "test-key", &key);
        (bytes, B64.encode(key.verifying_key().to_bytes()))
    }

    #[test]
    fn select_prefers_the_newest_compatible_version_and_falls_back() {
        let catalog = catalog(vec![
            entry("demo", "1.2.0", "3.1"),
            entry("demo", "1.1.0", "3.0"),
            entry("demo", "1.0.0", "3.0"),
            entry("future", "2.0.0", "4.0"),
        ]);
        let host = api("3.0");
        let selected = catalog.select(host);
        assert_eq!(selected["demo"].version.to_string(), "1.1.0");
        assert_eq!(selected["future"].version.to_string(), "2.0.0");
        assert!(!selected["future"].api.is_compatible_with(host));
        assert_eq!(
            catalog.select(api("3.1"))["demo"].version.to_string(),
            "1.2.0"
        );
    }

    #[test]
    fn signed_catalog_round_trips_and_rejects_tampering() {
        let (bytes, public) = signed(catalog(vec![entry("demo", "1.0.0", "3.0")]));
        let keys = [("test-key", public.as_str())];
        let opened = open_catalog(&bytes, Some(&keys)).unwrap();
        assert_eq!(opened.packages.len(), 1);
        assert_eq!(open_catalog(&bytes, None).unwrap(), opened);

        let text = String::from_utf8(bytes).unwrap();
        let tampered = text.replace("published: 100", "published: 101");
        assert_eq!(
            open_catalog(tampered.as_bytes(), Some(&keys)),
            Err(CatalogError::SignatureMismatch)
        );
    }

    #[test]
    fn pinned_repositories_reject_unsigned_and_other_keys() {
        let unsigned = catalog(vec![]).to_bytes();
        let (bytes, public) = signed(catalog(vec![]));
        let other = B64.encode(SigningKey::from_bytes(&[9; 32]).verifying_key().to_bytes());
        assert_eq!(
            open_catalog(&unsigned, Some(&[("test-key", public.as_str())])),
            Err(CatalogError::Unsigned)
        );
        assert_eq!(
            open_catalog(&bytes, Some(&[("test-key", other.as_str())])),
            Err(CatalogError::UnknownKey("test-key".into()))
        );
        assert_eq!(
            open_catalog(&bytes, Some(&[("other-id", public.as_str())])),
            Err(CatalogError::UnknownKey("test-key".into()))
        );
        assert!(open_catalog(&unsigned, None).is_ok());
    }

    #[test]
    fn validation_rejects_unsafe_paths_and_duplicates() {
        for path in [
            "../x.tar.gz",
            "/x.tar.gz",
            "https://evil/x.tar.gz",
            "a/./b.tar.gz",
        ] {
            let mut bad = entry("demo", "1.0.0", "3.0");
            bad.path = path.into();
            assert!(catalog(vec![bad]).validate().is_err(), "{path}");
        }
        let duplicate = catalog(vec![
            entry("demo", "1.0.0", "3.0"),
            entry("demo", "1.0.0", "3.1"),
        ]);
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn verify_rejects_a_mismatched_archive() {
        let mut package = entry("demo", "1.0.0", "3.0");
        package.sha256 = sha256_hex(b"archive");
        package.size = 7;
        assert!(package.verify(b"archive").is_ok());
        assert!(package.verify(b"archivX").is_err());
    }

    fn build(
        repo: &Path,
        previous: Option<&(Catalog, tempfile::TempDir)>,
    ) -> Result<(Catalog, tempfile::TempDir)> {
        let out = tempfile::tempdir()?;
        let catalog = build_catalog(
            BuildInput {
                repo_dir: repo,
                out_dir: out.path(),
                name: "Test",
                commit: None,
                published: 1,
                previous: previous.map(|(catalog, _)| catalog),
            },
            |package| Ok(fs::read(previous.unwrap().1.path().join(&package.path))?),
        )?;
        Ok((catalog, out))
    }

    #[test]
    fn content_change_without_version_bump_fails() {
        let repo = tempfile::tempdir().unwrap();
        let dir = package(repo.path(), "demo", "1.0.0", "3.0");
        let first = build(repo.path(), None).unwrap();
        let (unchanged, _) = build(repo.path(), Some(&first)).unwrap();
        assert_eq!(unchanged.packages, first.0.packages);
        fs::write(dir.join("main.lua"), "return {changed = true}\n").unwrap();
        let error = build(repo.path(), Some(&first)).unwrap_err();
        assert!(error.to_string().contains("bump the version"), "{error}");
    }

    #[test]
    fn older_api_entries_are_carried_forward() {
        let repo = tempfile::tempdir().unwrap();
        package(repo.path(), "demo", "1.0.0", "3.0");
        let first = build(repo.path(), None).unwrap();
        fs::remove_dir_all(repo.path().join("demo")).unwrap();
        package(repo.path(), "demo", "1.1.0", "3.1");

        let (second, out) = build(repo.path(), Some(&first)).unwrap();
        let versions: Vec<String> = second
            .packages
            .iter()
            .map(|p| p.version.to_string())
            .collect();
        assert_eq!(versions, ["1.1.0", "1.0.0"]);
        for package in &second.packages {
            package
                .verify(&fs::read(out.path().join(&package.path)).unwrap())
                .unwrap();
        }
    }

    #[test]
    fn a_version_older_than_published_fails() {
        let repo = tempfile::tempdir().unwrap();
        package(repo.path(), "demo", "2.0.0", "3.0");
        let first = build(repo.path(), None).unwrap();
        fs::remove_dir_all(repo.path().join("demo")).unwrap();
        package(repo.path(), "demo", "1.0.0", "3.0");
        let error = build(repo.path(), Some(&first)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("'demo' 1.0.0 is older than published 2.0.0"),
            "{error}"
        );
    }

    #[test]
    fn signature_framing_errors_name_the_exact_problem() {
        let (signed_bytes, _) = signed(catalog(Vec::new()));
        let text = String::from_utf8(signed_bytes).unwrap();
        let (signature_line, body) = text.split_once('\n').unwrap();
        let wrong_id = format!(
            "{}\n{body}",
            signature_line.replacen("test-key", "other-key", 1)
        );
        let cases = [
            (
                wrong_id.as_bytes(),
                "signature key id does not match the advertised key",
            ),
            (
                body.as_bytes(),
                "advertises a signing key but carries no signature",
            ),
            (
                b"# halod-sig: test-key AAAA".as_slice(),
                "signature line is not terminated",
            ),
        ];
        for (bytes, detail) in cases {
            assert_eq!(
                open_catalog(bytes, None).unwrap_err(),
                CatalogError::Malformed(detail.into())
            );
        }
    }
}
