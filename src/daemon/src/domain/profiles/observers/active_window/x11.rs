// SPDX-License-Identifier: GPL-3.0-or-later
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt, Window};
use x11rb::rust_connection::RustConnection;

use super::{normalize_name, FocusEvent};

pub async fn spawn() -> Result<mpsc::Receiver<FocusEvent>> {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (tx, rx) = mpsc::channel(16);
    std::thread::Builder::new()
        .name("halod-x11-focus".into())
        .spawn(move || run(tx, ready_tx))
        .context("spawning X11 focus watcher")?;
    ready_rx
        .await
        .context("X11 focus watcher exited during startup")??;
    Ok(rx)
}

fn run(tx: mpsc::Sender<FocusEvent>, ready: tokio::sync::oneshot::Sender<Result<()>>) {
    let mut ready = Some(ready);
    let result = (|| -> Result<()> {
        let (connection, screen_num) = RustConnection::connect(None).context("connect X11")?;
        let screen = connection
            .setup()
            .roots
            .get(screen_num)
            .context("X11 screen index out of range")?;
        let root = screen.root;
        let active_atom = intern(&connection, b"_NET_ACTIVE_WINDOW")?;
        let wm_class_atom = intern(&connection, b"WM_CLASS")?;
        let _ = ready.take().expect("startup sender available").send(Ok(()));
        let mut last = None;

        loop {
            let current = focused_process(&connection, root, active_atom, wm_class_atom)
                .unwrap_or_else(|error| {
                    log::debug!("[FocusWatcher] X11 focus query failed: {error:#}");
                    None
                });
            if current != last {
                let event = match &current {
                    Some(process_name) => FocusEvent::AppFocused {
                        process_name: process_name.clone(),
                    },
                    None => FocusEvent::NoApp,
                };
                if tx.blocking_send(event).is_err() {
                    return Ok(());
                }
                last = current;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    })();
    if let Err(error) = result {
        if let Some(ready) = ready {
            let _ = ready.send(Err(error));
        }
    }
}

fn intern(connection: &RustConnection, name: &[u8]) -> Result<Atom> {
    Ok(connection
        .intern_atom(false, name)?
        .reply()
        .context("X11 intern_atom reply")?
        .atom)
}

fn focused_process(
    connection: &RustConnection,
    root: Window,
    active_atom: Atom,
    wm_class_atom: Atom,
) -> Result<Option<String>> {
    let active = connection
        .get_property(false, root, active_atom, AtomEnum::WINDOW, 0, 1)?
        .reply()
        .context("read _NET_ACTIVE_WINDOW")?
        .value32()
        .and_then(|mut values| values.next())
        .filter(|window| *window != x11rb::NONE);
    let Some(window) = active else {
        return Ok(None);
    };
    let bytes = connection
        .get_property(false, window, wm_class_atom, AtomEnum::STRING, 0, 1024)?
        .reply()
        .context("read WM_CLASS")?
        .value;
    Ok(parse_wm_class(&bytes).map(|name| normalize_name(&name)))
}

fn parse_wm_class(bytes: &[u8]) -> Option<String> {
    // WM_CLASS is two NUL-separated strings: instance, then class. The class
    // is generally stable across windows, while instance may contain a title.
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .next_back()
        .and_then(|part| std::str::from_utf8(part).ok())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wm_class_prefers_stable_class_and_rejects_malformed_values() {
        assert_eq!(
            parse_wm_class(b"Navigator\0Firefox\0").as_deref(),
            Some("Firefox")
        );
        assert_eq!(parse_wm_class(b"code\0").as_deref(), Some("code"));
        assert_eq!(parse_wm_class(b"\xff\0"), None);
        assert_eq!(parse_wm_class(b"\0"), None);
    }
}
