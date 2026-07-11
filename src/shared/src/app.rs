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

/// Process name the GUI registers itself under (window title bar, `eframe`
/// app id, single-instance checks).
pub const GUI_PROCESS_NAME: &str = "halod-gui";

/// Reverse-DNS application id: the GUI's Wayland/X11 window class/app_id and
/// the base name of the desktop entry / D-Bus well-known name it registers
/// (`{APP_ID}.desktop`).
pub const APP_ID: &str = "dev.timp4w.Halod";
