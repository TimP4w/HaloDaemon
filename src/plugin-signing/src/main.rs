// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::SigningKey;
use halod_plugin_signing::catalog::{
    build_catalog, open_catalog, sign_catalog, verify_published, BuildInput, CATALOG_FILE,
    MAX_CATALOG_BYTES,
};
use halod_plugin_signing::fetch::{https_get, https_url, package_url};
use halod_plugin_signing::package::MAX_ARCHIVE_BYTES;
use zeroize::Zeroizing;

const DEFAULT_KEY_ENV: &str = "HALOD_PLUGIN_SIGNING_KEY_B64";
const USAGE: &str = "usage:
  halod-plugin-signing catalog <repo> --out <dir> --name <name> (--previous <url> [--trusted-key <id=base64>]... | --bootstrap)
  halod-plugin-signing sign <dir> --key-id <id> [--key-env <name>]
  halod-plugin-signing sign-release <SHA256SUMS> --version <v> --key-id <id> [--key-env <name>]
  halod-plugin-signing verify <dir> --trusted-key <id=base64>...
  halod-plugin-signing keygen [key-id]";

fn main() {
    if let Err(error) = run(std::env::args().skip(1).collect()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> Result<()> {
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("catalog") => catalog(rest),
        Some("sign") => sign(rest),
        Some("sign-release") => sign_release(rest),
        Some("verify") => verify(rest),
        Some("keygen") => keygen(rest.first().map(String::as_str)),
        _ => bail!("{USAGE}"),
    }
}

fn catalog(args: &[String]) -> Result<()> {
    let repo = positional(args)?;
    let out = PathBuf::from(required(args, "--out")?);
    let name = required(args, "--name")?;
    let bootstrap = args.iter().any(|arg| arg == "--bootstrap");
    let previous_url = option(args, "--previous")?;
    let previous = match (previous_url, bootstrap) {
        (Some(url), false) => {
            let bytes = https_get(&https_url(url)?, MAX_CATALOG_BYTES)
                .context("fetching the previous catalog (use --bootstrap for a first publish)")?;
            let keys = trusted_keys(args)?;
            let keys = borrow(&keys);
            Some(open_catalog(
                &bytes,
                (!keys.is_empty()).then_some(keys.as_slice()),
            )?)
        }
        (None, true) => None,
        _ => bail!("pass exactly one of --previous <url> or --bootstrap"),
    };
    let published = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let catalog = build_catalog(
        BuildInput {
            repo_dir: &repo,
            out_dir: &out,
            name,
            commit: std::env::var("GITHUB_SHA").ok(),
            published,
            previous: previous.as_ref(),
        },
        |package| {
            let url = previous_url.context("no previous catalog")?;
            https_get(&package_url(url, &package.path)?, MAX_ARCHIVE_BYTES)
        },
    )?;
    println!(
        "wrote {} with {} packages",
        out.join(CATALOG_FILE).display(),
        catalog.packages.len()
    );
    Ok(())
}

fn sign(args: &[String]) -> Result<()> {
    let dir = positional(args)?;
    let key_id = required(args, "--key-id")?;
    let key = signing_key(args)?;
    let path = dir.join(CATALOG_FILE);
    let mut catalog = open_catalog(&std::fs::read(&path)?, None)?;
    std::fs::write(&path, sign_catalog(&mut catalog, key_id, &key))?;
    println!("signed {}", path.display());
    Ok(())
}

fn sign_release(args: &[String]) -> Result<()> {
    let sums_path = positional(args)?;
    let version = required(args, "--version")?;
    let key_id = required(args, "--key-id")?;
    let key = signing_key(args)?;
    let sums = std::fs::read(&sums_path)?;
    let mut out = sums_path.into_os_string();
    out.push(".sig");
    std::fs::write(
        &out,
        halod_plugin_signing::release::sign(version, &sums, key_id, &key),
    )?;
    println!("wrote {}", Path::new(&out).display());
    Ok(())
}

fn signing_key(args: &[String]) -> Result<SigningKey> {
    let key_env = option(args, "--key-env")?.unwrap_or(DEFAULT_KEY_ENV);
    let encoded = Zeroizing::new(
        std::env::var(key_env).with_context(|| format!("reading signing key from {key_env}"))?,
    );
    let seed = Zeroizing::new(B64.decode(encoded.trim()).context("decoding signing key")?);
    let seed: [u8; 32] = seed
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("signing key seed is not 32 bytes"))?;
    Ok(SigningKey::from_bytes(&seed))
}

fn verify(args: &[String]) -> Result<()> {
    let dir = positional(args)?;
    let keys = trusted_keys(args)?;
    if keys.is_empty() {
        bail!("at least one --trusted-key is required");
    }
    let catalog = verify_published(&dir, &borrow(&keys))?;
    println!(
        "catalog signature and {} package archives are valid",
        catalog.packages.len()
    );
    Ok(())
}

fn keygen(key_id: Option<&str>) -> Result<()> {
    let mut seed = Zeroizing::new([0_u8; 32]);
    getrandom::fill(seed.as_mut()).context("reading OS randomness")?;
    let key = SigningKey::from_bytes(&seed);
    println!("key_id: {}", key_id.unwrap_or("halodaemon-official-2026"));
    println!("private_seed_b64: {}", B64.encode(seed.as_ref()));
    println!(
        "public_key_b64: {}",
        B64.encode(key.verifying_key().to_bytes())
    );
    Ok(())
}

fn trusted_keys(args: &[String]) -> Result<Vec<(String, String)>> {
    let mut keys = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--trusted-key" {
            let value = iter
                .next()
                .ok_or_else(|| anyhow!("--trusted-key requires id=base64"))?;
            let (id, key) = value
                .split_once('=')
                .ok_or_else(|| anyhow!("trusted key must be id=base64"))?;
            keys.push((id.to_owned(), key.to_owned()));
        }
    }
    Ok(keys)
}

fn borrow(keys: &[(String, String)]) -> Vec<(&str, &str)> {
    keys.iter()
        .map(|(id, key)| (id.as_str(), key.as_str()))
        .collect()
}

fn positional(args: &[String]) -> Result<PathBuf> {
    match args.first() {
        Some(arg) if !arg.starts_with("--") => Ok(Path::new(arg).to_path_buf()),
        _ => bail!("{USAGE}"),
    }
}

fn required<'a>(args: &'a [String], name: &str) -> Result<&'a str> {
    option(args, name)?.ok_or_else(|| anyhow!("missing {name}"))
}

fn option<'a>(args: &'a [String], name: &str) -> Result<Option<&'a str>> {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    args.get(index + 1)
        .map(|value| Some(value.as_str()))
        .ok_or_else(|| anyhow!("{name} requires a value"))
}

#[cfg(test)]
mod tests {
    use super::run;

    fn catalog_args(extra: &[&str]) -> Vec<String> {
        ["catalog", "repo", "--out", "out", "--name", "Test"]
            .iter()
            .chain(extra)
            .map(|arg| (*arg).to_owned())
            .collect()
    }

    #[test]
    fn catalog_needs_exactly_one_of_previous_or_bootstrap() {
        for extra in [
            &[][..],
            &[
                "--previous",
                "https://example.org/catalog.yaml",
                "--bootstrap",
            ][..],
        ] {
            let error = run(catalog_args(extra)).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("pass exactly one of --previous <url> or --bootstrap"),
                "{extra:?}: {error}"
            );
        }
    }
}
