// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::{
    drivers::Device,
    ipc::broadcast_state,
    registry::discovery::{DiscoveryHandle, TransportScanner},
    state::{AppState, HidTrackingEntry},
};

/// Exponential backoff capped at 30 s. After 5+ consecutive failures, promote
/// logging to warn so operators notice a persistent HID subsystem problem.
async fn backoff_sleep(failures: u32) {
    let secs = std::cmp::min(1u64 << failures.min(5), 30);
    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
}

/// Background service driven from `main`: re-enumerates HID devices every 2 s,
/// adding newly connected devices and removing disconnected ones. Backs off
/// exponentially after repeated failures (e.g. post-suspend).
pub async fn hotplug_monitor(app: Arc<AppState>) {
    let mut failures: u32 = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let live: Vec<HidDeviceInfo> = match tokio::task::spawn_blocking(|| {
            hidapi::HidApi::new().map(|api| api.device_list().map(HidDeviceInfo::from).collect())
        })
        .await
        {
            Ok(Ok(v)) => {
                failures = 0;
                v
            }
            Ok(Err(e)) => {
                failures += 1;
                if failures == 1 {
                    log::debug!("Hotplug: HID enumeration failed: {e}");
                } else {
                    log::warn!("Hotplug: HID enumeration failed ({failures} consecutive): {e}");
                }
                backoff_sleep(failures).await;
                continue;
            }
            Err(e) => {
                failures += 1;
                log::warn!("Hotplug: spawn_blocking panicked ({failures} consecutive): {e}");
                backoff_sleep(failures).await;
                continue;
            }
        };

        let live_keys: HashSet<String> = live
            .iter()
            .map(|i| hid_key(i.vid, i.pid, &i.serial))
            .collect();
        let tracked_keys: HashSet<String> = app.hid.keys().await;

        for key in tracked_keys
            .difference(&live_keys)
            .cloned()
            .collect::<Vec<_>>()
        {
            handle_hid_key_removed(Arc::clone(&app), key).await;
        }

        // Re-snapshot so a key freed this cycle can be re-added in the same pass.
        let tracked_keys: HashSet<String> = app.hid.keys().await;

        let picked = pick_hid_devices(&app.registry, &live);
        for (info, idx) in devices_to_register(&picked, &tracked_keys) {
            let serial_opt = (!info.serial.is_empty()).then_some(info.serial.as_str());
            add_hid_device(
                &app,
                info.vid,
                info.pid,
                &info.path,
                serial_opt,
                idx,
                info.usage_page,
                info.usage,
                Some(info.iface),
            )
            .await;
        }
    }
}

async fn discover(app: Arc<AppState>) -> Result<()> {
    let entries: Vec<HidDeviceInfo> = match tokio::task::spawn_blocking(|| {
        hidapi::HidApi::new().map(|api| api.device_list().map(HidDeviceInfo::from).collect())
    })
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            log::error!("Failed to initialize HIDAPI: {e}");
            return Ok(());
        }
        Err(e) => {
            log::error!("HID discover: spawn_blocking panicked: {e}");
            return Ok(());
        }
    };

    // Skip already-registered devices so a rescan never opens a second handle to
    // hardware in use (a rival listener corrupts HID++ messaging for both).
    let tracked_keys: HashSet<String> = app.hid.keys().await;
    let picked = pick_hid_devices(&app.registry, &entries);
    for (info, idx) in devices_to_register(&picked, &tracked_keys) {
        log::debug!(
            "Checking HID {:04x}:{:04x} path={} iface={}",
            info.vid,
            info.pid,
            info.path,
            info.iface
        );
        let serial = (!info.serial.is_empty()).then_some(info.serial.as_str());
        add_hid_device(
            &app,
            info.vid,
            info.pid,
            &info.path,
            serial,
            idx,
            info.usage_page,
            info.usage,
            Some(info.iface),
        )
        .await;
    }

    Ok(())
}

inventory::submit!(TransportScanner {
    name: "HID",
    detail: halod_shared::types::DiscoveryDetail::Hid,
    platform: None,
    scan: |app| Box::pin(async move {
        if let Err(e) = discover(app).await {
            log::error!("HID discovery failed: {e}");
        }
    }),
});

