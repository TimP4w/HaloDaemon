// SPDX-License-Identifier: GPL-3.0-or-later
//! Daemon-only string constants that aren't part of the wire-facing
//! `halod_shared::app` identity (see that module for cross-process constants).

/// UUID of the `halod@halod` GNOME Shell extension used for focus tracking.
pub const GNOME_EXTENSION_UUID: &str = "halod@halod";

/// D-Bus interface the `halod@halod` GNOME Shell extension emits its
/// `FocusChanged` signal on.
pub const FOCUS_WATCHER_DBUS_INTERFACE: &str = "dev.timp4w.halod.FocusWatcher1";

/// Prefix every halod-managed virtual PulseAudio/PipeWire sink is named with.
pub const AUDIO_SINK_PREFIX: &str = "halod_";

/// Display name of the virtual keyboard device created for key-remap actions.
pub const VIRTUAL_KEYBOARD_NAME: &str = "HaloDaemon Virtual Keyboard";

/// Display name of the virtual pointer device created for key-remap actions.
pub const VIRTUAL_POINTER_NAME: &str = "HaloDaemon Virtual Pointer";

/// Client name the canvas engine's screen-capture session registers under.
pub const SCREEN_CAPTURE_CLIENT: &str = "halod-screen";

/// Filename of the Windows service supervisor's log file.
pub const SERVICE_LOG_FILENAME: &str = "halod-service.log";
