// SPDX-License-Identifier: GPL-3.0-or-later
//! USB topology observer for non-HID plugin devices.

use std::collections::HashSet;
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::device::UsbLocation;
use crate::infrastructure::drivers::transports::usb::{
    present_usb_locations, stale_usb_ids, USB_LIVENESS_POLL,
};

async fn list_present() -> Option<HashSet<UsbLocation>> {
    tokio::task::spawn_blocking(present_usb_locations)
        .await
        .ok()
        .flatten()
}

pub async fn run(app: Arc<AppState>) {
    let mut last_present = list_present().await.unwrap_or_default();
    loop {
        tokio::time::sleep(USB_LIVENESS_POLL).await;
        let Some(present) = list_present().await else {
            continue;
        };
        let registered = {
            let devices = app.device_registry.read().await;
            devices
                .iter()
                .filter_map(|device| Some((device.id().to_owned(), device.usb_location()?)))
                .collect::<Vec<_>>()
        };
        for id in stale_usb_ids(&registered, &present) {
            crate::application::usecases::registry::registration::unregister_device_and_children(
                &app, &id,
            )
            .await;
        }
        if present != last_present {
            for arrived in present.difference(&last_present) {
                app.registry
                    .reset_transport_open_failures_for_usb(arrived.vid, arrived.pid);
            }
            crate::domain::registry::observers::discovery::scan_usb_non_hid(Arc::clone(&app)).await;
        }
        last_present = present;
    }
}