/// Owned snapshot of a single enumerated HID collection.
///
/// On Windows one USB interface can expose several collections, each a separate
/// entry sharing vid/pid/iface/serial but differing by `usage_page`/`usage`.
struct HidDeviceInfo {
    vid: u16,
    pid: u16,
    path: String,
    iface: i32,
    serial: String,
    usage_page: u16,
    usage: u16,
}

impl From<&hidapi::DeviceInfo> for HidDeviceInfo {
    fn from(i: &hidapi::DeviceInfo) -> Self {
        HidDeviceInfo {
            vid: i.vendor_id(),
            pid: i.product_id(),
            path: i.path().to_string_lossy().into_owned(),
            iface: i.interface_number(),
            serial: i
                .serial_number()
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_default(),
            usage_page: i.usage_page(),
            usage: i.usage(),
        }
    }
}

/// Resolve enumerated HID entries to one entry per physical device,
/// keyed by `(vid, pid, serial)`.
///
/// Filters entries against the loaded plugin registry. When multiple entries exist for one
/// physical device (Windows HID collections), the one whose usage_page/usage
/// satisfies a match is preferred; otherwise the first entry in the group
/// wins. Result order follows enumeration first-occurrence so device `idx`
/// assignment stays stable.
fn pick_hid_devices<'a>(
    registry: &crate::plugin::Registry,
    entries: &'a [HidDeviceInfo],
) -> Vec<&'a HidDeviceInfo> {
    let make_probe = |e: &HidDeviceInfo| DiscoveryHandle::Hid {
        vid: e.vid,
        pid: e.pid,
        path: "",
        serial: None,
        idx: 0,
        usage_page: e.usage_page,
        usage: e.usage,
        interface_number: Some(e.iface),
    };

    let mut order: Vec<(u16, u16, String)> = Vec::new();
    let mut groups: HashMap<(u16, u16, String), Vec<&HidDeviceInfo>> = HashMap::new();

    let is_recognized = |probe: &DiscoveryHandle<'_>| registry.has_match(probe);

    for e in entries {
        let probe = make_probe(e);
        if !is_recognized(&probe) {
            continue;
        }
        let key = (e.vid, e.pid, e.serial.clone());
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(e);
    }

    order
        .into_iter()
        .map(|key| {
            let candidates = &groups[&key];
            candidates
                .iter()
                .copied()
                .find(|e| is_recognized(&make_probe(e)))
                .unwrap_or(candidates[0])
        })
        .collect()
}

fn hid_key(vid: u16, pid: u16, serial: &str) -> String {
    format!("{vid:04x}:{pid:04x}:{serial}")
}

/// From the picker's output, the entries not already tracked, each tagged with
/// the device index it should receive. Filtering out tracked devices avoids
/// registering a second `Device` (and rival listener) for open hardware; indices
/// continue past the tracked devices of the same `(vid, pid)` so IDs stay unique.
fn devices_to_register<'a>(
    picked: &[&'a HidDeviceInfo],
    tracked_keys: &HashSet<String>,
) -> Vec<(&'a HidDeviceInfo, usize)> {
    let mut counts: HashMap<(u16, u16), usize> = tracked_keys
        .iter()
        .filter_map(|k| {
            let mut p = k.splitn(3, ':');
            let vid = u16::from_str_radix(p.next()?, 16).ok()?;
            let pid = u16::from_str_radix(p.next()?, 16).ok()?;
            Some((vid, pid))
        })
        .fold(HashMap::new(), |mut m, k| {
            *m.entry(k).or_insert(0) += 1;
            m
        });

    let mut out = Vec::new();
    for &info in picked {
        if tracked_keys.contains(&hid_key(info.vid, info.pid, &info.serial)) {
            continue;
        }
        let counter = counts.entry((info.vid, info.pid)).or_insert(0);
        let idx = *counter;
        *counter += 1;
        out.push((info, idx));
    }
    out
}

