// SPDX-License-Identifier: GPL-3.0-or-later
// Discovery is driven by transport scanners. Peripheral construction is owned
// exclusively by the runtime plugin registry.
use halod_shared::types::DiscoveryPhase;
use std::{future::Future, pin::Pin, sync::Arc};

use crate::domain::plugin::DeviceSpec;

/// Pending rediscovery work. This is distinct from [`DiscoveryScope`], which
/// describes the scanner currently running. `Full` dominates and plugin sets
/// are unioned, so concurrent requests cannot overwrite each other.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PendingRediscovery {
    #[default]
    Clean,
    PluginSet(std::collections::HashSet<String>),
    Full,
}

impl PendingRediscovery {
    pub fn merge(&mut self, incoming: Self) {
        match (&mut *self, incoming) {
            (Self::Full, _) | (_, Self::Clean) => {}
            (state, Self::Full) => *state = Self::Full,
            (Self::Clean, Self::PluginSet(ids)) => *self = Self::PluginSet(ids),
            (Self::PluginSet(current), Self::PluginSet(ids)) => current.extend(ids),
        }
    }

    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

#[cfg(test)]
mod pending_rediscovery_tests {
    use super::PendingRediscovery;
    use std::collections::HashSet;

    fn ids(values: &[&str]) -> HashSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn plugin_requests_union_and_take_returns_to_clean() {
        let mut pending = PendingRediscovery::Clean;
        pending.merge(PendingRediscovery::PluginSet(ids(&["a"])));
        pending.merge(PendingRediscovery::PluginSet(ids(&["b", "a"])));
        assert_eq!(
            pending.take(),
            PendingRediscovery::PluginSet(ids(&["a", "b"]))
        );
        assert_eq!(pending, PendingRediscovery::Clean);
    }

    #[test]
    fn full_dominates_all_scoped_work() {
        let mut pending = PendingRediscovery::PluginSet(ids(&["a"]));
        pending.merge(PendingRediscovery::Full);
        pending.merge(PendingRediscovery::PluginSet(ids(&["b"])));
        assert_eq!(pending, PendingRediscovery::Full);
    }

    #[test]
    fn no_discovery_root_is_listed_twice() {
        let names: Vec<&str> = super::SCANNERS.iter().map(|s| s.name).collect();
        let unique: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "{names:?}");
    }
}

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
        bus: u8,
        address: u8,
        port_path: &'a [u8],
        serial: Option<&'a str>,
        interface_number: u8,
    },
    Smbus {
        bus: Arc<crate::infrastructure::drivers::transports::smbus::SmBusDevice>,
        addr: u8,
        bus_number: u8,
        bus_kind: crate::infrastructure::drivers::transports::smbus::SmbusBusKind,
    },
    Command {
        executable: &'a str,
    },
    #[cfg(target_os = "windows")]
    AmdSmn {
        family: u8,
        model: u8,
    },
    #[cfg(target_os = "windows")]
    Lpcio {
        slot: u8,
        chip_id: u16,
        revision: u8,
        hwm_base: u16,
    },
}

/// A discovery gate: only handles matching at least one declared `DeviceSpec`
/// pass. Wrapped in a [`DiscoveryScope::PluginSet`].
pub struct DiscoveryFilter {
    pub specs: Vec<DeviceSpec>,
}

impl DiscoveryFilter {
    /// True when `handle` matches at least one spec in this filter.
    pub fn matches(&self, handle: &DiscoveryHandle<'_>) -> bool {
        self.specs.iter().any(|s| s.matches(handle))
    }
}

/// `AppState::discovery_scope`. `PluginSet`/`Full` both mean "a rediscovery
/// is touching `app.device_registry`" (the coarse gate, `integration_monitor`'s
/// tick); only `PluginSet` restricts which handles register (the fine gate,
/// `handle_in_scope`). `Clean -> PluginSet|Full -> Clean`.
#[derive(Clone)]
pub enum DiscoveryScope {
    Clean,
    /// Scoped to `plugin_ids`'s hardware. `filter` is a specs snapshot
    /// (captured before + after `reload_registry`, since a deleted plugin's
    /// specs aren't derivable afterwards).
    PluginSet {
        plugin_ids: std::collections::HashSet<String>,
        filter: Arc<DiscoveryFilter>,
    },
    /// An unscoped full rescan is in flight.
    Full,
}

