// SPDX-License-Identifier: GPL-3.0-or-later
//! OS "launch at login" integration for the GUI.
//!
//! Registers/unregisters `halod-gui --background` with the platform's autostart
//! mechanism, always from the logged-in user's session (never the elevated
//! daemon): an XDG `~/.config/autostart/*.desktop` file on Linux, an
//! `HKCU\…\Run` registry value on Windows. The source of truth is the OS
//! registration itself, so [`is_enabled`] reads live state rather than a cached
//! config flag.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// Whether HaloDaemon is currently registered to launch at login.
pub fn is_enabled() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::is_enabled()
    }
    #[cfg(windows)]
    {
        windows::is_enabled()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        false
    }
}

/// Whether an OS integration governs launch-at-login declaratively (e.g. the
/// NixOS module), so the in-app toggle is inert and should be shown read-only.
pub fn system_managed() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::system_managed()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Register (`enable = true`) or remove (`false`) the launch-at-login entry.
pub fn set_enabled(enable: bool) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::set_enabled(enable)
    }
    #[cfg(windows)]
    {
        windows::set_enabled(enable)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = enable;
        Ok(())
    }
}

/// Update an existing platform registration to the currently running binary.
#[cfg(windows)]
pub fn repair_enabled_command() {
    windows::repair_enabled_command();
}
