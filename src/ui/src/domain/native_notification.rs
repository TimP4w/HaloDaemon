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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use zbus::zvariant::{Str, Value};

    static NEXT_NOTIFICATION_ID: AtomicU64 = AtomicU64::new(1);

    pub(super) fn show(title: &str, message: &str) {
        let title = title.to_owned();
        let message = message.to_owned();
        std::thread::spawn(move || {
            for attempt in 1..=3 {
                match send(&title, &message) {
                    Ok(backend) => {
                        log::debug!("showed native notification through {backend}");
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

    fn send(title: &str, message: &str) -> zbus::Result<&'static str> {
        let connection = zbus::blocking::Connection::session()?;
        match send_gtk(&connection, title, message) {
            Ok(()) => Ok("GTK notification service"),
            Err(gtk_error) => {
                log::debug!("GTK notification service unavailable: {gtk_error}");
                send_freedesktop(&connection, title, message)?;
                Ok("freedesktop service")
            }
        }
    }

    fn send_gtk(
        connection: &zbus::blocking::Connection,
        title: &str,
        message: &str,
    ) -> zbus::Result<()> {
        let proxy = zbus::blocking::Proxy::new(
            connection,
            "org.gtk.Notifications",
            "/org/gtk/Notifications",
            "org.gtk.Notifications",
        )?;
        let mut notification: HashMap<&str, Value<'_>> = HashMap::new();
        notification.insert("title", Value::Str(Str::from(title)));
        notification.insert("body", Value::Str(Str::from(message)));
        notification.insert("priority", Value::Str(Str::from("urgent")));

        // Reusing a GTK notification ID replaces the existing notification.
        // Include the process ID so notifications retained across GUI restarts
        // cannot collide with IDs from a previous process.
        let sequence = NEXT_NOTIFICATION_ID.fetch_add(1, Ordering::Relaxed);
        let notification_id = format!("halod-{}-{sequence}", std::process::id());
        proxy.call(
            "AddNotification",
            &(halod_shared::app::APP_ID, notification_id, notification),
        )
    }

    fn send_freedesktop(
        connection: &zbus::blocking::Connection,
        title: &str,
        message: &str,
    ) -> zbus::Result<()> {
        let proxy = zbus::blocking::Proxy::new(
            connection,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )?;
        let actions: Vec<&str> = Vec::new();
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("urgency", Value::U8(2));
        hints.insert(
            "desktop-entry",
            Value::Str(Str::from(halod_shared::app::APP_ID)),
        );
        let _: u32 = proxy.call(
            "Notify",
            &(
                halod_shared::app::APP_DISPLAY_NAME,
                0_u32,
                "",
                title,
                message,
                actions,
                hints,
                10_000_i32,
            ),
        )?;
        Ok(())
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
