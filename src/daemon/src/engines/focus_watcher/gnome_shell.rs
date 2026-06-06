use futures_util::StreamExt as _;
use tokio::sync::mpsc;
use zbus::zvariant::Value;
use zbus::{Connection, MatchRule, MessageStream};

use super::FocusEvent;

/// Connects to the HaloDaemon GNOME Shell extension's D-Bus signal for focus
/// tracking. Returns an error if the extension is not installed/enabled.
pub async fn spawn() -> anyhow::Result<mpsc::Receiver<FocusEvent>> {
    let session = Connection::session().await?;

    // Ask GNOME Shell whether our extension is enabled (state == 1).
    let reply = session
        .call_method(
            Some("org.gnome.Shell"),
            "/org/gnome/Shell",
            Some("org.gnome.Shell.Extensions"),
            "GetExtensionInfo",
            &("halod@halod",),
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

    // Subscribe to FocusChanged signals emitted by the extension.
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("dev.timp4w.halod.FocusWatcher1")?
        .member("FocusChanged")?
        .build();
    let stream = MessageStream::for_match_rule(rule, &session, Some(32)).await?;

    let (tx, rx) = mpsc::channel::<FocusEvent>(32);

    tokio::spawn(async move {
        let _session = session;
        let mut stream = stream;

        while let Some(msg) = stream.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    log::debug!("[FocusWatcher/GnomeShell] stream error: {e}");
                    break;
                }
            };
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
                _ => {}
            }
        }
        log::debug!("[FocusWatcher/GnomeShell] event stream ended");
    });

    Ok(rx)
}
