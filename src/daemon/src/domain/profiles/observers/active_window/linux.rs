// SPDX-License-Identifier: GPL-3.0-or-later
use super::FocusEvent;
use tokio::sync::mpsc;

fn is_gnome_session() -> bool {
    ["XDG_CURRENT_DESKTOP", "DESKTOP_SESSION"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .flat_map(|value| {
            value
                .split([':', ';'])
                .map(str::trim)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .any(|desktop| {
            desktop.eq_ignore_ascii_case("gnome")
                || desktop.eq_ignore_ascii_case("gnome-classic")
                || desktop.eq_ignore_ascii_case("gnome-flashback")
                || desktop.eq_ignore_ascii_case("ubuntu")
        })
}

pub async fn spawn() -> anyhow::Result<mpsc::Receiver<FocusEvent>> {
    // GNOME exposes focused-window information through the HaloDaemon Shell
    // extension on both Wayland and X11. Do not silently fall back to a less
    // accurate compositor/window-system backend when the extension is missing.
    if is_gnome_session() {
        return super::gnome_shell::spawn().await;
    }

    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        return super::wayland::spawn().await;
    }
    anyhow::bail!("no supported Wayland focus backend available")
}
