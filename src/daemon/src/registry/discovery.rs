// SPDX-License-Identifier: GPL-3.0-or-later
// Discovery is driven by transport scanners. Peripheral construction is owned
// exclusively by the runtime plugin registry.
use halod_shared::types::DiscoveryPhase;
use std::{future::Future, pin::Pin, sync::Arc};

use crate::drivers::plugins::DeviceSpec;

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
    },
    Smbus {
        bus: Arc<dyn crate::drivers::transports::smbus::SmBusOps>,
        addr: u8,
        bus_kind: crate::drivers::transports::smbus::SmbusBusKind,
    },
    Command {
        executable: &'a str,
    },
    AmdSmn {
        family: u8,
        model: u8,
    },
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
/// is touching `app.devices`" (the coarse gate, `integration_monitor`'s
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

/// Convenience: find the matching descriptor, construct, and register.
/// Under a [`DiscoveryScope::PluginSet`], silently skips handles outside it.
pub async fn discover_handle(app: &Arc<crate::state::AppState>, handle: DiscoveryHandle<'_>) {
    let scoped = matches!(
        *app.discovery_scope.read().await,
        DiscoveryScope::PluginSet { .. }
    );
    if scoped && !app.handle_in_scope(&handle).await {
        return;
    }
    if let Some(device) = app.registry.make_device(app, handle) {
        crate::registry::usecases::registration::register_device_and_children(app, device).await;
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
        };
        let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
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
