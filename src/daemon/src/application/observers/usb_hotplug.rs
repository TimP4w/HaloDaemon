// SPDX-License-Identifier: GPL-3.0-or-later
//! USB topology observer for non-HID plugin devices.

use std::collections::HashSet;
use std::sync::Arc;

use crate::application::state::AppState;
use crate::application::usecases::registry::{registration, runtime};
use crate::domain::device::UsbLocation;
use crate::domain::registry::observers::discovery;
use crate::infrastructure::drivers::transports::usb::{
    present_usb_locations, stale_usb_ids, USB_LIVENESS_POLL,
};

async fn list_present() -> Option<HashSet<UsbLocation>> {
    tokio::task::spawn_blocking(present_usb_locations)
        .await
        .ok()
        .flatten()
}

async fn drop_unplugged(app: &Arc<AppState>, present: &HashSet<UsbLocation>) {
    let revision = app.device_registry.revision();
    let registered = {
        let devices = app.device_registry.read().await;
        devices
            .iter()
            .filter_map(|device| Some((device.id().to_owned(), device.usb_location()?)))
            .collect::<Vec<_>>()
    };
    for id in stale_usb_ids(&registered, present) {
        registration::unregister_device_and_children(app, &id).await;
    }
    runtime::topology_changed_since(app, revision).await;
}

pub async fn run(app: Arc<AppState>) {
    let mut last_present = list_present().await.unwrap_or_default();
    loop {
        tokio::time::sleep(USB_LIVENESS_POLL).await;
        let Some(present) = list_present().await else {
            continue;
        };
        drop_unplugged(&app, &present).await;
        if present != last_present {
            for arrived in present.difference(&last_present) {
                app.registry
                    .reset_transport_open_failures_for_usb(arrived.vid, arrived.pid);
            }
            let revision = app.device_registry.revision();
            discovery::scan_usb_non_hid(Arc::clone(&app)).await;
            runtime::topology_changed_since(&app, revision).await;
        }
        last_present = present;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_with, MockDevice};

    fn location(address: u8) -> UsbLocation {
        UsbLocation {
            vid: 0x1234,
            pid: 0x5678,
            bus: 1,
            address,
            port_path: vec![address],
        }
    }

    #[tokio::test]
    async fn an_unplugged_usb_device_is_tombstoned() {
        let gone = Arc::new(MockDevice::new("gone").with_usb_location(location(1)));
        let kept = Arc::new(MockDevice::new("kept").with_usb_location(location(2)));
        let app = app_with(vec![gone.clone(), kept, Arc::new(MockDevice::new("other"))]);
        runtime::bootstrap(&app).await;
        let mut transactions = app.data_bus.subscribe_transactions();

        drop_unplugged(&app, &HashSet::from([location(2)])).await;

        let transaction = transactions.recv().await.unwrap();
        assert_eq!(
            transaction.tombstones,
            vec![halod_shared::bus::topic::device("gone")]
        );
        assert!(gone.closed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(app.device_registry.read().await.len(), 2);
    }

    #[tokio::test]
    async fn an_unchanged_bus_publishes_nothing() {
        let app = app_with(vec![Arc::new(
            MockDevice::new("kept").with_usb_location(location(2)),
        )]);
        runtime::bootstrap(&app).await;
        let mut transactions = app.data_bus.subscribe_transactions();

        drop_unplugged(&app, &HashSet::from([location(2)])).await;

        assert!(transactions.try_recv().is_err());
    }
}
