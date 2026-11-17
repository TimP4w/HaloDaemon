// SPDX-License-Identifier: GPL-3.0-or-later
//! Daemon-only string constants that aren't part of the wire-facing
//! `halod_shared::app` identity (see that module for cross-process constants).

/// UUID of the `halod@halod` GNOME Shell extension used for focus tracking.
#[cfg(target_os = "linux")]
pub const GNOME_EXTENSION_UUID: &str = "companion@halod.timp4w.dev";

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

pub const OFFICIAL_PLUGIN_CATALOG_URL: &str =
    "https://timp4w.github.io/HaloDaemon-plugins/catalog.yaml";

/// Fixed so a URL change can never orphan the non-removable official record.
pub const OFFICIAL_PLUGIN_REPO_SLUG: &str = "official";

/// Indexed by key id so a daemon release can overlap old and rotated keys.
pub const OFFICIAL_PLUGIN_REPO_PUBLIC_KEYS: &[(&str, &str)] = &[(
    "halodaemon-official-2026",
    "tjbwm5X4f70e+soVNV1AfRyb/TtnEsNNl+93YMO6IhQ=",
)];

#[cfg(test)]
mod tests {
    use super::OFFICIAL_PLUGIN_REPO_PUBLIC_KEYS;
    use halod_plugin_signing::catalog::verifying_key;

    #[test]
    fn official_pinned_keys_are_valid_ed25519_keys() {
        assert!(!OFFICIAL_PLUGIN_REPO_PUBLIC_KEYS.is_empty());
        for (id, key) in OFFICIAL_PLUGIN_REPO_PUBLIC_KEYS {
            assert!(halod_shared::ids::is_identifier(id), "{id}");
            verifying_key(key).unwrap();
        }
    }
}
