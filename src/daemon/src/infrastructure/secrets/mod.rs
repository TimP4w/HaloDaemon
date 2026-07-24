// SPDX-License-Identifier: GPL-3.0-or-later
//! Storage for plugin-declared secret config values (`ConfigFieldDef::secure`).
//!
//! Two backends implement [`SecretStore`]:
//! - [`keyring_store::KeyringStore`] — the OS credential store (Windows Credential
//!   Manager / Linux Secret Service), used when reachable.
//! - [`file_store::FileKeyStore`] — an XChaCha20-Poly1305-encrypted fallback for
//!   headless/no-D-Bus environments, keyed by a machine-local file.
//!
//! [`open_secret_store`] probes the keyring once and picks whichever backend is
//! live; callers never know which one is in play. Neither backend is a vault
//! against an attacker who already runs as this user — see `docs/plugins.md`
//! for the threat model.

mod file_store;
mod keyring_store;

pub use file_store::FileKeyStore;

use std::sync::Arc;

use anyhow::Result;

/// Per-plugin secret storage. Keys are namespaced by `(plugin_id, field_key)` so
/// one plugin's secret can never collide with, or be read as, another's.
pub trait SecretStore: Send + Sync {
    fn set(&self, plugin_id: &str, key: &str, plaintext: &str) -> Result<()>;
    /// `Ok(None)` when nothing is stored for this key (not an error).
    fn get(&self, plugin_id: &str, key: &str) -> Result<Option<String>>;
    /// Deleting an absent entry is not an error.
    fn delete(&self, plugin_id: &str, key: &str) -> Result<()>;
}

/// Pick the OS keyring if reachable, else the encrypted-file fallback. Probes
/// the keyring once with a throwaway round-trip; a probe failure (no D-Bus
/// Secret Service, headless session, …) falls back rather than erroring, since
/// secrets must keep working on a server install.
pub fn open_secret_store() -> Arc<dyn SecretStore> {
    match keyring_store::KeyringStore::probe() {
        Ok(store) => {
            log::info!("[secrets] using the OS keyring for secure plugin config");
            Arc::new(store)
        }
        Err(e) => {
            log::info!(
                "[secrets] OS keyring unavailable ({e:#}); falling back to the encrypted file store"
            );
            Arc::new(file_store::FileKeyStore::new())
        }
    }
}
