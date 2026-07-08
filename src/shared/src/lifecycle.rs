//! Constants both the daemon and the GUI must agree on for the boot/idle
//! lifecycle: how long the daemon waits for a frontend before shutting itself
//! down, and the CLI flags that start each process in a non-default mode.

use std::time::Duration;

/// How long the daemon tolerates having zero connected IPC clients before it
/// shuts itself down. Long enough for the frontend that just started it to
/// finish connecting; short enough that a killed/crashed frontend doesn't
/// leave the daemon running unattended for long.
pub const IDLE_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// GUI argv flag: start tray-only, no window, until the tray "Open" is used.
/// Used for sign-in autostart.
pub const BACKGROUND_ARG: &str = "--background";

/// Daemon argv flag: opt out of idle-shutdown, so the daemon stays up with no
/// connected frontend. For a future headless TUI/CLI-only deployment.
pub const HEADLESS_ARG: &str = "--headless";
