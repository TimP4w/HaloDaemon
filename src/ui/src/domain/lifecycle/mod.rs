// SPDX-License-Identifier: GPL-3.0-or-later
//! The GUI's half of the daemon/tray lifecycle. The daemon shuts itself down
//! once no frontend is connected (`daemon/src/lifecycle.rs`), so this side
//! only needs to bring it back up whenever the tray needs it.

pub mod autostart;

use std::path::{Path, PathBuf};

use halod_shared::lifecycle::BACKGROUND_ARG;

/// What a window-close request should do, decided the same way for both backends.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CloseAction {
    Stay,
    HideToTray,
    Quit,
}

/// The tray "Quit" sets `force_quit`, so it always quits; otherwise the ×
/// button / WM close honours the `close_to_tray` config.
pub fn classify_close(close_requested: bool, force_quit: bool, close_to_tray: bool) -> CloseAction {
    if !close_requested {
        CloseAction::Stay
    } else if force_quit || !close_to_tray {
        CloseAction::Quit
    } else {
        CloseAction::HideToTray
    }
}

pub fn start_in_background<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|a| a == BACKGROUND_ARG)
}

fn sibling_path(current_exe: Option<&Path>, name: &str) -> Option<PathBuf> {
    let path = current_exe?.parent()?.join(name);
    path.exists().then_some(path)
}

/// Ordered `(program, args)` attempts to bring the daemon up; first success wins.
///
/// The daemon is a plain user process (not a Windows service since the privilege
/// split — the elevated register-bus broker is a separate on-demand service that
/// the daemon itself starts). So the GUI just spawns `halod.exe`: the sibling
/// next to the GUI, then `halod.exe` on `PATH`.
#[cfg(windows)]
fn daemon_start_attempts(current_exe: Option<&Path>) -> Vec<(String, Vec<String>)> {
    let exe = format!("{}.exe", halod_shared::app::APP_NAME);
    let mut attempts = Vec::new();
    if let Some(sibling) = sibling_path(current_exe, &exe) {
        attempts.push((sibling.to_string_lossy().into_owned(), vec![]));
    }
    attempts.push((exe, vec![]));
    attempts
}

#[cfg(not(windows))]
fn daemon_start_attempts(current_exe: Option<&Path>) -> Vec<(String, Vec<String>)> {
    let mut attempts = Vec::new();
    if let Some(sibling) = sibling_path(current_exe, halod_shared::app::APP_NAME) {
        attempts.push((sibling.to_string_lossy().into_owned(), vec![]));
    }
    attempts.push((halod_shared::app::APP_NAME.to_string(), vec![]));
    attempts
}

#[cfg(windows)]
fn spawn_attempt(prog: &str, args: &[String]) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(prog)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}

#[cfg(not(windows))]
fn spawn_attempt(prog: &str, args: &[String]) -> std::io::Result<()> {
    std::process::Command::new(prog)
        .args(args)
        .spawn()
        .map(|_| ())
}

/// The GUI only auto-starts a daemon in release builds. In a debug build the
/// developer runs their own daemon (with logs) and the GUI just connects, so a
/// dev session never relaunches the installed service underneath itself.
fn should_autostart(is_debug: bool) -> bool {
    !is_debug
}

/// Tries each attempt in order, stopping at the first successful spawn.
pub fn ensure_daemon_up() {
    if !should_autostart(cfg!(debug_assertions)) {
        log::debug!("debug build: not auto-starting a daemon (connecting to an existing one only)");
        return;
    }
    let current_exe = std::env::current_exe().ok();
    for (prog, args) in daemon_start_attempts(current_exe.as_deref()) {
        match spawn_attempt(&prog, &args) {
            Ok(()) => return,
            Err(e) => log::debug!("daemon start attempt '{prog}' failed: {e}"),
        }
    }
    log::warn!("failed to start the halod daemon (all attempts exhausted)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_close_request_stays() {
        assert_eq!(classify_close(false, true, false), CloseAction::Stay);
        assert_eq!(classify_close(false, false, true), CloseAction::Stay);
    }

    #[test]
    fn force_quit_always_quits() {
        assert_eq!(classify_close(true, true, true), CloseAction::Quit);
        assert_eq!(classify_close(true, true, false), CloseAction::Quit);
    }

    #[test]
    fn close_to_tray_hides_otherwise_quits() {
        assert_eq!(classify_close(true, false, true), CloseAction::HideToTray);
        assert_eq!(classify_close(true, false, false), CloseAction::Quit);
    }

    #[test]
    fn autostarts_only_in_release() {
        // Debug build → connect-only (the dev runs their own daemon).
        assert!(!should_autostart(true));
        // Release build → the GUI brings a daemon up.
        assert!(should_autostart(false));
    }

    #[test]
    fn start_in_background_detects_the_flag() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(start_in_background(args(&["halod-gui", "--background"])));
        assert!(!start_in_background(args(&["halod-gui"])));
        assert!(!start_in_background(args(&["halod-gui", "--other"])));
    }

    #[test]
    fn sibling_path_requires_existence() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("halod-gui");
        std::fs::write(&exe, b"").unwrap();
        assert_eq!(sibling_path(Some(&exe), "missing-sibling"), None);

        let sibling = dir.path().join("halod");
        std::fs::write(&sibling, b"").unwrap();
        assert_eq!(sibling_path(Some(&exe), "halod"), Some(sibling));
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_attempts_prefer_sibling_then_fall_back_to_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("halod-gui");
        std::fs::write(&exe, b"").unwrap();
        let sibling = dir.path().join("halod");
        std::fs::write(&sibling, b"").unwrap();

        let attempts = daemon_start_attempts(Some(&exe));
        assert_eq!(attempts[0].0, sibling.to_string_lossy());
        assert_eq!(attempts[1].0, "halod");
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_attempts_fall_back_to_path_only_without_a_sibling() {
        let attempts = daemon_start_attempts(None);
        assert_eq!(attempts, vec![("halod".to_string(), vec![])]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_attempts_prefer_sibling_then_fall_back_to_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("halod-gui.exe");
        std::fs::write(&exe, b"").unwrap();
        let sibling = dir.path().join("halod.exe");
        std::fs::write(&sibling, b"").unwrap();

        let attempts = daemon_start_attempts(Some(&exe));
        assert_eq!(attempts[0].0, sibling.to_string_lossy());
        assert_eq!(attempts[1].0, "halod.exe");
    }

    #[cfg(windows)]
    #[test]
    fn windows_attempts_fall_back_to_path_only_without_a_sibling() {
        let attempts = daemon_start_attempts(None);
        assert_eq!(attempts, vec![("halod.exe".to_string(), vec![])]);
    }
}
