// SPDX-License-Identifier: GPL-3.0-or-later
use super::FocusEvent;
use tokio::sync::mpsc;

pub async fn spawn() -> anyhow::Result<mpsc::Receiver<FocusEvent>> {
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        // Try GNOME Shell extension first — compositor-native, no accessibility required.
        match super::gnome_shell::spawn().await {
            Ok(rx) => {
                log::info!("[FocusWatcher] Using GNOME Shell extension backend");
                return Ok(rx);
            }
            Err(e) => log::debug!("[FocusWatcher] GNOME Shell extension unavailable: {e}"),
        }

        // Try wlroots/KDE compositor (zwlr_foreign_toplevel_manager_v1).
        match super::wayland::spawn().await {
            Ok(rx) => {
                log::info!("[FocusWatcher] Using Wayland (wlr) backend");
                return Ok(rx);
            }
            Err(e) => log::debug!("[FocusWatcher] wlr backend unavailable: {e}"),
        }
    }
    if std::env::var_os("DISPLAY").is_some() {
        match super::x11::spawn().await {
            Ok(rx) => {
                log::info!("[FocusWatcher] Using X11 EWMH backend");
                return Ok(rx);
            }
            Err(e) => log::debug!("[FocusWatcher] X11 backend unavailable: {e}"),
        }
    }
    anyhow::bail!("no supported focus backend (headless, SSH, or no compositor protocol available)")
}