/// Handles a HID key that has disappeared from the enumeration.
///
pub(crate) async fn handle_hid_key_removed(app: Arc<AppState>, key: String) {
    let entry = app.hid.untrack(&key).await;
    if let Some(HidTrackingEntry::Primary(arcs)) = entry {
        let to_close: Vec<Arc<dyn Device>> = {
            let mut devs = app.devices.write().await;
            let closing: Vec<_> = devs
                .iter()
                .filter(|d| arcs.iter().any(|a| Arc::ptr_eq(a, d)))
                .cloned()
                .collect();
            devs.retain(|d| !arcs.iter().any(|a| Arc::ptr_eq(a, d)));
            closing
        };
        let mut should_rescan_controllers = false;
        for device in &to_close {
            if device.wire_connection_type().await
                == Some(halod_shared::types::ConnectionType::Wired)
            {
                should_rescan_controllers = true;
                break;
            }
        }
        for d in &to_close {
            crate::registry::usecases::registration::close_device(&app, d).await;
        }
        log::info!("Hotplug: removed device(s) for key {key}");
        broadcast_state(&app).await;

        // The device may now be available through its paired receiver, whose
        // pairing table only shows the slot once the cable is gone — rescan
        // every controller after a short delay.
        if should_rescan_controllers {
            let app2 = Arc::clone(&app);
            tokio::spawn(async move {
                if let Err(e) = async {
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    let controllers: Vec<Arc<dyn Device>> = app2
                        .devices
                        .read()
                        .await
                        .iter()
                        .filter(|d| d.as_controller().is_some())
                        .cloned()
                        .collect();
                    for ctrl_dev in controllers {
                        if let Some(ctrl) = ctrl_dev.as_controller() {
                            let children = ctrl.rescan_children().await;
                            for child in children {
                                crate::registry::usecases::registration::register_device(
                                    &app2, child,
                                )
                                .await;
                            }
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                }
                .await
                {
                    log::error!("[hotplug] controller rescan task failed: {e}");
                }
            });
        }
    }
}

/// Creates, initializes, and registers one HID device (plus any hub children).
///
/// The new device is registered as a `Primary` entry. Wired/wireless duplicate
/// arbitration is handled by the central plugin registration lifecycle.
#[allow(clippy::too_many_arguments)] // HID identity and usage tuple comes directly from hidapi
async fn add_hid_device(
    app: &Arc<AppState>,
    vid: u16,
    pid: u16,
    path: &str,
    serial: Option<&str>,
    idx: usize,
    usage_page: u16,
    usage: u16,
    interface_number: Option<i32>,
) {
    let handle = DiscoveryHandle::Hid {
        vid,
        pid,
        path,
        serial,
        idx,
        usage_page,
        usage,
        interface_number,
    };
    // Scoped-plugin gate: under `PluginSet`, only in-scope handles (the
    // changed plugins' hardware) register. A scoped device otherwise goes
    // through the same register → children → track flow as a normal one, so a
    // re-enabled plugin gets its children and HID tracking too — only the
    // native-eviction below and skipping the TransportSwitchable adoption are
    // scoped-specific.
    let scoped = {
        use crate::registry::discovery::DiscoveryScope;
        match &*app.discovery_scope.read().await {
            DiscoveryScope::PluginSet { filter, .. } => {
                if !filter.matches(&handle) {
                    return;
                }
                true
            }
            DiscoveryScope::Clean | DiscoveryScope::Full => false,
        }
    };

    let Some(impl_) = app.registry.make_device(app, handle.clone()) else {
        return;
    };
    let serial_key = serial.filter(|s| !s.is_empty()).unwrap_or("").to_string();
    let key = hid_key(vid, pid, &serial_key);

    let _ = scoped;

    // Register as a new primary device via the centralised registration lifecycle.
    let registered =
        crate::registry::usecases::registration::register_device(app, impl_.clone()).await;
    if !registered {
        return;
    }

    if impl_.active_state() != halod_shared::types::VisibilityState::Disabled {
        if let Some(ctrl) = impl_.as_controller() {
            for child in ctrl.discover_children().await {
                crate::registry::usecases::registration::register_device(app, child).await;
            }
        }
    }

    // Collect the parent and all devices registered after it in a single lock
    // to avoid a TOCTOU race with the hotplug monitor. Find the parent by Arc
    // pointer equality rather than a stale index.
    let arcs: Vec<Arc<dyn Device>> = {
        let devs = app.devices.read().await;
        match devs.iter().position(|d| Arc::ptr_eq(d, &impl_)) {
            Some(pos) => devs[pos..].to_vec(),
            None => return,
        }
    };
    app.hid.track(key, HidTrackingEntry::Primary(arcs)).await;
}
