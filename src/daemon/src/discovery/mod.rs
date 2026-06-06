// Discovery is driven by two inventory-registered types:
//
// - `TransportScanner` — submitted by each transport; `discover_devices()` loops over all of them.
// - `DeviceDescriptor` — submitted by each device module; bus scanners call `make_device()` or
//   `discover_handle()` to match a handle against registered descriptors and construct devices.
use halod_protocol::types::DiscoveryPhase;
use std::{future::Future, pin::Pin, sync::Arc};

/// All information a bus scanner passes to a device descriptor.
pub enum DiscoveryHandle<'a> {
    Hid {
        vid: u16,
        pid: u16,
        path: &'a str,
        serial: Option<&'a str>,
        /// 0-based index among devices with the same (vid, pid) on this bus.
        idx: usize,
        /// Windows HID collection routing fields; 0 on Linux.
        usage_page: u16,
        usage: u16,
        interface_number: Option<i32>,
    },
    UsbNonHid {
        vid: u16,
        pid: u16,
    },
    Smbus {
        bus: &'a Arc<crate::drivers::transports::smbus::SmBusDevice>,
        addr: u8,
        bus_kind: crate::drivers::transports::smbus::SmbusBusKind,
    },
    // TODO: should probably be a generic Chain
    NzxtChain {
        channel_id: u8,
        accessory_id: u8,
        chain_hub: Arc<dyn crate::drivers::chain::ChainHub>,
        fan_hub: Arc<dyn crate::drivers::NzxtFanHub>, // TODO: this should be a generic FanHub
    },
    // Logitech wireless device
    LogitechSlot {
        devnum: u8,
        wpid: u16,
        serial: Option<&'a str>,
        messenger: Arc<
            crate::drivers::vendors::logitech::protocols::hidpp::HidppMessenger<
                crate::drivers::transports::hid::HidTransport,
            >,
        >,
    },
}

/// One device registration entry submitted via `inventory::submit!`.
pub struct DeviceDescriptor {
    pub matches: fn(&DiscoveryHandle<'_>) -> bool,
    pub make: fn(DiscoveryHandle<'_>) -> anyhow::Result<Arc<dyn crate::drivers::Device>>,
}
inventory::collect!(DeviceDescriptor);

/// SMBus scan configuration submitted alongside a `DeviceDescriptor`.
/// The SMBus scanner iterates these to know which addresses to probe on which bus, then calls `discover_handle()` for each hit.
pub struct SmBusScanEntry {
    pub bus_kind: crate::drivers::transports::smbus::SmbusBusKind,
    pub addresses: &'static [u8],
    pub pre_scan: Option<
        fn(
            Arc<crate::drivers::transports::smbus::SmBusDevice>,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>,
    >,
}
inventory::collect!(SmBusScanEntry);

/// A bus scanner registered by a transport module.
pub struct TransportScanner {
    pub name: &'static str,
    /// `std::env::consts::OS` value to restrict to one platform, or `None`.
    pub platform: Option<&'static str>,
    pub scan: fn(Arc<crate::state::AppState>) -> Pin<Box<dyn Future<Output = ()> + Send>>,
}
inventory::collect!(TransportScanner);

inventory::submit!(TransportScanner {
    name: "USB non-HID",
    platform: None,
    scan: |app| Box::pin(async move {
        use rusb::{Context, UsbContext};
        let ctx = match Context::new() {
            Ok(c) => c,
            Err(e) => {
                log::error!("USB non-HID discovery failed: {e}");
                return;
            }
        };
        let present: std::collections::HashSet<(u16, u16)> = ctx
            .devices()
            .map(|devs| {
                devs.iter()
                    .filter_map(|d| {
                        d.device_descriptor()
                            .ok()
                            .map(|dd| (dd.vendor_id(), dd.product_id()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        for (vid, pid) in present {
            crate::discovery::discover_handle(
                &app,
                crate::discovery::DiscoveryHandle::UsbNonHid { vid, pid },
            )
            .await;
        }
    }),
});

/// Find the first matching `DeviceDescriptor` and construct the device.
/// Does NOT register — the caller decides what to do with the result.
pub fn make_device(handle: DiscoveryHandle<'_>) -> Option<Arc<dyn crate::drivers::Device>> {
    for desc in inventory::iter::<DeviceDescriptor> {
        if (desc.matches)(&handle) {
            return match (desc.make)(handle) {
                Ok(device) => Some(device),
                Err(e) => {
                    log::warn!("Device construction failed: {e}");
                    None
                }
            };
        }
    }
    None
}

/// Convenience: find the matching descriptor, construct, and register.
pub async fn discover_handle(app: &Arc<crate::state::AppState>, handle: DiscoveryHandle<'_>) {
    if let Some(device) = make_device(handle) {
        crate::usecases::registration::register_device(app, device).await;
    }
}

use crate::{ipc::broadcast_state, state::AppState};

pub async fn discover_devices(app: Arc<AppState>) {
    {
        let mut discovery = app.discovery.lock().await;
        discovery.phase = DiscoveryPhase::Discovering;
    }
    broadcast_state(app.clone()).await;

    for scanner in inventory::iter::<TransportScanner> {
        if let Some(platform) = scanner.platform {
            if platform != std::env::consts::OS {
                continue;
            }
        }
        log::debug!("Running transport scanner: {}", scanner.name);
        (scanner.scan)(Arc::clone(&app)).await;
    }

    log::info!("Discovered {} devices", app.devices.lock().await.len());

    {
        let mut discovery = app.discovery.lock().await;
        discovery.phase = DiscoveryPhase::Complete;
    }
    broadcast_state(app).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_device_returns_none_for_unrecognised_handle() {
        // A UsbNonHid with VID:PID 0x0000:0x0000 should not match any real descriptor.
        let handle = DiscoveryHandle::UsbNonHid {
            vid: 0x0000,
            pid: 0x0000,
        };
        assert!(make_device(handle).is_none());
    }
}