/// A bus scanner registered by a transport module.
type TransportScan =
    fn(Arc<crate::application::state::AppState>) -> Pin<Box<dyn Future<Output = ()> + Send>>;

pub struct TransportScanner {
    pub name: &'static str,
    pub detail: halod_shared::types::DiscoveryDetail,
    /// `std::env::consts::OS` value to restrict to one platform, or `None`.
    pub platform: Option<&'static str>,
    pub scan: TransportScan,
}

const USB_NON_HID_SCANNER: TransportScanner = TransportScanner {
    name: "USB non-HID",
    detail: halod_shared::types::DiscoveryDetail::Usb,
    platform: None,
    scan: |app| Box::pin(scan_usb_non_hid(app)),
};

/// Every discovery root, run in this order. Scanners are independent — each
/// registers handles through `discover_handle` and reads no other's results.
static SCANNERS: &[TransportScanner] = &[
    crate::application::observers::computer::SCANNER,
    crate::application::observers::hid::SCANNER,
    USB_NON_HID_SCANNER,
    crate::infrastructure::drivers::transports::smbus::SCANNER,
    crate::domain::plugin::observers::host_scan::SCANNER,
    crate::domain::plugin::observers::integration_scan::SCANNER,
];

pub async fn scan_usb_non_hid(app: Arc<crate::application::state::AppState>) {
    let present = tokio::task::spawn_blocking(|| {
        use rusb::{Context, UsbContext};
        let ctx = Context::new().map_err(|error| error.to_string())?;
        Ok::<_, String>(
            ctx.devices()
                .map(|devs| {
                    devs.iter()
                        .filter_map(|d| {
                            let dd = d.device_descriptor().ok()?;
                            let serial = d
                                .open()
                                .ok()
                                .and_then(|h| h.read_serial_number_string_ascii(&dd).ok());
                            let mut interfaces: Vec<u8> = d
                                .active_config_descriptor()
                                .ok()
                                .map(|c| {
                                    c.interfaces()
                                        .flat_map(|i| i.descriptors())
                                        .map(|i| i.interface_number())
                                        .collect()
                                })
                                .unwrap_or_else(|| vec![0]);
                            interfaces.sort_unstable();
                            interfaces.dedup();
                            Some((
                                dd.vendor_id(),
                                dd.product_id(),
                                d.bus_number(),
                                d.address(),
                                d.port_numbers().unwrap_or_default(),
                                serial,
                                interfaces,
                            ))
                        })
                        .collect()
                })
                .unwrap_or_else(|error| {
                    log::warn!("USB non-HID: device list failed: {error}");
                    Default::default()
                }),
        )
    })
    .await;
    let present: Vec<(u16, u16, u8, u8, Vec<u8>, Option<String>, Vec<u8>)> = match present {
        Ok(Ok(present)) => present,
        Ok(Err(error)) => {
            log::error!("USB non-HID discovery failed: {error}");
            return;
        }
        Err(error) => {
            log::error!("USB non-HID discovery task panicked: {error}");
            return;
        }
    };

    for (vid, pid, bus, address, port_path, serial, interfaces) in &present {
        for interface_number in interfaces {
            discover_handle(
                &app,
                DiscoveryHandle::UsbNonHid {
                    vid: *vid,
                    pid: *pid,
                    bus: *bus,
                    address: *address,
                    port_path,
                    serial: serial.as_deref(),
                    interface_number: *interface_number,
                },
            )
            .await;
        }
    }
}

/// Convenience: find the matching descriptor, construct, and register.
/// Under a [`DiscoveryScope::PluginSet`], silently skips handles outside it.
pub async fn discover_handle(
    app: &Arc<crate::application::state::AppState>,
    handle: DiscoveryHandle<'_>,
) {
    let scoped = matches!(
        *app.discovery_scope.read().await,
        DiscoveryScope::PluginSet { .. }
    );
    if scoped && !app.handle_in_scope(&handle).await {
        return;
    }
    if let Some(device) = app.registry.make_device(app, handle) {
        crate::application::usecases::registry::registration::register_device_and_children(
            app, device,
        )
        .await;
    }
}

