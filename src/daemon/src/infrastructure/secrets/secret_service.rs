// SPDX-License-Identifier: GPL-3.0-or-later
//! Linux OS credential backend: the freedesktop Secret Service D-Bus API
//! (gnome-keyring, KWallet, keepassxc, …), spoken directly over zbus.
//!
//! Item naming matches what the `keyring` crate wrote before this backend
//! replaced it — attributes `service`/`username` and the `keyring:user@service`
//! label — so credentials stored by earlier versions still resolve.

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Type, Value};
use zbus::Connection;

const DEFAULT_COLLECTION: &str = "/org/freedesktop/secrets/aliases/default";
/// The Secret Service returns this path in place of a prompt when none is needed.
const NO_PROMPT: &str = "/";

#[derive(Debug, serde::Serialize, serde::Deserialize, Type)]
pub struct Secret {
    session: OwnedObjectPath,
    parameters: Vec<u8>,
    value: Vec<u8>,
    content_type: String,
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Service",
    default_service = "org.freedesktop.secrets",
    default_path = "/org/freedesktop/secrets"
)]
trait Service {
    fn open_session(
        &self,
        algorithm: &str,
        input: &Value<'_>,
    ) -> zbus::Result<(OwnedValue, OwnedObjectPath)>;

    fn search_items(
        &self,
        attributes: HashMap<&str, &str>,
    ) -> zbus::Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)>;

    fn unlock(
        &self,
        objects: &[ObjectPath<'_>],
    ) -> zbus::Result<(Vec<OwnedObjectPath>, OwnedObjectPath)>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Collection",
    default_service = "org.freedesktop.secrets"
)]
trait Collection {
    fn create_item(
        &self,
        properties: HashMap<&str, &Value<'_>>,
        secret: &Secret,
        replace: bool,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Item",
    default_service = "org.freedesktop.secrets"
)]
trait Item {
    fn get_secret(&self, session: &ObjectPath<'_>) -> zbus::Result<Secret>;
    fn delete(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn label(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn attributes(&self) -> zbus::Result<HashMap<String, String>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Prompt",
    default_service = "org.freedesktop.secrets"
)]
trait Prompt {
    fn prompt(&self, window_id: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    fn completed(&self, dismissed: bool, result: Value<'_>) -> zbus::Result<()>;
}

pub struct SecretService {
    connection: Connection,
    session: OwnedObjectPath,
}

impl SecretService {
    pub async fn connect() -> Result<Self> {
        let connection = Connection::session()
            .await
            .context("connecting to the session bus")?;
        let service = ServiceProxy::new(&connection)
            .await
            .context("opening the Secret Service")?;
        // "plain" is safe here for the same reason the file fallback is: an
        // attacker on this session bus already runs as this user.
        let (_, session) = service
            .open_session("plain", &Value::new(""))
            .await
            .context("opening a Secret Service session")?;
        Ok(Self {
            connection,
            session,
        })
    }

    async fn service(&self) -> Result<ServiceProxy<'_>> {
        ServiceProxy::new(&self.connection)
            .await
            .context("opening the Secret Service")
    }

    /// Drive a returned prompt to completion, if the service asked for one.
    /// Returns `false` when the user dismissed it.
    async fn settle_prompt(&self, path: &OwnedObjectPath) -> Result<bool> {
        if path.as_str() == NO_PROMPT {
            return Ok(true);
        }
        let prompt = PromptProxy::builder(&self.connection)
            .path(path.clone())
            .context("addressing the prompt")?
            .build()
            .await
            .context("opening the prompt")?;
        let mut completed = prompt
            .receive_completed()
            .await
            .context("subscribing to the prompt result")?;
        prompt.prompt("").await.context("showing the prompt")?;
        let Some(signal) = futures_util::StreamExt::next(&mut completed).await else {
            return Err(anyhow!("the prompt closed without a result"));
        };
        Ok(!signal
            .args()
            .context("reading the prompt result")?
            .dismissed)
    }

    async fn find_item(&self, attributes: &Attributes) -> Result<Option<OwnedObjectPath>> {
        let service = self.service().await?;
        let (unlocked, locked) = service
            .search_items(attributes.as_map())
            .await
            .context("searching the Secret Service")?;
        if let Some(path) = unlocked.into_iter().next() {
            return Ok(Some(path));
        }
        let Some(path) = locked.into_iter().next() else {
            return Ok(None);
        };
        let (unlocked, prompt) = service
            .unlock(&[path.as_ref()])
            .await
            .context("unlocking the credential")?;
        if !self.settle_prompt(&prompt).await? {
            return Err(anyhow!("unlocking the credential was dismissed"));
        }
        Ok(unlocked.into_iter().next().or(Some(path)))
    }

    pub async fn set(&self, attributes: &Attributes, plaintext: &str) -> Result<()> {
        let collection = CollectionProxy::builder(&self.connection)
            .path(DEFAULT_COLLECTION)
            .context("addressing the default collection")?
            .build()
            .await
            .context("opening the default collection")?;
        let label = Value::new(attributes.label());
        let map = attributes.as_map();
        let attribute_value = Value::new(map);
        let properties = HashMap::from([
            ("org.freedesktop.Secret.Item.Label", &label),
            ("org.freedesktop.Secret.Item.Attributes", &attribute_value),
        ]);
        let secret = Secret {
            session: self.session.clone(),
            parameters: Vec::new(),
            value: plaintext.as_bytes().to_vec(),
            content_type: "text/plain".to_owned(),
        };
        let (_, prompt) = collection
            .create_item(properties, &secret, true)
            .await
            .context("storing the secret")?;
        if !self.settle_prompt(&prompt).await? {
            return Err(anyhow!("storing the secret was dismissed"));
        }
        Ok(())
    }

    pub async fn get(&self, attributes: &Attributes) -> Result<Option<String>> {
        let Some(path) = self.find_item(attributes).await? else {
            return Ok(None);
        };
        let item = ItemProxy::builder(&self.connection)
            .path(path)
            .context("addressing the credential")?
            .build()
            .await
            .context("opening the credential")?;
        let secret = item
            .get_secret(&self.session.as_ref())
            .await
            .context("reading the secret")?;
        String::from_utf8(secret.value)
            .map(Some)
            .context("the stored secret is not valid UTF-8")
    }

    #[cfg(test)]
    async fn describe(
        &self,
        attributes: &Attributes,
    ) -> Result<Option<(String, HashMap<String, String>)>> {
        let Some(path) = self.find_item(attributes).await? else {
            return Ok(None);
        };
        let item = ItemProxy::builder(&self.connection)
            .path(path)
            .context("addressing the credential")?
            .build()
            .await
            .context("opening the credential")?;
        Ok(Some((item.label().await?, item.attributes().await?)))
    }

    pub async fn delete(&self, attributes: &Attributes) -> Result<()> {
        let Some(path) = self.find_item(attributes).await? else {
            return Ok(());
        };
        let item = ItemProxy::builder(&self.connection)
            .path(path)
            .context("addressing the credential")?
            .build()
            .await
            .context("opening the credential")?;
        let prompt = item.delete().await.context("deleting the secret")?;
        if !self.settle_prompt(&prompt).await? {
            return Err(anyhow!("deleting the secret was dismissed"));
        }
        Ok(())
    }
}

/// The lookup attributes identifying one stored credential.
pub struct Attributes {
    service: String,
    username: String,
}

impl Attributes {
    pub fn new(service: &str, username: &str) -> Self {
        Self {
            service: service.to_owned(),
            username: username.to_owned(),
        }
    }

    fn as_map(&self) -> HashMap<&str, &str> {
        HashMap::from([
            ("service", self.service.as_str()),
            ("username", self.username.as_str()),
        ])
    }

    fn label(&self) -> String {
        format!("keyring:{}@{}", self.username, self.service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips against the live session Secret Service, so it is ignored by
    /// default: CI and headless builds have no D-Bus session (that case is what
    /// the encrypted-file fallback exists for). Run with
    /// `cargo test -p halod secret_service -- --ignored` on a desktop session.
    #[tokio::test]
    #[ignore = "requires a running Secret Service on the session bus"]
    async fn round_trips_a_secret_against_the_live_service() {
        let attributes = Attributes::new("halod", "__halod_selftest__/token");
        let service = SecretService::connect().await.expect("secret service");

        service.delete(&attributes).await.expect("clean slate");
        assert_eq!(service.get(&attributes).await.expect("get"), None);

        service.set(&attributes, "hunter2").await.expect("set");
        assert_eq!(
            service.get(&attributes).await.expect("get"),
            Some("hunter2".to_owned())
        );

        // Items a previous halod stored via the `keyring` crate are found by
        // these attributes; writing anything else would strand them.
        let (label, stored) = service
            .describe(&attributes)
            .await
            .expect("describe")
            .expect("item exists");
        assert_eq!(label, "keyring:__halod_selftest__/token@halod");
        assert_eq!(stored.get("service").map(String::as_str), Some("halod"));
        assert_eq!(
            stored.get("username").map(String::as_str),
            Some("__halod_selftest__/token")
        );

        service.set(&attributes, "hunter3").await.expect("replace");
        assert_eq!(
            service.get(&attributes).await.expect("get"),
            Some("hunter3".to_owned()),
            "a second write must replace, not duplicate, the item"
        );

        service.delete(&attributes).await.expect("delete");
        assert_eq!(service.get(&attributes).await.expect("get"), None);
        service
            .delete(&attributes)
            .await
            .expect("delete is idempotent");
    }

    #[test]
    fn attributes_match_the_schema_the_keyring_crate_wrote() {
        let attributes = Attributes::new("halod", "nanoleaf/token");
        assert_eq!(attributes.label(), "keyring:nanoleaf/token@halod");
        let map = attributes.as_map();
        assert_eq!(map.get("service"), Some(&"halod"));
        assert_eq!(map.get("username"), Some(&"nanoleaf/token"));
        assert_eq!(
            map.len(),
            2,
            "extra attributes would not match stored items"
        );
    }
}
