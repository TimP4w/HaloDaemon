// SPDX-License-Identifier: GPL-3.0-or-later
//! Encrypted-file fallback for [`super::SecretStore`], used when the OS keyring
//! is unreachable (headless Linux with no D-Bus session, etc.).
//!
//! A random 32-byte key is generated on first use and kept at
//! `config_dir()/secret.key` (`0600` on Unix). Values are XChaCha20-Poly1305
//! sealed (a fresh random nonce per value) and stored as base64 tokens in
//! `plugin_secrets.yaml`. This protects secrets at rest against backups, sync,
//! and other users on the machine — not against another process already
//! running as this user, which could read the key file itself.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use anyhow::{anyhow, Context, Result};
use chacha20poly1305::aead::{Aead, Generate, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

use super::SecretStore;

const KEY_FILE: &str = "secret.key";
const SECRETS_FILE: &str = "plugin_secrets.yaml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SecretsFile {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    secrets: HashMap<String, HashMap<String, String>>,
}

pub struct FileKeyStore {
    cipher: XChaCha20Poly1305,
    /// Cached in-memory mirror of the on-disk file; guarded so `set`/`delete`
    /// read-modify-write atomically within this process.
    state: RwLock<SecretsFile>,
}

impl FileKeyStore {
    pub fn new() -> Self {
        let key = load_or_create_key();
        let state = load_secrets_file().unwrap_or_else(|e| {
            log::warn!("[secrets] failed to read {SECRETS_FILE}, starting empty: {e:#}");
            SecretsFile::default()
        });
        Self {
            cipher: XChaCha20Poly1305::new(&key),
            state: RwLock::new(state),
        }
    }

    fn encrypt(&self, plaintext: &str) -> Result<String> {
        let nonce = XNonce::generate();
        let ct = self
            .cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| anyhow!("encryption failed"))?;
        let mut blob = Vec::with_capacity(nonce.len() + ct.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ct);
        Ok(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            blob,
        ))
    }

    fn decrypt(&self, token: &str) -> Result<String> {
        let blob = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, token)
            .context("token is not valid base64")?;
        if blob.len() < 24 {
            anyhow::bail!("token too short to contain a nonce");
        }
        let (nonce_bytes, ct) = blob.split_at(24);
        let nonce = XNonce::try_from(nonce_bytes).map_err(|_| anyhow!("malformed nonce"))?;
        let pt = self
            .cipher
            .decrypt(&nonce, ct)
            .map_err(|_| anyhow!("decryption failed (wrong key or corrupted token)"))?;
        String::from_utf8(pt).context("decrypted secret is not valid UTF-8")
    }

    fn persist(&self, state: &SecretsFile) -> Result<()> {
        let yaml = serde_yaml::to_string(state)?;
        let path = secrets_file_path();
        crate::config::atomic_write(&path, &yaml)?;
        restrict_permissions(&path);
        Ok(())
    }
}

impl Default for FileKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for FileKeyStore {
    fn set(&self, plugin_id: &str, key: &str, plaintext: &str) -> Result<()> {
        let token = self.encrypt(plaintext)?;
        let mut state = self.state.write().expect("secrets state poisoned");
        state
            .secrets
            .entry(plugin_id.to_owned())
            .or_default()
            .insert(key.to_owned(), token);
        self.persist(&state)
    }

    fn get(&self, plugin_id: &str, key: &str) -> Result<Option<String>> {
        let token = {
            let state = self.state.read().expect("secrets state poisoned");
            state
                .secrets
                .get(plugin_id)
                .and_then(|m| m.get(key))
                .cloned()
        };
        token.map(|t| self.decrypt(&t)).transpose()
    }

    fn delete(&self, plugin_id: &str, key: &str) -> Result<()> {
        let mut state = self.state.write().expect("secrets state poisoned");
        let mut changed = false;
        if let Some(m) = state.secrets.get_mut(plugin_id) {
            changed = m.remove(key).is_some();
            if m.is_empty() {
                state.secrets.remove(plugin_id);
            }
        }
        if changed {
            self.persist(&state)?;
        }
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "encrypted-file"
    }
}

