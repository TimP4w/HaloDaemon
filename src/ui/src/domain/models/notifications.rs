// SPDX-License-Identifier: GPL-3.0-or-later
//! Translated copy for daemon-pushed notification codes.

use halod_shared::ids::{DeviceId, PluginId};
use halod_shared::types::NotificationCode;

/// The translated `(title, message)` for a notification code in the active
/// locale. Structured params (device ids, error detail, …) are interpolated
/// verbatim — only the surrounding copy is translated. The exhaustive match
/// makes the compiler flag any new code that lacks copy.
pub fn notification_text(code: &NotificationCode) -> (String, String) {
    use NotificationCode::*;
    match code {
        EngineStopped { detail } => (
            t!("notify.engine_stopped.title").to_string(),
            t!("notify.engine_stopped.message", detail = detail).to_string(),
        ),
        KeyRemapUnavailable { detail } => (
            t!("notify.key_remap_unavailable.title").to_string(),
            t!("notify.key_remap_unavailable.message", detail = detail).to_string(),
        ),
        ProfileSwitched { profile } => (
            t!("notify.profile_switched.title").to_string(),
            t!("notify.profile_switched.message", profile = profile).to_string(),
        ),
        LowBattery {
            device,
            battery,
            level,
            threshold,
            ..
        } => (
            t!("notify.low_battery.title", device = device).to_string(),
            t!(
                "notify.low_battery.message",
                battery = battery,
                level = level,
                threshold = threshold
            )
            .to_string(),
        ),
        DeviceInitFailed { device, detail, .. } => (
            t!("notify.device_init_failed.title", device = device).to_string(),
            t!("notify.device_init_failed.message", detail = detail).to_string(),
        ),
        DeviceWriteFailed { device, .. } => (
            t!("notify.device_write_failed.title", device = device).to_string(),
            t!("notify.device_write_failed.message").to_string(),
        ),
        FanStalled { fan } => (
            t!("notify.fan_stalled.title", fan = fan).to_string(),
            t!("notify.fan_stalled.message").to_string(),
        ),
        PluginRuntimeError { plugin, detail: _ } => (
            t!("notify.plugin_runtime_error.title", plugin = plugin).to_string(),
            t!("notify.plugin_runtime_error.message").to_string(),
        ),
        PluginConnectFailed { plugin, detail: _ } => (
            t!("notify.plugin_connect_failed.title", plugin = plugin).to_string(),
            t!("notify.plugin_connect_failed.message").to_string(),
        ),
        PluginRecommended { name, .. } => (
            t!("notify.plugin_recommended.title").to_string(),
            t!("notify.plugin_recommended.message", plugin = name).to_string(),
        ),
        PluginUpdateNeedsPermissions { name, .. } => (
            t!(
                "notify.plugin_update_needs_permissions.title",
                plugin = name
            )
            .to_string(),
            t!("notify.plugin_update_needs_permissions.message").to_string(),
        ),
        Generic { message } => (t!("notify.error_title").to_string(), message.clone()),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum NotificationTarget {
    Page(crate::domain::state::Page),
    Plugin(PluginId),
}

pub fn notification_target(
    code: &NotificationCode,
    devices: &[halod_shared::types::WireDevice],
) -> Option<NotificationTarget> {
    use crate::domain::state::Page;
    use NotificationCode::*;
    let known_device = |id: &DeviceId| {
        devices
            .iter()
            .any(|device| device.id == *id)
            .then(|| NotificationTarget::Page(Page::Device(id.clone())))
    };
    match code {
        LowBattery { device_id, .. }
        | DeviceInitFailed { device_id, .. }
        | DeviceWriteFailed { device_id, .. } => known_device(device_id),
        FanStalled { .. } => Some(NotificationTarget::Page(Page::Cooling)),
        ProfileSwitched { profile } => {
            Some(NotificationTarget::Page(Page::Profile(profile.clone())))
        }
        PluginRuntimeError { plugin, .. }
        | PluginConnectFailed { plugin, .. }
        | PluginRecommended { plugin, .. }
        | PluginUpdateNeedsPermissions { plugin, .. } => {
            Some(NotificationTarget::Plugin(plugin.clone()))
        }
        EngineStopped { .. } | KeyRemapUnavailable { .. } | Generic { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_lead_to_the_page_they_are_about() {
        use crate::domain::state::Page;
        let devices = vec![halod_shared::types::WireDevice {
            id: "mouse-1".parse().unwrap(),
            name: "Glide Pro".into(),
            ..Default::default()
        }];
        let target = |code| notification_target(&code, &devices);
        assert_eq!(
            target(NotificationCode::LowBattery {
                device_id: "mouse-1".parse().unwrap(),
                device: "Glide Pro".into(),
                battery: "Battery".into(),
                level: 5,
                threshold: 10,
            }),
            Some(NotificationTarget::Page(Page::Device(
                "mouse-1".parse().unwrap()
            )))
        );
        assert_eq!(
            target(NotificationCode::PluginUpdateNeedsPermissions {
                plugin: "openrgb".parse().unwrap(),
                name: "OpenRGB".into(),
            }),
            Some(NotificationTarget::Plugin("openrgb".parse().unwrap()))
        );
        assert_eq!(
            target(NotificationCode::PluginConnectFailed {
                plugin: "openrgb".parse().unwrap(),
                detail: "refused".into(),
            }),
            Some(NotificationTarget::Plugin("openrgb".parse().unwrap()))
        );
        assert_eq!(
            target(NotificationCode::FanStalled { fan: "cpu".into() }),
            Some(NotificationTarget::Page(Page::Cooling))
        );
        assert_eq!(
            target(NotificationCode::DeviceInitFailed {
                device_id: "gone-1".parse().unwrap(),
                device: "Gone".into(),
                detail: "x".into(),
            }),
            None
        );
        assert_eq!(
            target(NotificationCode::Generic {
                message: "x".into()
            }),
            None
        );
    }

    #[test]
    fn notification_text_covers_every_code_with_interpolation() {
        use NotificationCode::*;
        let cases = [
            EngineStopped {
                detail: "boom".into(),
            },
            KeyRemapUnavailable {
                detail: "boom".into(),
            },
            ProfileSwitched {
                profile: "Gaming".into(),
            },
            DeviceInitFailed {
                device_id: "kraken-1".parse().unwrap(),
                device: "Kraken".into(),
                detail: "boom".into(),
            },
            DeviceWriteFailed {
                device_id: "kraken-1".parse().unwrap(),
                device: "Kraken".into(),
                detail: "boom".into(),
            },
            FanStalled { fan: "cpu".into() },
            PluginRuntimeError {
                plugin: "wled_udp".parse().unwrap(),
                detail: "boom".into(),
            },
            PluginConnectFailed {
                plugin: "wled_udp".parse().unwrap(),
                detail: "boom".into(),
            },
            PluginRecommended {
                plugin: "nzxt_kraken".parse().unwrap(),
                name: "NZXT Kraken".into(),
            },
            PluginUpdateNeedsPermissions {
                plugin: "nzxt_kraken".parse().unwrap(),
                name: "NZXT Kraken".into(),
            },
            Generic {
                message: "boom".into(),
            },
        ];
        for code in cases {
            let (title, message) = notification_text(&code);
            assert!(!title.trim().is_empty(), "empty title for {code:?}");
            assert!(!message.trim().is_empty(), "empty message for {code:?}");
            // A missing key makes rust-i18n echo the key path back verbatim.
            assert!(!title.contains("notify."), "untranslated title: {title}");
            assert!(
                !message.contains("notify."),
                "untranslated message: {message}"
            );
        }
        // Params are spliced into the copy, not dropped.
        let (_, msg) = notification_text(&ProfileSwitched {
            profile: "Gaming".into(),
        });
        assert!(msg.contains("Gaming"), "param not interpolated: {msg}");

        let (title, message) = notification_text(&PluginRuntimeError {
            plugin: "xyz".parse().unwrap(),
            detail: "private stack trace".into(),
        });
        assert_eq!(title, "An error occurred in plugin xyz");
        assert!(!title.contains("private stack trace"));
        assert!(!message.contains("private stack trace"));
    }
}
