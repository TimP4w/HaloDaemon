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

/// Reverse-DNS application id: the GUI's Wayland/X11 window class/app_id and
/// the base name of the desktop entry / D-Bus well-known name it registers
/// (`{APP_ID}.desktop`).
pub const APP_ID: &str = "dev.timp4w.Halod";

/// Main project repository URL (distinct from the plugins repo in
/// `daemon::constants::OFFICIAL_PLUGIN_REPO_URL`).
pub const REPO_URL: &str = "https://github.com/TimP4w/HaloDaemon";

/// Author/developer credit shown in the GUI about/credits.
pub const AUTHOR: &str = "TimP4w";
