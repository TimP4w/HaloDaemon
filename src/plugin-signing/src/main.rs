// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use halod_plugin_signing as signing;
use zeroize::Zeroizing;

const DEFAULT_KEY_ENV: &str = "HALOD_PLUGIN_SIGNING_KEY_B64";

fn main() {
    if let Err(error) = run(std::env::args().skip(1).collect()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("validate") => {
            let repo = one_repo_arg(&args[1..])?;
            let manifest = signing::read_repository_index(&repo)?;
            signing::validate_repository(&repo, &manifest)?;
            println!("validated {} packages", manifest.packages.len());
            Ok(())
        }
        Some("index") => index(&args[1..]),
        Some("bundle") => bundle(&args[1..]),
        Some("sign") => sign(&args[1..]),
        Some("verify") => verify(&args[1..]),
        Some("verify-assets") => verify_assets(&args[1..]),
        Some("keygen") => keygen(args.get(1).map(String::as_str)),
        _ => bail!(
            "usage:\n  halod-plugin-signing validate <release-dir>\n  halod-plugin-signing index <release-dir> [--version <semver>] [--id <id> --name <name>] [--check]\n  halod-plugin-signing bundle <release-dir> --output <bundle>\n  halod-plugin-signing sign <release-dir> --key-id <id> [--key-env <name>]\n  halod-plugin-signing verify <release-dir> --trusted-key <id=base64>...\n  halod-plugin-signing verify-assets <asset-dir> --trusted-key <id=base64>...\n  halod-plugin-signing keygen [key-id]"
        ),
    }
}

fn bundle(args: &[String]) -> Result<()> {
    let repo = repo_arg(args)?;
    let output = option(args, "--output")?
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("missing --output"))?;
    let metadata = signing::write_bundle(&repo, &output)?;
    println!(
        "wrote {} for {} at {}",
        output.display(),
        metadata.repository_id,
        metadata.release
    );
    Ok(())
}

fn index(args: &[String]) -> Result<()> {
    let repo = repo_arg(args)?;
    let version = option(args, "--version")?;
    let id = option(args, "--id")?;
    let name = option(args, "--name")?;
    let check = args.iter().any(|arg| arg == "--check");
    if let Some(arg) = args.iter().skip(1).find(|arg| {
        arg.starts_with("--") && !["--version", "--id", "--name", "--check"].contains(&arg.as_str())
    }) {
        bail!("unknown index argument '{arg}'");
    }
    if check && version.is_some() {
        bail!("--check and --version are mutually exclusive");
    }
    if id.is_some() != name.is_some() {
        bail!("--id and --name must be provided together");
    }
    let changed = signing::rewrite_index(&repo, version, check, id.zip(name))?;
    println!(
        "{}",
        if changed {
            "wrote release.yaml"
        } else {
            "release.yaml is current"
        }
    );
    Ok(())
}

fn sign(args: &[String]) -> Result<()> {
    let repo = repo_arg(args)?;
    let key_id = option(args, "--key-id")?.ok_or_else(|| anyhow!("missing --key-id"))?;
    let key_env = option(args, "--key-env")?.unwrap_or(DEFAULT_KEY_ENV);
    let encoded = Zeroizing::new(
        std::env::var(key_env).with_context(|| format!("reading signing key from {key_env}"))?,
    );
    let decoded = Zeroizing::new(B64.decode(encoded.trim()).context("decoding signing key")?);
    let seed = Zeroizing::new(
        decoded
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("signing key seed is not 32 bytes"))?,
    );
    let signing_key = SigningKey::from_bytes(&seed);
    let mut manifest = signing::read_repository_index(&repo)?;
    manifest.signing_key = Some(signing::RepositorySigningKey {
        id: key_id.to_owned(),
        algorithm: signing::SIGNATURE_ALGORITHM.to_owned(),
        public_key: B64.encode(signing_key.verifying_key().to_bytes()),
    });
    let payload = signing::canonical_index_bytes(&manifest)?;
    signing::atomic_write(&repo.join("release.yaml"), &payload)?;
    let signature = signing_key.sign(&payload).to_bytes();
    signing::atomic_write(
        &repo.join("release.sig"),
        &signing::signature_bytes(key_id, &signature),
    )?;
    println!("wrote {}", repo.join("release.sig").display());
    Ok(())
}

fn verify(args: &[String]) -> Result<()> {
    let repo = repo_arg(args)?;
    let keys = trusted_keys(&args[1..])?;
    let manifest = signing::read_repository_index(&repo)?;
    signing::validate_repository(&repo, &manifest)?;
    let payload = std::fs::read(repo.join("release.yaml"))?;
    let signature = std::fs::read(repo.join("release.sig"))?;
    let borrowed = borrow_keys(&keys);
    signing::verify_signature(&payload, &signature, &borrowed)?;
    println!("release signature and all package hashes are valid");
    Ok(())
}

fn verify_assets(args: &[String]) -> Result<()> {
    let repo = repo_arg(args)?;
    let keys = trusted_keys(&args[1..])?;
    let borrowed = borrow_keys(&keys);
    signing::verify_release_assets(&repo, &borrowed)?;
    println!("release signature and archive are valid");
    Ok(())
}

fn trusted_keys(args: &[String]) -> Result<Vec<(String, String)>> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] != "--trusted-key" {
            bail!("unknown verification argument '{}'", args[i]);
        }
        i += 1;
        let value = args
            .get(i)
            .ok_or_else(|| anyhow!("--trusted-key requires id=base64"))?;
        let (id, key) = value
            .split_once('=')
            .ok_or_else(|| anyhow!("trusted key must be id=base64"))?;
        keys.push((id.to_owned(), key.to_owned()));
        i += 1;
    }
    if keys.is_empty() {
        bail!("at least one --trusted-key is required");
    }
    Ok(keys)
}

fn borrow_keys(keys: &[(String, String)]) -> Vec<(&str, &str)> {
    keys.iter()
        .map(|(id, key)| (id.as_str(), key.as_str()))
        .collect()
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

fn repo_arg(args: &[String]) -> Result<PathBuf> {
    args.first()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("missing <repo>"))
}

fn one_repo_arg(args: &[String]) -> Result<PathBuf> {
    match args {
        [repo] => Ok(repo.into()),
        _ => bail!("expected exactly one <repo> argument"),
    }
}

fn option<'a>(args: &'a [String], name: &str) -> Result<Option<&'a str>> {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    args.get(index + 1)
        .map(String::as_str)
        .map(Some)
        .ok_or_else(|| anyhow!("{name} requires a value"))
}
