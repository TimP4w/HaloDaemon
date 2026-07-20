// SPDX-License-Identifier: GPL-3.0-or-later
//! OS credential store backend: Windows Credential Manager (DPAPI) or Linux
//! Secret Service (libsecret over D-Bus), via the `keyring` crate.

use anyhow::{Context, Result};
use std::sync::mpsc;

use super::SecretStore;

const SERVICE: &str = halod_shared::app::APP_NAME;

fn account(plugin_id: &str, key: &str) -> String {
    format!("{plugin_id}/{key}")
}

pub struct KeyringStore {
    tx: mpsc::Sender<Request>,
}

enum Request {
    Probe(mpsc::Sender<Result<()>>),
    Set {
        account: String,
        plaintext: String,
        reply: mpsc::Sender<Result<()>>,
    },
    Get {
        account: String,
        reply: mpsc::Sender<Result<Option<String>>>,
    },
    Delete {
        account: String,
        reply: mpsc::Sender<Result<()>>,
    },
}

fn entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, account).context("opening credential entry")
}

fn worker(rx: mpsc::Receiver<Request>) {
    while let Ok(request) = rx.recv() {
        match request {
            Request::Probe(reply) => {
                let result = (|| {
                    let entry = keyring::Entry::new(SERVICE, "__halod_probe__")
                        .context("opening a probe credential entry")?;
                    entry
                        .set_password("probe")
                        .context("writing a probe credential")?;
                    let _ = entry.delete_credential();
                    Ok(())
                })();
                let _ = reply.send(result);
            }
            Request::Set {
                account,
                plaintext,
                reply,
            } => {
                let result = entry(&account)
                    .and_then(|entry| entry.set_password(&plaintext).context("storing secret"));
                let _ = reply.send(result);
            }
            Request::Get { account, reply } => {
                let result = entry(&account).and_then(|entry| match entry.get_password() {
                    Ok(value) => Ok(Some(value)),
                    Err(keyring::Error::NoEntry) => Ok(None),
                    Err(error) => Err(error).context("reading secret"),
                });
                let _ = reply.send(result);
            }
            Request::Delete { account, reply } => {
                let result = entry(&account).and_then(|entry| match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                    Err(error) => Err(error).context("deleting secret"),
                });
                let _ = reply.send(result);
            }
        }
    }
}

fn receive<T>(rx: mpsc::Receiver<Result<T>>) -> Result<T> {
    rx.recv().context("keyring worker stopped unexpectedly")?
}

impl KeyringStore {
    /// Probe the platform credential store with a throwaway round-trip; `Err`
    /// means the keyring is unreachable (no D-Bus session, headless, …) and the
    /// caller should fall back to the encrypted-file store.
    pub fn probe() -> Result<Self> {
        // keyring's Linux backend is synchronous and creates a private Tokio
        // runtime internally. Keep every call on a plain OS thread so invoking
        // this store from halod's async runtime cannot nest Tokio runtimes.
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("halod-keyring".to_owned())
            .spawn(move || worker(rx))
            .context("starting keyring worker")?;

        let store = Self { tx };
        let (reply, result) = mpsc::channel();
        store
            .tx
            .send(Request::Probe(reply))
            .context("sending keyring probe to worker")?;
        receive(result)?;
        Ok(store)
    }
}

impl SecretStore for KeyringStore {
    fn set(&self, plugin_id: &str, key: &str, plaintext: &str) -> Result<()> {
        let (reply, result) = mpsc::channel();
        self.tx
            .send(Request::Set {
                account: account(plugin_id, key),
                plaintext: plaintext.to_owned(),
                reply,
            })
            .context("sending keyring write to worker")?;
        receive(result)
    }

    fn get(&self, plugin_id: &str, key: &str) -> Result<Option<String>> {
        let (reply, result) = mpsc::channel();
        self.tx
            .send(Request::Get {
                account: account(plugin_id, key),
                reply,
            })
            .context("sending keyring read to worker")?;
        receive(result)
    }

    fn delete(&self, plugin_id: &str, key: &str) -> Result<()> {
        let (reply, result) = mpsc::channel();
        self.tx
            .send(Request::Delete {
                account: account(plugin_id, key),
                reply,
            })
            .context("sending keyring delete to worker")?;
        receive(result)
    }
}
