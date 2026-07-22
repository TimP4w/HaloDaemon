// SPDX-License-Identifier: GPL-3.0-or-later
//! OS credential store backend: Windows Credential Manager (DPAPI) or Linux
//! Secret Service over D-Bus.
//!
//! Every call is serialized onto one dedicated OS thread, which on Linux owns
//! the zbus connection and its own current-thread runtime — so this synchronous
//! [`SecretStore`] can be called from halod's async runtime without nesting.

use std::sync::mpsc;

use anyhow::{Context, Result};

use super::SecretStore;

const SERVICE: &str = halod_shared::app::APP_NAME;

fn account(plugin_id: &str, key: &str) -> String {
    format!("{plugin_id}/{key}")
}

const PROBE_ACCOUNT: &str = "__halod_probe__";

pub struct OsStore {
    tx: mpsc::Sender<Request>,
}

enum Request {
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

#[cfg(target_os = "linux")]
mod backend {
    use super::*;
    use crate::infrastructure::secrets::secret_service::{Attributes, SecretService};

    pub struct Backend {
        runtime: tokio::runtime::Runtime,
        service: SecretService,
    }

    impl Backend {
        pub fn open() -> Result<Self> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("starting the credential-store runtime")?;
            let service = runtime.block_on(SecretService::connect())?;
            Ok(Self { runtime, service })
        }

        pub fn set(&self, account: &str, plaintext: &str) -> Result<()> {
            self.runtime.block_on(
                self.service
                    .set(&Attributes::new(SERVICE, account), plaintext),
            )
        }

        pub fn get(&self, account: &str) -> Result<Option<String>> {
            self.runtime
                .block_on(self.service.get(&Attributes::new(SERVICE, account)))
        }

        pub fn delete(&self, account: &str) -> Result<()> {
            self.runtime
                .block_on(self.service.delete(&Attributes::new(SERVICE, account)))
        }
    }
}

#[cfg(target_os = "windows")]
mod backend {
    use super::*;
    use crate::infrastructure::secrets::windows_credentials as credentials;

    pub struct Backend;

    impl Backend {
        pub fn open() -> Result<Self> {
            Ok(Self)
        }

        pub fn set(&self, account: &str, plaintext: &str) -> Result<()> {
            credentials::set(&credentials::target_name(SERVICE, account), plaintext)
        }

        pub fn get(&self, account: &str) -> Result<Option<String>> {
            credentials::get(&credentials::target_name(SERVICE, account))
        }

        pub fn delete(&self, account: &str) -> Result<()> {
            credentials::delete(&credentials::target_name(SERVICE, account))
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod backend {
    use super::*;

    pub struct Backend;

    impl Backend {
        pub fn open() -> Result<Self> {
            anyhow::bail!("no OS credential store on this platform")
        }

        pub fn set(&self, _account: &str, _plaintext: &str) -> Result<()> {
            unreachable!("the backend never opens")
        }

        pub fn get(&self, _account: &str) -> Result<Option<String>> {
            unreachable!("the backend never opens")
        }

        pub fn delete(&self, _account: &str) -> Result<()> {
            unreachable!("the backend never opens")
        }
    }
}

fn worker(backend: backend::Backend, rx: mpsc::Receiver<Request>) {
    while let Ok(request) = rx.recv() {
        match request {
            Request::Set {
                account,
                plaintext,
                reply,
            } => {
                let _ = reply.send(backend.set(&account, &plaintext).context("storing secret"));
            }
            Request::Get { account, reply } => {
                let _ = reply.send(backend.get(&account).context("reading secret"));
            }
            Request::Delete { account, reply } => {
                let _ = reply.send(backend.delete(&account).context("deleting secret"));
            }
        }
    }
}

fn receive<T>(rx: mpsc::Receiver<Result<T>>) -> Result<T> {
    rx.recv().context("credential-store worker stopped")?
}

impl OsStore {
    /// Probe the platform credential store with a throwaway round-trip; `Err`
    /// means it is unreachable (no D-Bus session, headless, …) and the caller
    /// should fall back to the encrypted-file store.
    pub fn probe() -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("halod-credentials".to_owned())
            .spawn(move || match backend::Backend::open() {
                Ok(backend) => {
                    let _ = ready_tx.send(Ok(()));
                    worker(backend, rx);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .context("starting the credential-store worker")?;
        ready_rx
            .recv()
            .context("the credential-store worker stopped before opening")??;

        let store = Self { tx };
        store
            .set_account(PROBE_ACCOUNT, "probe")
            .context("writing a probe credential")?;
        let _ = store.delete_account(PROBE_ACCOUNT);
        Ok(store)
    }

    fn set_account(&self, account: &str, plaintext: &str) -> Result<()> {
        let (reply, result) = mpsc::channel();
        self.tx
            .send(Request::Set {
                account: account.to_owned(),
                plaintext: plaintext.to_owned(),
                reply,
            })
            .context("sending a credential write to the worker")?;
        receive(result)
    }

    fn delete_account(&self, account: &str) -> Result<()> {
        let (reply, result) = mpsc::channel();
        self.tx
            .send(Request::Delete {
                account: account.to_owned(),
                reply,
            })
            .context("sending a credential delete to the worker")?;
        receive(result)
    }
}

impl SecretStore for OsStore {
    fn set(&self, plugin_id: &str, key: &str, plaintext: &str) -> Result<()> {
        self.set_account(&account(plugin_id, key), plaintext)
    }

    fn get(&self, plugin_id: &str, key: &str) -> Result<Option<String>> {
        let (reply, result) = mpsc::channel();
        self.tx
            .send(Request::Get {
                account: account(plugin_id, key),
                reply,
            })
            .context("sending a credential read to the worker")?;
        receive(result)
    }

    fn delete(&self, plugin_id: &str, key: &str) -> Result<()> {
        self.delete_account(&account(plugin_id, key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The account string is the compatibility contract with credentials the
    /// `keyring` crate stored before this backend replaced it.
    #[test]
    fn accounts_namespace_secrets_by_plugin() {
        assert_eq!(account("nanoleaf", "token"), "nanoleaf/token");
    }
}
