// SPDX-License-Identifier: GPL-3.0-or-later
// Discovery is driven by two inventory-registered types:
//
// - `TransportScanner` — submitted by each transport; `discover_devices()` loops over all of them.
// - `DeviceDescriptor` — submitted by each device module; bus scanners call `make_device()` or
//   `discover_handle()` to match a handle against registered descriptors and construct devices.
use halod_shared::types::DiscoveryPhase;
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
        bus: Arc<dyn crate::drivers::transports::smbus::SmBusOps>,
        addr: u8,
        bus_kind: crate::drivers::transports::smbus::SmbusBusKind,
    },
    ChainAccessory {
        channel_id: u8,
        accessory_id: u8,
        chain_hub: Arc<dyn crate::drivers::chain::ChainHub>,
        fan_hub: Arc<dyn crate::drivers::FanHub>,
    },
    // Logitech wireless device
    LogitechSlot {
        devnum: u8,
        wpid: u16,
        serial: Option<&'a str>,
        messenger: Arc<dyn crate::drivers::vendors::logitech::protocols::hidpp::HidppChannel>,
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
    /// Optional write-rate ceiling applied to the bus this entry opens, shared
    /// by every stick/controller it discovers. Slow controllers (e.g. ENE DRAM)
    /// declare one so a rapid effect stream to several modules on one bus can't
    /// outrun what the hardware latches. `None` leaves the bus unthrottled.
    pub write_rate_limit: Option<halod_shared::types::WriteRateLimit>,
    /// PCI-identity gate confining this scan to known cards. Empty is permitted
    /// only on a `Chipset` bus; a `Gpu` entry MUST list at least one match or the
    /// scanner refuses to touch any GPU bus (the display-bus hazard). See
    /// [`crate::drivers::transports::smbus::PciMatch`].
    pub pci_match: &'static [crate::drivers::transports::smbus::PciMatch],
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
            .unwrap_or_else(|e| {
                log::warn!("USB non-HID: device list failed: {e}");
                Default::default()
            });

        for (vid, pid) in present {
            crate::registry::discovery::discover_handle(
                &app,
                crate::registry::discovery::DiscoveryHandle::UsbNonHid { vid, pid },
            )
            .await;
        }
    }),
});

/// Does NOT register — the caller decides what to do with the result.
pub fn make_device(handle: DiscoveryHandle<'_>) -> Option<Arc<dyn crate::drivers::Device>> {
    // Plugins are consulted before native descriptors so a plugin shadows a
    // native driver for the same hardware (the Corsair/Razer migration path).
    if let Some(device) = crate::drivers::plugins::match_handle(&handle) {
        return Some(device);
    }
    for desc in inventory::iter::<DeviceDescriptor> {
        if (desc.matches)(&handle) {
            return match (desc.make)(handle) {
                Ok(device) => Some(device),
                Err(e) => {
                    // `{e:#}` prints the full anyhow context chain (e.g. the
                    // underlying "Permission denied" behind a higher-level wrap).
                    log::warn!("Device construction failed: {e:#}");
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
        crate::registry::usecases::registration::register_device(app, device).await;
    }
}

use crate::{ipc::broadcast_state, state::AppState};

/// Push a free-form status line describing the current discovery step to any
/// connected UI. Transport scanners call this to report finer-grained progress
/// (e.g. the specific bus being probed) than the top-level per-transport label.
pub async fn set_discovery_detail(app: &Arc<AppState>, detail: impl Into<String>) {
    {
        let mut discovery = app.discovery.lock().await;
        discovery.detail = detail.into();
    }
    broadcast_state(app).await;
}

pub async fn discover_devices(app: Arc<AppState>) {
    {
        let mut discovery = app.discovery.lock().await;
        discovery.phase = DiscoveryPhase::Discovering;
    }
    broadcast_state(&app).await;

    for scanner in inventory::iter::<TransportScanner> {
        if let Some(platform) = scanner.platform {
            if platform != std::env::consts::OS {
                continue;
            }
        }
        log::debug!("Running transport scanner: {}", scanner.name);
        set_discovery_detail(&app, scanner.name).await;
        if tokio::time::timeout(
            std::time::Duration::from_secs(30),
            (scanner.scan)(Arc::clone(&app)),
        )
        .await
        .is_err()
        {
            log::warn!("Scanner '{}' timed out", scanner.name);
        }
    }

    log::info!("Discovered {} devices", app.devices.read().await.len());

    {
        let mut discovery = app.discovery.lock().await;
        discovery.phase = DiscoveryPhase::Complete;
        discovery.detail = String::new();
    }
    broadcast_state(&app).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_device_returns_none_for_unrecognised_handle() {
        let handle = DiscoveryHandle::UsbNonHid {
            vid: 0x0000,
            pid: 0x0000,
        };
        assert!(make_device(handle).is_none());
    }
}
