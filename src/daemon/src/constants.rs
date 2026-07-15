// SPDX-License-Identifier: GPL-3.0-or-later
//! Daemon-only string constants that aren't part of the wire-facing
//! `halod_shared::app` identity (see that module for cross-process constants).

/// UUID of the `halod@halod` GNOME Shell extension used for focus tracking.
#[cfg(target_os = "linux")]
pub const GNOME_EXTENSION_UUID: &str = "halod@halod";

/// D-Bus interface the `halod@halod` GNOME Shell extension emits its
/// `FocusChanged` signal on.
#[cfg(target_os = "linux")]
pub const FOCUS_WATCHER_DBUS_INTERFACE: &str = "dev.timp4w.halod.FocusWatcher1";

/// Prefix every halod-managed virtual PulseAudio/PipeWire sink is named with.
#[cfg(target_os = "linux")]
pub const AUDIO_SINK_PREFIX: &str = "halod_";

/// Display name of the virtual keyboard device created for key-remap actions.
#[cfg(target_os = "linux")]
pub const VIRTUAL_KEYBOARD_NAME: &str = "HaloDaemon Virtual Keyboard";

/// Display name of the virtual pointer device created for key-remap actions.
#[cfg(target_os = "linux")]
pub const VIRTUAL_POINTER_NAME: &str = "HaloDaemon Virtual Pointer";

/// Client name the canvas engine's screen-capture session registers under.
#[cfg(target_os = "linux")]
pub const SCREEN_CAPTURE_CLIENT: &str = "halod-screen";

/// The official plugin repository's URL, seeded into config and cloned at
/// startup (see `registry::ensure_official_repo`). Not bundled: nothing here
/// is embedded in the daemon binary, so this is a network fetch, not a build input.
pub const OFFICIAL_PLUGIN_REPO_URL: &str = "https://github.com/TimP4w/HaloDaemon-plugins";

/// Fixed slug for the official plugin repo record — never derived from the
/// URL, so a future URL change can't orphan the non-removable guard in
/// `registry::usecases::repos::remove_repo`.
pub const OFFICIAL_PLUGIN_REPO_SLUG: &str = "official";

/// Trusted release keys for the official plugin repository. Keys are indexed
/// by stable id so a daemon release can overlap old and rotated signing keys.
pub const OFFICIAL_PLUGIN_REPO_PUBLIC_KEYS: &[(&str, &str)] = &[(
    "halodaemon-official-2026",
    "tjbwm5X4f70e+soVNV1AfRyb/TtnEsNNl+93YMO6IhQ=",
)];
