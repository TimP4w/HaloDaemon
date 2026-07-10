// SPDX-License-Identifier: GPL-3.0-or-later
//! Translated copy for daemon-pushed notification codes.

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
        WirelessReinitFailed { device } => (
            t!("notify.wireless_reinit_failed.title").to_string(),
            t!("notify.wireless_reinit_failed.message", device = device).to_string(),
        ),
        DeviceReconnectFailed { device } => (
            t!("notify.device_reconnect_failed.title").to_string(),
            t!("notify.device_reconnect_failed.message", device = device).to_string(),
        ),
        ProfileSwitched { profile } => (
            t!("notify.profile_switched.title").to_string(),
            t!("notify.profile_switched.message", profile = profile).to_string(),
        ),
        ChainLinkRestoreFailed { name, detail } => (
            t!("notify.chain_link_restore_failed.title", name = name).to_string(),
            t!("notify.chain_link_restore_failed.message", detail = detail).to_string(),
        ),
        DeviceInitFailed { device, detail } => (
            t!("notify.device_init_failed.title", device = device).to_string(),
            t!("notify.device_init_failed.message", detail = detail).to_string(),
        ),
        FanStalled { fan } => (
            t!("notify.fan_stalled.title", fan = fan).to_string(),
            t!("notify.fan_stalled.message").to_string(),
        ),
        PluginNeedsPermission { plugin } => (
            t!("notify.plugin_needs_permission.title").to_string(),
            t!("notify.plugin_needs_permission.message", plugin = plugin).to_string(),
        ),
        Generic { message } => (t!("notify.error_title").to_string(), message.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            WirelessReinitFailed {
                device: "Kraken".into(),
            },
            DeviceReconnectFailed {
                device: "Kraken".into(),
            },
            ProfileSwitched {
                profile: "Gaming".into(),
            },
            ChainLinkRestoreFailed {
                name: "Strip".into(),
                detail: "boom".into(),
            },
            DeviceInitFailed {
                device: "Kraken".into(),
                detail: "boom".into(),
            },
            FanStalled { fan: "cpu".into() },
            PluginNeedsPermission {
                plugin: "wled_udp".into(),
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
    }
}
