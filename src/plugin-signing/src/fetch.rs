// SPDX-License-Identifier: GPL-3.0-or-later
use std::io::Read as _;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use ureq::RequestExt as _;

use crate::package::normalized_relative_path;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub fn https_url(value: &str) -> Result<url::Url> {
    let url = url::Url::parse(value).with_context(|| format!("parsing URL '{value}'"))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        bail!("'{value}' is not an https URL");
    }
    Ok(url)
}

pub fn package_url(catalog_url: &str, path: &str) -> Result<url::Url> {
    let base = https_url(catalog_url)?;
    let relative = normalized_relative_path(std::path::Path::new(path))?;
    let url = base.join(&relative)?;
    if url.origin() != base.origin() {
        bail!("package path '{path}' leaves the catalog origin");
    }
    Ok(url)
}

pub fn https_get(url: &url::Url, limit: u64) -> Result<Vec<u8>> {
    if url.scheme() != "https" {
        bail!("refusing non-https download {url}");
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(true)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_send_request(Some(RESPONSE_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .timeout_recv_body(Some(BODY_TIMEOUT))
        .build()
        .into();
    let mut response = ureq::http::Request::builder()
        .method("GET")
        .uri(url.as_str())
        .header("User-Agent", "HaloDaemon-plugins")
        .body(())
        .with_context(|| format!("building the request for {url}"))?
        .with_agent(&agent)
        .run()
        .with_context(|| format!("downloading {url}"))?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {url}"))?;
    if bytes.len() as u64 > limit {
        bail!("download from {url} exceeds {limit} bytes");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str = "https://example.github.io/plugins/catalog.yaml";

    #[test]
    fn package_paths_resolve_under_the_catalog_directory() {
        assert_eq!(
            package_url(CATALOG, "packages/demo-1.0.0.tar.gz")
                .unwrap()
                .as_str(),
            "https://example.github.io/plugins/packages/demo-1.0.0.tar.gz"
        );
    }

    #[test]
    fn package_paths_cannot_escape_or_change_origin() {
        for bad in [
            "../other/x.tar.gz",
            "/plugins/x.tar.gz",
            "//evil.example/x.tar.gz",
            "https://evil.example/x.tar.gz",
            "packages/../../x.tar.gz",
        ] {
            assert!(package_url(CATALOG, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_https_catalogs_are_accepted() {
        assert!(https_url("http://example.github.io/catalog.yaml").is_err());
        assert!(https_url("file:///tmp/catalog.yaml").is_err());
        assert!(https_url(CATALOG).is_ok());
    }
}