fn secrets_file_path() -> PathBuf {
    crate::config::config_dir().join(SECRETS_FILE)
}

fn key_file_path() -> PathBuf {
    crate::config::config_dir().join(KEY_FILE)
}

fn load_secrets_file() -> Result<SecretsFile> {
    let path = secrets_file_path();
    if !path.exists() {
        return Ok(SecretsFile::default());
    }
    let raw = std::fs::read_to_string(&path)?;
    Ok(serde_yaml::from_str(&raw)?)
}

/// Load the machine-local key, generating and persisting one on first use.
fn load_or_create_key() -> Key {
    let path = key_file_path();
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return Key::from(arr);
        }
        log::warn!(
            "[secrets] {} has unexpected length, regenerating",
            path.display()
        );
    }
    let key = Key::generate();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, key.as_slice()) {
        log::error!("[secrets] failed to persist {}: {e}", path.display());
    }
    restrict_permissions(&path);
    key
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        log::warn!(
            "[secrets] could not restrict permissions on {}: {e}",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {
    // Windows ACLs are left at their inherited default (the config dir is
    // already user-owned); a tighter per-user ACL is tracked as a follow-up.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_tmp_config<R>(f: impl FnOnce() -> R) -> R {
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };
        let result = f();
        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
        result
    }

    #[test]
    fn round_trips_a_secret() {
        with_tmp_config(|| {
            let store = FileKeyStore::new();
            store.set("openrgb", "token", "s3cr3t").unwrap();
            assert_eq!(
                store.get("openrgb", "token").unwrap(),
                Some("s3cr3t".to_string())
            );
        });
    }

    #[test]
    fn missing_key_returns_none_not_error() {
        with_tmp_config(|| {
            let store = FileKeyStore::new();
            assert_eq!(store.get("openrgb", "nope").unwrap(), None);
        });
    }

    #[test]
    fn delete_removes_the_value() {
        with_tmp_config(|| {
            let store = FileKeyStore::new();
            store.set("openrgb", "token", "s3cr3t").unwrap();
            store.delete("openrgb", "token").unwrap();
            assert_eq!(store.get("openrgb", "token").unwrap(), None);
        });
    }

    #[test]
    fn delete_of_absent_key_is_not_an_error() {
        with_tmp_config(|| {
            let store = FileKeyStore::new();
            assert!(store.delete("openrgb", "nope").is_ok());
        });
    }

    #[test]
    fn key_file_is_created_and_reused_across_instances() {
        with_tmp_config(|| {
            let a = FileKeyStore::new();
            a.set("openrgb", "token", "s3cr3t").unwrap();
            // A second store instance must load the same key and decrypt fine.
            let b = FileKeyStore::new();
            assert_eq!(
                b.get("openrgb", "token").unwrap(),
                Some("s3cr3t".to_string())
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn key_file_has_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        with_tmp_config(|| {
            let _store = FileKeyStore::new();
            let meta = std::fs::metadata(key_file_path()).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        });
    }

    #[test]
    fn ciphertext_never_contains_the_plaintext_secret() {
        with_tmp_config(|| {
            let store = FileKeyStore::new();
            store.set("openrgb", "token", "super-secret-value").unwrap();
            let raw = std::fs::read_to_string(secrets_file_path()).unwrap();
            assert!(!raw.contains("super-secret-value"));
        });
    }

    #[test]
    fn decrypting_with_a_different_key_fails_without_panicking() {
        with_tmp_config(|| {
            let a = FileKeyStore::new();
            let token = a.encrypt("s3cr3t").unwrap();
            // Overwrite the key file with a different key, then build a fresh
            // store from it — decrypting `a`'s token must error, not panic.
            std::fs::write(key_file_path(), Key::generate().as_slice()).unwrap();
            let b = FileKeyStore::new();
            assert!(b.decrypt(&token).is_err());
        });
    }
}
