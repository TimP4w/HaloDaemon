// SPDX-License-Identifier: GPL-3.0-or-later
//! Release metadata parsing for the application update check.

use anyhow::{Context, Result};
use halod_shared::app::SUMS_SIG_ASSET;
use semver::Version;
use serde::Deserialize;

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
}

/// `None` until the release carries its signed checksums.
pub fn latest_release(json: &[u8]) -> Result<Option<Version>> {
    let release: Release = serde_json::from_slice(json).context("parsing release metadata")?;
    let tag = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let version =
        Version::parse(tag).with_context(|| format!("parsing tag '{}'", release.tag_name))?;
    let signed = release
        .assets
        .iter()
        .any(|asset| asset.name == SUMS_SIG_ASSET);
    Ok(signed.then_some(version))
}

pub fn newer(current: &Version, latest: Version) -> Option<Version> {
    (latest > *current).then_some(latest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn release(tag: &str, assets: &[&str]) -> Vec<u8> {
        let assets: Vec<_> = assets
            .iter()
            .map(|name| serde_json::json!({ "name": name }))
            .collect();
        serde_json::json!({ "tag_name": tag, "assets": assets })
            .to_string()
            .into_bytes()
    }

    #[test]
    fn signed_release_yields_its_version() {
        let json = release("v0.14.0", &["SHA256SUMS", "SHA256SUMS.sig"]);
        assert_eq!(latest_release(&json).unwrap(), Some(Version::new(0, 14, 0)));
    }

    #[test]
    fn release_without_signature_is_hidden() {
        let json = release("v0.14.0", &["SHA256SUMS"]);
        assert_eq!(latest_release(&json).unwrap(), None);
    }

    #[test]
    fn malformed_tag_and_json_are_errors() {
        assert!(latest_release(&release("nightly", &["SHA256SUMS.sig"])).is_err());
        assert!(latest_release(b"not json").is_err());
    }

    fn version() -> impl Strategy<Value = Version> {
        (0u64..5, 0u64..5, 0u64..5).prop_map(|(a, b, c)| Version::new(a, b, c))
    }

    proptest! {
        #[test]
        fn newer_returns_latest_exactly_when_it_is_greater(current in version(), latest in version()) {
            let expected = (latest > current).then_some(latest.clone());
            prop_assert_eq!(newer(&current, latest), expected);
        }
    }
}
