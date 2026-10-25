use std::time::Duration;

/// Long enough for the spawning frontend to connect, short enough not to outlive a crashed one.
pub const IDLE_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Sign-in autostart: tray-only until the tray "Open" is used.
pub const BACKGROUND_ARG: &str = "--background";
