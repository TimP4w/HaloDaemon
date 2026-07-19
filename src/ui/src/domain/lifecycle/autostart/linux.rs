// SPDX-License-Identifier: GPL-3.0-or-later
//! Linux autostart via an XDG `~/.config/autostart/*.desktop` entry.

use std::io;
use std::path::{Path, PathBuf};

use halod_shared::app::{APP_ID, APP_NAME};
use halod_shared::lifecycle::BACKGROUND_ARG;

/// Autostart entry file name.
fn entry_file() -> String {
    format!("{APP_ID}.desktop")
}

/// Every basename a system-wide autostart entry for this app could be
/// installed under — the reverse-DNS name we ship, plus the plain
/// `{APP_NAME}.desktop` some packaging conventions use instead.
fn system_entry_files() -> [String; 2] {
    [format!("{APP_NAME}.desktop"), entry_file()]
}

/// The `~/.config/autostart` directory, honoring `$XDG_CONFIG_HOME` then `$HOME`
/// — parallels `config_dir()` in the daemon.
fn autostart_dir(config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(cfg) = config_home.filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(cfg).join("autostart"));
    }
    home.filter(|s| !s.is_empty())
        .map(|h| PathBuf::from(h).join(".config").join("autostart"))
}

fn entry_path() -> Option<PathBuf> {
    autostart_dir(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
    .map(|d| d.join(entry_file()))
}

/// The `.desktop` autostart entry that launches `exec` in the background.
fn desktop_entry(exec: &str) -> String {
    use halod_shared::app::APP_DISPLAY_NAME;
    format!(
        "[Desktop Entry]\n\
         Name={APP_DISPLAY_NAME}\n\
         Comment=Peripheral control - fan curves, RGB, LCD, audio and more\n\
         Exec={exec} {BACKGROUND_ARG}\n\
         Icon={APP_NAME}\n\
         StartupNotify=true\n\
         Terminal=false\n\
         Type=Application\n\
         Categories=Utility;HardwareSettings;\n\
         X-GNOME-Autostart-enabled=true\n\
         X-GNOME-UsesNotifications=true\n"
    )
}

fn write_entry(path: &Path, exec: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, desktop_entry(exec))
}

fn remove_entry(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn is_enabled() -> bool {
    entry_path().map(|p| p.exists()).unwrap_or(false)
}

/// Candidate system autostart entry paths from `$XDG_CONFIG_DIRS` (default
/// `/etc/xdg` per the XDG base-dir spec).
fn system_entry_paths(config_dirs: Option<&str>) -> Vec<PathBuf> {
    let names = system_entry_files();
    config_dirs
        .filter(|s| !s.is_empty())
        .unwrap_or("/etc/xdg")
        .split(':')
        .filter(|s| !s.is_empty())
        .flat_map(|dir| {
            names
                .iter()
                .map(|name| PathBuf::from(dir).join("autostart").join(name))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Whether an OS integration installs a system-wide autostart entry that we can't
/// remove — in which case the per-user toggle can't govern launch-at-login.
pub fn system_managed() -> bool {
    system_entry_paths(std::env::var("XDG_CONFIG_DIRS").ok().as_deref())
        .iter()
        .any(|p| p.exists())
}

pub fn set_enabled(enable: bool) -> io::Result<()> {
    let path = entry_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "neither $XDG_CONFIG_HOME nor $HOME is set",
        )
    })?;
    if enable {
        let exe = std::env::current_exe()?;
        write_entry(&path, &exe.to_string_lossy())
    } else {
        remove_entry(&path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_launches_exec_in_background() {
        let entry = desktop_entry("/opt/halod/halod-gui");
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Exec=/opt/halod/halod-gui --background\n"));
        assert!(entry.contains("Type=Application\n"));
        assert!(entry.contains("X-GNOME-UsesNotifications=true\n"));
        // GNOME's Notifications panel classifies entries in the exact
        // `Settings` category as system services and deliberately hides them.
        assert!(!entry.contains(";Settings;"));
    }

    #[test]
    fn autostart_dir_prefers_xdg_config_home() {
        assert_eq!(
            autostart_dir(Some("/x/cfg"), Some("/home/u")),
            Some(PathBuf::from("/x/cfg/autostart"))
        );
    }

    #[test]
    fn autostart_dir_falls_back_to_home() {
        assert_eq!(
            autostart_dir(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/autostart"))
        );
        assert_eq!(
            autostart_dir(Some(""), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/autostart"))
        );
    }

    #[test]
    fn autostart_dir_none_without_env() {
        assert_eq!(autostart_dir(None, None), None);
        assert_eq!(autostart_dir(Some(""), Some("")), None);
    }

    #[test]
    fn system_entry_paths_default_to_etc_xdg() {
        let paths = system_entry_paths(None);
        assert!(paths.contains(&PathBuf::from("/etc/xdg/autostart/halod.desktop")));
        assert!(paths.contains(&PathBuf::from("/etc/xdg/autostart").join(entry_file())));
    }

    #[test]
    fn system_entry_paths_span_all_config_dirs() {
        let paths = system_entry_paths(Some("/a:/b"));
        assert!(paths.contains(&PathBuf::from("/a/autostart/halod.desktop")));
        assert!(paths.contains(&PathBuf::from("/b/autostart/halod.desktop")));
    }

    #[test]
    fn system_entry_paths_ignore_empty_segments() {
        assert_eq!(system_entry_paths(Some("")), system_entry_paths(None));
        let paths = system_entry_paths(Some("/a::"));
        assert!(paths.iter().all(|p| p.starts_with("/a")));
    }

    #[test]
    fn write_then_remove_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(entry_file());

        assert!(!path.exists());
        write_entry(&path, "/bin/halod-gui").unwrap();
        assert!(path.exists());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("Exec=/bin/halod-gui --background\n"));

        remove_entry(&path).unwrap();
        assert!(!path.exists());
        remove_entry(&path).unwrap();
    }
}
