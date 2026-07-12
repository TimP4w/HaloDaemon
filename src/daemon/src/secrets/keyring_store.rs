// SPDX-License-Identifier: GPL-3.0-or-later
//! OS credential store backend: Windows Credential Manager (DPAPI) or Linux
//! Secret Service (libsecret over D-Bus), via the `keyring` crate.

use anyhow::{Context, Result};

use super::SecretStore;

const SERVICE: &str = halod_shared::app::APP_NAME;

fn account(plugin_id: &str, key: &str) -> String {
    format!("{plugin_id}/{key}")
}

pub struct KeyringStore;

impl KeyringStore {
    /// Probe the platform credential store with a throwaway round-trip; `Err`
    /// means the keyring is unreachable (no D-Bus session, headless, …) and the
    /// caller should fall back to the encrypted-file store.
    pub fn probe() -> Result<Self> {
        let entry = keyring::Entry::new(SERVICE, "__halod_probe__")
            .context("opening a probe credential entry")?;
        // Setting and immediately deleting confirms the store actually accepts
        // writes (some platforms report a store as present but reject them).
        entry
            .set_password("probe")
            .context("writing a probe credential")?;
        let _ = entry.delete_credential();
        Ok(Self)
    }
}

impl SecretStore for KeyringStore {
    fn set(&self, plugin_id: &str, key: &str, plaintext: &str) -> Result<()> {
        let entry = keyring::Entry::new(SERVICE, &account(plugin_id, key))
            .context("opening credential entry")?;
        entry.set_password(plaintext).context("storing secret")
    }

    fn get(&self, plugin_id: &str, key: &str) -> Result<Option<String>> {
        let entry = keyring::Entry::new(SERVICE, &account(plugin_id, key))
            .context("opening credential entry")?;
        match entry.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).context("reading secret"),
        }
    }

    fn delete(&self, plugin_id: &str, key: &str) -> Result<()> {
        let entry = keyring::Entry::new(SERVICE, &account(plugin_id, key))
            .context("opening credential entry")?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e).context("deleting secret"),
        }
    }

    fn backend_name(&self) -> &'static str {
        "keyring"
    }
}
