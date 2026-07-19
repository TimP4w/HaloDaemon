// SPDX-License-Identifier: GPL-3.0-or-later
use futures_util::StreamExt as _;
use tokio::sync::mpsc;
use zbus::zvariant::Value;
use zbus::{Connection, MatchRule, MessageStream};

use super::FocusEvent;

/// Detected state of the `halod@halod` GNOME Shell extension, for the settings
/// dependency panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionStatus {
    /// The GNOME Shell Extensions D-Bus API answered but does not know the
    /// extension — it is not installed.
    Missing,
    /// Installed but not enabled (GNOME state != ENABLED).
    Disabled,
    /// Installed and enabled — the focus backend can use it.
    Enabled,
}

/// Best-effort probe of the extension's install/enable state. `None` when this
/// isn't a GNOME session (no session bus, or no `org.gnome.Shell.Extensions`
/// API), so the caller can hide the row entirely off GNOME.
pub async fn extension_status() -> Option<ExtensionStatus> {
    let session = Connection::session().await.ok()?;
    let reply = session
        .call_method(
            Some("org.gnome.Shell"),
            "/org/gnome/Shell",
            Some("org.gnome.Shell.Extensions"),
            "GetExtensionInfo",
            &(crate::constants::GNOME_EXTENSION_UUID,),
        )
        .await
        .ok()?;
    let info: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
        reply.body().deserialize().ok()?;
    if info.is_empty() {
        return Some(ExtensionStatus::Missing);
    }
    let enabled = info
        .get("state")
        .map(|v| match &**v {
            Value::F64(f) => (*f as u32) == 1,
            Value::U32(n) => *n == 1,
            Value::I32(n) => *n == 1,
            _ => false,
        })
        .unwrap_or(false);
    Some(if enabled {
        ExtensionStatus::Enabled
    } else {
        ExtensionStatus::Disabled
    })
}

/// Current unique-name owner of `org.gnome.Shell`, or `None` if unowned.
async fn gnome_shell_owner(session: &Connection) -> Option<String> {
    let reply = session
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetNameOwner",
            &("org.gnome.Shell",),
        )
        .await
        .ok()?;
    reply.body().deserialize::<String>().ok()
}

/// Connects to the HaloDaemon GNOME Shell extension's D-Bus signal for focus
/// tracking. Returns an error if the extension is not installed/enabled.
pub async fn spawn() -> anyhow::Result<mpsc::Receiver<FocusEvent>> {
    let session = Connection::session().await?;

    let reply = session
        .call_method(
            Some("org.gnome.Shell"),
            "/org/gnome/Shell",
            Some("org.gnome.Shell.Extensions"),
            "GetExtensionInfo",
            &(crate::constants::GNOME_EXTENSION_UUID,),
        )
        .await
        .map_err(|e| anyhow::anyhow!("GNOME Shell Extensions API unavailable: {e}"))?;

    let info: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
        reply.body().deserialize()?;

    if info.is_empty() {
        anyhow::bail!("halod extension not installed");
    }

    // GNOME Shell serialises state as float64 ('d'), value 1.0 = ENABLED.
    let enabled = info
        .get("state")
        .map(|v| match &**v {
            Value::F64(f) => (*f as u32) == 1,
            Value::U32(n) => *n == 1,
            Value::I32(n) => *n == 1,
            _ => false,
        })
        .unwrap_or(false);
    if !enabled {
        let state = info
            .get("state")
            .map(|v| format!("{:?}", **v))
            .unwrap_or_default();
        anyhow::bail!("halod extension not enabled (state={state})");
    }

    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(crate::constants::FOCUS_WATCHER_DBUS_INTERFACE)?
        .member("FocusChanged")?
        .build();
    let stream = MessageStream::for_match_rule(rule, &session, Some(32)).await?;

    let (tx, rx) = mpsc::channel::<FocusEvent>(32);

    let consumer = tokio::spawn(async move {
        let session = session;
        let mut stream = stream;
        // Only trust signals from the current owner of org.gnome.Shell.
        let mut owner = gnome_shell_owner(&session).await;

        while let Some(msg) = stream.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    log::debug!("[FocusWatcher/GnomeShell] stream error: {e}");
                    break;
                }
            };
            let sender = msg.header().sender().map(|s| s.to_string());
            if sender != owner {
                owner = gnome_shell_owner(&session).await;
                if sender != owner {
                    log::debug!("[FocusWatcher/GnomeShell] dropping signal from unexpected sender {sender:?}");
                    continue;
                }
            }
            match msg.body().deserialize::<(String,)>() {
                Ok((name,)) if !name.is_empty() => {
                    let normalized = super::normalize_name(&name);
                    if tx
                        .send(FocusEvent::AppFocused {
                            process_name: normalized,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok((name,)) if name.is_empty() => {
                    // GNOME Shell extension signals focus loss with an empty name.
                    if tx.send(FocusEvent::NoApp).await.is_err() {
                        break;
                    }
                }
                Ok(_) => {} // Catch-all for any other Ok variant.
                Err(e) => {
                    log::debug!("[FocusWatcher/GnomeShell] deserialize error: {e}");
                }
            }
        }
        log::debug!("[FocusWatcher/GnomeShell] event stream ended");
    });
    tokio::spawn(async move {
        if let Err(e) = consumer.await {
            log::warn!("[FocusWatcher/GnomeShell] consumer task panicked: {e}");
        }
    });

    Ok(rx)
}