use crate::application::state::AppState;

/// Push a free-form status line describing the current discovery step to any
/// connected UI. Transport scanners call this to report finer-grained progress
/// (e.g. the specific bus being probed) than the top-level per-transport label.
pub async fn set_discovery_detail(
    app: &Arc<AppState>,
    detail: halod_shared::types::DiscoveryDetail,
) {
    {
        let mut discovery = app.discovery.lock().await;
        discovery.detail = detail;
    }
    crate::application::usecases::registry::runtime::topology_changed(app).await;
}

pub async fn discover_devices(app: Arc<AppState>) {
    // Claim `Full` only if nothing scoped this already (a `reconcile_plugins`
    // caller owns its own `PluginSet` scope and clears it itself).
    let owns_scope = {
        let mut scope = app.discovery_scope.write().await;
        if matches!(*scope, DiscoveryScope::Clean) {
            *scope = DiscoveryScope::Full;
            true
        } else {
            false
        }
    };

    // Full discovery is an explicit user/startup retry boundary. Scoped
    // reconciliation must preserve the current attempt episode unless the
    // plugin's enable state itself changed.
    if matches!(*app.discovery_scope.read().await, DiscoveryScope::Full) {
        app.registry.reset_transport_open_failures();
    }

    {
        let mut discovery = app.discovery.lock().await;
        discovery.phase = DiscoveryPhase::Discovering;
    }
    crate::application::usecases::registry::runtime::topology_changed(&app).await;

    // Unit tests drive the usecase/reconcile layer with synthetic handles and a
    // `MockDevice` registry; enumerating the host's real USB/HID/SMBus hardware
    // here would hang and non-deterministically register stray devices. Skip the
    // scanners under `cfg(test)` — device construction/matching is covered
    // directly via `make_device`/`discover_handle` with hand-built handles.
    if !cfg!(test) {
        for scanner in SCANNERS {
            if let Some(platform) = scanner.platform {
                if platform != std::env::consts::OS {
                    continue;
                }
            }
            log::debug!("Running transport scanner: {}", scanner.name);
            set_discovery_detail(&app, scanner.detail.clone()).await;
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

    log::info!(
        "Discovered {} devices",
        app.device_registry.read().await.len()
    );

    {
        let mut discovery = app.discovery.lock().await;
        discovery.phase = DiscoveryPhase::Complete;
        discovery.detail = Default::default();
    }
    crate::application::usecases::registry::runtime::topology_changed(&app).await;

    if owns_scope {
        app.set_discovery_scope(DiscoveryScope::Clean).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_device_returns_none_for_unrecognised_handle() {
        let handle = DiscoveryHandle::UsbNonHid {
            vid: 0x0000,
            pid: 0x0000,
            bus: 0,
            address: 0,
            port_path: &[],
            serial: None,
            interface_number: 0,
        };
        let app = Arc::new(crate::application::state::AppState::new(
            crate::config::Config::default(),
        ));
        assert!(app.registry.make_device(&app, handle).is_none());
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
        let mut spec: DeviceSpec = serde_json::from_value(serde_json::json!({
            "vendor": "x", "model": "y",
            "match": { "hid": { "vid": 0x1234, "pid": 0x5678 } }
        }))
        .unwrap();
        spec.transport = "hid".to_owned();
        spec.vid = Some(0x1234);
        spec.pid = Some(0x5678);
        let filter = DiscoveryFilter { specs: vec![spec] };

        assert!(filter.matches(&hid_handle(0x1234, 0x5678)));
        assert!(!filter.matches(&hid_handle(0x1234, 0x9999)));
        assert!(
            !DiscoveryFilter { specs: vec![] }.matches(&hid_handle(0x1234, 0x5678)),
            "an empty filter matches nothing"
        );
    }
}
