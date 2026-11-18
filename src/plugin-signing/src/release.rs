// SPDX-License-Identifier: GPL-3.0-or-later
//! Signed `SHA256SUMS` manifests for application releases.

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};

use crate::catalog::{decode_array, verifying_key};

pub fn message(version: &str, sums: &[u8]) -> Vec<u8> {
    let mut out = format!("halod-release v{version}\n").into_bytes();
    out.extend_from_slice(sums);
    out
}

pub fn sign(version: &str, sums: &[u8], key_id: &str, key: &SigningKey) -> String {
    let signature = B64.encode(key.sign(&message(version, sums)).to_bytes());
    format!("{key_id} {signature}\n")
}

pub fn verify(version: &str, sums: &[u8], signature: &str, trusted: &[(&str, &str)]) -> Result<()> {
    let (key_id, signature) = signature
        .trim()
        .split_once(' ')
        .ok_or_else(|| anyhow!("signature must be '<key-id> <signature>'"))?;
    let public = trusted
        .iter()
        .find(|(id, _)| *id == key_id)
        .map(|(_, key)| *key)
        .ok_or_else(|| anyhow!("release signed by untrusted key '{key_id}'"))?;
    let key = verifying_key(public)?;
    let signature = decode_array::<64>(signature).context("decoding release signature")?;
    if key
        .verify(&message(version, sums), &Signature::from_bytes(&signature))
        .is_err()
    {
        bail!("release signature does not match");
    }
    Ok(())
}

/// Looks up `file` in `sha256sum` output (`<hex>  <name>`, `*<name>` in binary mode).
pub fn expected_hash<'a>(sums: &'a str, file: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        let lowercase_hex = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'f');
        (name == file && hash.len() == 64 && hash.bytes().all(lowercase_hex)).then_some(hash)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMS: &[u8] =
        b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  halod-setup-x64.exe\n";

    fn key(seed: u8) -> (SigningKey, String) {
        let key = SigningKey::from_bytes(&[seed; 32]);
        let public = B64.encode(key.verifying_key().to_bytes());
        (key, public)
    }

    #[test]
    fn signed_manifest_round_trips() {
        let (key, public) = key(7);
        let sig = sign("0.14.0", SUMS, "rel", &key);
        verify("0.14.0", SUMS, &sig, &[("rel", &public)]).unwrap();
    }

    #[test]
    fn signature_is_bound_to_the_version() {
        let (key, public) = key(7);
        let sig = sign("0.13.0", SUMS, "rel", &key);
        assert!(verify("0.14.0", SUMS, &sig, &[("rel", &public)]).is_err());
    }

    #[test]
    fn wrong_key_is_rejected() {
        let (key, _) = key(7);
        let (_, other) = self::key(9);
        let sig = sign("0.14.0", SUMS, "rel", &key);
        assert!(verify("0.14.0", SUMS, &sig, &[("rel", &other)]).is_err());
    }

    #[test]
    fn tampered_sums_are_rejected() {
        let (key, public) = key(7);
        let sig = sign("0.14.0", SUMS, "rel", &key);
        let mut tampered = SUMS.to_vec();
        tampered[0] = b'b';
        assert!(verify("0.14.0", &tampered, &sig, &[("rel", &public)]).is_err());
    }

    #[test]
    fn unknown_key_id_is_rejected() {
        let (key, public) = key(7);
        let sig = sign("0.14.0", SUMS, "other", &key);
        let error = verify("0.14.0", SUMS, &sig, &[("rel", &public)]).unwrap_err();
        assert!(error.to_string().contains("untrusted key 'other'"));
    }

    #[test]
    fn expected_hash_reads_text_and_binary_lines() {
        let hash = "a".repeat(64);
        let upper = "A".repeat(64);
        let sums =
            format!("{hash}  halod-setup-x64.exe\n{hash} *halod.tar.gz\nbad  x\n{upper}  up\n");
        assert_eq!(
            expected_hash(&sums, "halod-setup-x64.exe"),
            Some(hash.as_str())
        );
        assert_eq!(expected_hash(&sums, "halod.tar.gz"), Some(hash.as_str()));
        assert_eq!(expected_hash(&sums, "x"), None);
        assert_eq!(expected_hash(&sums, "missing"), None);
        assert_eq!(expected_hash(&sums, "up"), None);
    }

    #[test]
    fn official_pinned_keys_are_valid_ed25519_keys() {
        assert!(!halod_shared::app::OFFICIAL_PUBLIC_KEYS.is_empty());
        for (id, key) in halod_shared::app::OFFICIAL_PUBLIC_KEYS {
            assert!(halod_shared::ids::is_identifier(id), "{id}");
            verifying_key(key).unwrap();
        }
    }
}
