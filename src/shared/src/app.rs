//! Identity constants shared by the daemon and GUI processes.

/// Config-dir name (`~/.config/halod`, `%APPDATA%\halod`) and other
/// filesystem-facing identifiers.
pub const APP_NAME: &str = "halod";

/// Human-readable product name shown in window titles, notifications, and
/// virtual device names.
pub const APP_DISPLAY_NAME: &str = "HaloDaemon";

/// Unix domain socket filename, under the runtime dir.
pub const SOCKET_FILENAME: &str = "halod.sock";

/// Windows named pipe path.
pub const PIPE_NAME: &str = r"\\.\pipe\halod";

/// Unix domain socket filename for the GUI's single-instance guard, under the
/// runtime dir. A second GUI launch pings it so the running instance surfaces
/// its window instead of opening a duplicate window + tray icon.
pub const GUI_SOCKET_FILENAME: &str = "halod-gui.sock";

/// Windows named pipe for the GUI single-instance guard.
pub const GUI_PIPE_NAME: &str = r"\\.\pipe\halod-gui";

/// Process name the GUI registers itself under (window title bar, `eframe`
/// app id, single-instance checks).
pub const GUI_PROCESS_NAME: &str = "halod-gui";

pub const GNOME_EXTENSION_UUID: &str = "companion@halod.timp4w.dev";

/// Reverse-DNS application id: the GUI's Wayland/X11 window class/app_id and
/// the base name of the desktop entry / D-Bus well-known name it registers
/// (`{APP_ID}.desktop`).
pub const APP_ID: &str = "dev.timp4w.Halod";

/// Main project repository URL (distinct from the plugins repo in
/// `daemon::constants::OFFICIAL_PLUGIN_REPO_URL`).
pub const REPO_URL: &str = "https://github.com/TimP4w/HaloDaemon";

/// Signs plugin catalogs and application releases; indexed by key id so a
/// release can overlap old and rotated keys.
pub const OFFICIAL_PUBLIC_KEYS: &[(&str, &str)] = &[(
    "halodaemon-official-2026",
    "tjbwm5X4f70e+soVNV1AfRyb/TtnEsNNl+93YMO6IhQ=",
)];

pub const INSTALLER_ASSET: &str = "halod-setup-x64.exe";
pub const SUMS_ASSET: &str = "SHA256SUMS";
pub const SUMS_SIG_ASSET: &str = "SHA256SUMS.sig";

/// `HALOD_REPO_URL` points the update check at a fork; releases must still be
/// signed with [`OFFICIAL_PUBLIC_KEYS`].
fn release_repo_url() -> std::borrow::Cow<'static, str> {
    match std::env::var("HALOD_REPO_URL") {
        Ok(url) if !url.is_empty() => url.into(),
        _ => REPO_URL.into(),
    }
}

pub fn latest_release_api_url() -> String {
    let repo = release_repo_url();
    let path = repo.strip_prefix("https://github.com/").unwrap_or(&repo);
    format!("https://api.github.com/repos/{path}/releases/latest")
}

pub fn release_page_url(version: &str) -> String {
    format!("{}/releases/tag/v{version}", release_repo_url())
}

pub fn release_asset_url(version: &str, file: &str) -> String {
    format!("{}/releases/download/v{version}/{file}", release_repo_url())
}

/// Author/developer credit shown in the GUI about/credits.
pub const AUTHOR: &str = "TimP4w";

pub fn config_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("HALOD_CONFIG_DIR") {
        return dir.into();
    }
    #[cfg(target_os = "windows")]
    let base = std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| ".".into()));
    #[cfg(not(target_os = "windows"))]
    let base = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join(".config");
    base.join(APP_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_urls_derive_from_the_repository() {
        assert_eq!(
            latest_release_api_url(),
            "https://api.github.com/repos/TimP4w/HaloDaemon/releases/latest"
        );
        assert_eq!(
            release_page_url("0.14.0"),
            format!("{REPO_URL}/releases/tag/v0.14.0")
        );
        assert_eq!(
            release_asset_url("0.14.0", SUMS_SIG_ASSET),
            format!("{REPO_URL}/releases/download/v0.14.0/SHA256SUMS.sig")
        );
    }
}
