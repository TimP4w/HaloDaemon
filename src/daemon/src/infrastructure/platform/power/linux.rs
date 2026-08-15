// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "linux")]
//! Resume detection via systemd-logind. `PrepareForSleep(true)` announces an
//! impending suspend; the matching `false` is the resume.

use anyhow::Result;
use futures_util::StreamExt as _;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use zbus::{Connection, MatchRule, MessageStream};

pub async fn resume_events() -> Result<UnboundedReceiver<()>> {
    let system = Connection::system().await?;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.login1")?
        .interface("org.freedesktop.login1.Manager")?
        .member("PrepareForSleep")?
        .build();
    let mut stream = MessageStream::for_match_rule(rule, &system, Some(8)).await?;
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        // The stream borrows nothing, but dropping the connection unsubscribes.
        let _system = system;
        while let Some(message) = stream.next().await {
            let message = match message {
                Ok(message) => message,
                Err(e) => {
                    log::debug!("[power] PrepareForSleep stream error: {e}");
                    break;
                }
            };
            match message.body().deserialize::<bool>() {
                Ok(true) => log::debug!("[power] system is suspending"),
                Ok(false) => {
                    if tx.send(()).is_err() {
                        break;
                    }
                }
                Err(e) => log::debug!("[power] malformed PrepareForSleep: {e}"),
            }
        }
    });

    Ok(rx)
}
