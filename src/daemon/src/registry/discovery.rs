// SPDX-License-Identifier: GPL-3.0-or-later
// Discovery is driven by two inventory-registered types:
//
// - `TransportScanner` — submitted by each transport; `discover_devices()` loops over all of them.
// - `DeviceDescriptor` — submitted by each device module; bus scanners call `make_device()` or
//   `discover_handle()` to match a handle against registered descriptors and construct devices.
use halod_shared::types::DiscoveryPhase;
use std::{future::Future, pin::Pin, sync::Arc};

use crate::drivers::plugins::DeviceSpec;

/// All information a bus scanner passes to a device descriptor.
#[derive(Clone)]
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
    #[allow(dead_code)] // plugin discovery protocol variant; no built-in currently emits it
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

/// A discovery gate: when set on `AppState::discovery_filter`, only handles
/// matching at least one of the declared `DeviceSpec`s are registered.
/// `None` means no filter — every handle passes.
pub struct DiscoveryFilter {
    pub specs: Vec<DeviceSpec>,
}

impl DiscoveryFilter {
    /// True when `handle` matches at least one spec in this filter.
    pub fn matches(&self, handle: &DiscoveryHandle<'_>) -> bool {
        self.specs.iter().any(|s| s.matches(handle))
    }
}

/// SMBus scan configuration submitted alongside a `DeviceDescriptor`.
/// The SMBus scanner iterates these to know which addresses to probe on which bus, then calls `discover_handle()` for each hit.
type SmBusPreScan = fn(
    Arc<crate::drivers::transports::smbus::SmBusDevice>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

pub struct SmBusScanEntry {
    pub bus_kind: crate::drivers::transports::smbus::SmbusBusKind,
    pub addresses: &'static [u8],
    pub pre_scan: Option<SmBusPreScan>,
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
type TransportScan = fn(Arc<crate::state::AppState>) -> Pin<Box<dyn Future<Output = ()> + Send>>;

pub struct TransportScanner {
    pub name: &'static str,
    /// `std::env::consts::OS` value to restrict to one platform, or `None`.
    pub platform: Option<&'static str>,
    pub scan: TransportScan,
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

/// Construct the native device for `handle` (no plugin matching) — the compile-
/// time native registry backend. Plugin composition lives in
/// [`crate::drivers::plugins::Registry::make_device`], which calls this as its
/// fallback. Also used directly by [`discover_handle_replacing`] to find the
/// native device a plugin would shadow so it can be evicted.
pub fn make_device_native_only(
    handle: DiscoveryHandle<'_>,
) -> Option<Arc<dyn crate::drivers::Device>> {
    for desc in inventory::iter::<DeviceDescriptor> {
        if (desc.matches)(&handle) {
            return match (desc.make)(handle) {
                Ok(device) => Some(device),
                Err(e) => {
                    log::warn!("Device construction failed (native-only): {e:#}");
                    None
                }
            };
        }
    }
    None
}

/// True when a native `DeviceDescriptor` matches `handle` (no hardware open —
/// just the descriptor's `matches` fn). Used for the enable-over-native-shadow
/// eviction path.
pub fn has_native_match(handle: &DiscoveryHandle<'_>) -> bool {
    inventory::iter::<DeviceDescriptor>().any(|desc| (desc.matches)(handle))
}

/// Build the winning device for `handle` (a plugin shadows native, so an
/// enabled plugin wins) and register it. When the winner is a plugin device but
/// a native driver *also* matches the same hardware, first evict the stale
/// native device a prior unfiltered scan left registered — otherwise both would
/// end up bound to the same hardware. Used during scoped rediscovery.
pub async fn discover_handle_replacing(
    app: &Arc<crate::state::AppState>,
    handle: DiscoveryHandle<'_>,
) {
    let Some(device) = app.registry.make_device(app, handle.clone()) else {
        return;
    };
    // A plugin claimed hardware a native driver also matches: the native device
    // still registered from the last full scan has a *different* id (dedup won't
    // evict it), so probe the native id and drop it before the plugin takes over.
    if device.owning_plugin_id().is_some() && has_native_match(&handle) {
        if let Some(native) = make_device_native_only(handle) {
            let native_id = native.id().to_owned();
            native.close().await;
            crate::registry::usecases::registration::unregister_device_and_children(
                app, &native_id,
            )
            .await;
        }
    }
    crate::registry::usecases::registration::register_device(app, device).await;
}

/// Convenience: find the matching descriptor, construct, and register.
/// When a [`DiscoveryFilter`] is active on `app`, silently skips handles
/// that don't match it — all other handles are left undisturbed.
/// With a filter active, routes through [`discover_handle_replacing`] to
/// evict any native device a newly enabled plugin would shadow.
pub async fn discover_handle(app: &Arc<crate::state::AppState>, handle: DiscoveryHandle<'_>) {
    let filtered = app.discovery_filter.read().await.is_some();
    if filtered {
        if !app.handle_in_scope(&handle).await {
            return;
        }
        discover_handle_replacing(app, handle).await;
        return;
    }
    if let Some(device) = app.registry.make_device(app, handle) {
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

    // Unit tests drive the usecase/reconcile layer with synthetic handles and a
    // `MockDevice` registry; enumerating the host's real USB/HID/SMBus hardware
    // here would hang and non-deterministically register stray devices. Skip the
    // scanners under `cfg(test)` — device construction/matching is covered
    // directly via `make_device`/`discover_handle` with hand-built handles.
    if !cfg!(test) {
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
        let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
        assert!(app.registry.make_device(&app, handle).is_none());
    }

    #[test]
    fn has_native_match_is_false_for_unknown_hardware() {
        let handle = DiscoveryHandle::UsbNonHid { vid: 0, pid: 0 };
        assert!(!has_native_match(&handle));
    }

    #[test]
    fn make_device_native_only_is_none_for_unknown_hardware() {
        let handle = DiscoveryHandle::UsbNonHid { vid: 0, pid: 0 };
        assert!(make_device_native_only(handle).is_none());
    }

    fn hid_handle(vid: u16, pid: u16) -> DiscoveryHandle<'static> {
        DiscoveryHandle::Hid {
            vid,
            pid,
            path: "",
            serial: None,
            idx: 0,
            usage_page: 0,
            usage: 0,
            interface_number: None,
        }
    }

    #[test]
    fn discovery_filter_matches_only_declared_specs() {
        let spec: DeviceSpec = serde_json::from_value(serde_json::json!({
            "vendor": "x", "model": "y", "transport": "hid",
            "vid": 0x1234, "pid": 0x5678,
        }))
        .unwrap();
        let filter = DiscoveryFilter { specs: vec![spec] };

        assert!(filter.matches(&hid_handle(0x1234, 0x5678)));
        assert!(!filter.matches(&hid_handle(0x1234, 0x9999)));
        assert!(
            !DiscoveryFilter { specs: vec![] }.matches(&hid_handle(0x1234, 0x5678)),
            "an empty filter matches nothing"
        );
    }
}
