// SPDX-License-Identifier: GPL-3.0-or-later
//! Best-effort native desktop notifications for events discovered by the GUI.

/// Show a native notification without blocking the render loop. Delivery is
/// best-effort: the in-app error toast remains the authoritative notification.
pub fn show(title: &str, message: &str) {
    platform::show(title, message);
}

#[cfg(target_os = "linux")]
mod platform {
    use std::collections::HashMap;
    use std::time::Duration;

    use zbus::zvariant::{Str, Value};

    pub(super) fn show(title: &str, message: &str) {
        let title = title.to_owned();
        let message = message.to_owned();
        std::thread::spawn(move || {
            for attempt in 1..=3 {
                match send(&title, &message) {
                    Ok(id) => {
                        log::debug!("showed native notification {id}");
                        return;
                    }
                    Err(error) if attempt < 3 => {
                        log::warn!(
                            "failed to show native notification (attempt {attempt}/3): {error}"
                        );
                        std::thread::sleep(Duration::from_millis(500 * attempt));
                    }
                    Err(error) => {
                        log::warn!("failed to show native notification after 3 attempts: {error}");
                    }
                }
            }
        });
    }

    fn send(title: &str, message: &str) -> zbus::Result<u32> {
        let connection = zbus::blocking::Connection::session()?;
        let proxy = zbus::blocking::Proxy::new(
            &connection,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )?;
        let actions: Vec<&str> = Vec::new();
        let mut hints = HashMap::new();
        // Freedesktop urgency 2 is critical. A zero timeout requests a
        // persistent native notification; the desktop may still apply policy.
        hints.insert("urgency", Value::U8(2));
        // Associate the notification with our installed desktop file. GNOME,
        // Plasma, and other servers can use this for app permissions, grouping,
        // and presentation instead of treating the sender as an unknown client.
        hints.insert(
            "desktop-entry",
            Value::Str(Str::from(halod_shared::app::APP_ID)),
        );
        hints.insert("resident", Value::Bool(true));
        proxy.call(
            "Notify",
            &(
                halod_shared::app::APP_DISPLAY_NAME,
                0_u32,
                halod_shared::app::APP_NAME,
                title,
                message,
                actions,
                hints,
                0_i32,
            ),
        )
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use windows::{
        core::HSTRING,
        Data::Xml::Dom::XmlDocument,
        Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID,
        UI::Notifications::{ToastNotification, ToastNotificationManager},
    };

    pub(super) fn show(title: &str, message: &str) {
        if let Err(error) = send(title, message) {
            log::warn!("failed to show native notification: {error}");
        }
    }

    fn send(title: &str, message: &str) -> windows::core::Result<()> {
        // The installer assigns this same AUMID to its shortcuts. Windows uses
        // that identity to attribute unpackaged desktop-app notifications.
        let app_id = HSTRING::from(halod_shared::app::APP_ID);
        unsafe { SetCurrentProcessExplicitAppUserModelID(&app_id)? };
        let xml = format!(
            "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
            escape_xml(title),
            escape_xml(message)
        );
        let document = XmlDocument::new()?;
        document.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&document)?;
        ToastNotificationManager::CreateToastNotifierWithId(&app_id)?.Show(&toast)
    }

    fn escape_xml(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod platform {
    pub(super) fn show(_title: &str, _message: &str) {}
}
