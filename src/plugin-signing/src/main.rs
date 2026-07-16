// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::{Path, PathBuf};

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
        Some("keygen") => keygen(args.get(1).map(String::as_str)),
        _ => bail!(
            "usage:\n  halod-plugin-signing validate <repo>\n  halod-plugin-signing index <repo> [--version <semver>] [--check]\n  halod-plugin-signing bundle <repo> --commit <sha> --output <tar>\n  halod-plugin-signing sign <repo> --key-id <id> [--key-env <name>]\n  halod-plugin-signing verify <repo> --trusted-key <id=base64>...\n  halod-plugin-signing keygen [key-id]"
        ),
    }
}

fn bundle(args: &[String]) -> Result<()> {
    let repo = args
        .first()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("missing <repo>"))?;
    let commit = option(args, "--commit")?.ok_or_else(|| anyhow!("missing --commit"))?;
    let output = option(args, "--output")?
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("missing --output"))?;
    let metadata = signing::write_bundle(&repo, commit, &output)?;
    println!(
        "wrote {} for {} at {}",
        output.display(),
        metadata.repository_id,
        metadata.commit
    );
    Ok(())
}

fn index(args: &[String]) -> Result<()> {
    let repo = args
        .first()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("missing <repo>"))?;
    let mut version = None;
    let mut check = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--version" => {
                i += 1;
                version = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow!("--version requires a value"))?,
                );
            }
            "--check" => check = true,
            other => bail!("unknown index argument '{other}'"),
        }
        i += 1;
    }
    if check && version.is_some() {
        bail!("--check and --version are mutually exclusive");
    }
    let changed = signing::rewrite_index(&repo, version.map(String::as_str), check)?;
    println!(
        "{}",
        if changed {
            "wrote repository.yaml"
        } else {
            "repository.yaml is current"
        }
    );
    Ok(())
}

fn sign(args: &[String]) -> Result<()> {
    let repo = args
        .first()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("missing <repo>"))?;
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
    let payload = std::fs::read(repo.join("repository.yaml"))?;
    let signature = SigningKey::from_bytes(&seed).sign(&payload).to_bytes();
    atomic_write(
        &repo.join("repository.sig"),
        &signing::signature_bytes(key_id, &signature),
    )?;
    println!("wrote {}", repo.join("repository.sig").display());
    Ok(())
}

fn verify(args: &[String]) -> Result<()> {
    let repo = args
        .first()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("missing <repo>"))?;
    let mut keys = Vec::new();
    let mut i = 1;
    while i < args.len() {
        if args[i] != "--trusted-key" {
            bail!("unknown verify argument '{}'", args[i]);
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
    let manifest = signing::read_repository_index(&repo)?;
    signing::validate_compatibility(&manifest, env!("CARGO_PKG_VERSION"), 1)?;
    signing::validate_repository_packages(&repo, &manifest)?;
    let payload = std::fs::read(repo.join("repository.yaml"))?;
    let signature = std::fs::read(repo.join("repository.sig"))?;
    let borrowed: Vec<(&str, &str)> = keys
        .iter()
        .map(|(id, key)| (id.as_str(), key.as_str()))
        .collect();
    signing::verify_signature(&payload, &signature, &borrowed)?;
    println!("repository signature and all package hashes are valid");
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

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}
