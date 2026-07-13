// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::{
    drivers::Device,
    ipc::broadcast_state,
    platform::notify,
    registry::discovery::{DeviceDescriptor, DiscoveryHandle, TransportScanner},
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
/// Filters entries against both the native `DeviceDescriptor` inventory and
/// the loaded plugin registry (`plugins::has_match`) — a device driven only
/// by a plugin, with no native fallback, must still pass this pre-filter or
/// it never reaches `make_device`. When multiple entries exist for one
/// physical device (Windows HID collections), the one whose usage_page/usage
/// satisfies a match is preferred; otherwise the first entry in the group
/// wins. Result order follows enumeration first-occurrence so device `idx`
/// assignment stays stable.
fn pick_hid_devices<'a>(
    registry: &crate::drivers::plugins::Registry,
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

    let is_recognized = |probe: &DiscoveryHandle<'_>| {
        inventory::iter::<DeviceDescriptor>().any(|d| (d.matches)(probe))
            || registry.has_match(probe)
    };

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
/// Matches on `HidTrackingEntry`:
/// - `Primary`: removes and closes all registered device Arcs.
/// - `WiredOverride`: reverts the device to wireless transport (Arc stays in `app.devices`);
///   if there is no wireless fallback the device is removed and closed.
pub(crate) async fn handle_hid_key_removed(app: Arc<AppState>, key: String) {
    let entry = app.hid.untrack(&key).await;
    match entry {
        Some(HidTrackingEntry::Primary(arcs)) => {
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
            for d in &to_close {
                d.close().await;
            }
            log::info!("Hotplug: removed device(s) for key {key}");
            broadcast_state(&app).await;

            // A wired TransportSwitchable device may now be available through its
            // paired receiver, whose pairing table only shows the slot once the cable
            // is gone — rescan every controller after a short delay.
            let any_switchable = to_close
                .iter()
                .any(|d| d.as_transport_switchable().is_some());
            if any_switchable {
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
        Some(HidTrackingEntry::WiredOverride(dev)) => {
            // Wired path gone — try reverting to wireless; the Arc stays in app.devices.
            if let Some(switchable) = dev.as_transport_switchable() {
                if switchable.disconnect_direct().await {
                    log::info!(
                        "Hotplug: wired key {key} gone, {} reverted to wireless",
                        dev.id()
                    );
                    let dev_arc = Arc::clone(&dev);
                    let app2 = Arc::clone(&app);
                    tokio::spawn(async move {
                        if let Err(e) = async {
                            for attempt in 0..6u8 {
                                tokio::time::sleep(tokio::time::Duration::from_millis(1000))
                                    .await;
                                // Abort if the Primary handler already removed this device.
                                if !app2
                                    .devices
                                    .read()
                                    .await
                                    .iter()
                                    .any(|d| Arc::ptr_eq(d, &dev_arc))
                                {
                                    log::info!(
                                        "[hotplug] {} no longer registered; wireless re-init aborted",
                                        dev_arc.id()
                                    );
                                    return Ok(());
                                }
                                match dev_arc.initialize().await {
                                    Ok(true) => {
                                        let saved_state = {
                                            let cfg = app2.config.read().await;
                                            Some(cfg.effective_device_state(dev_arc.id()))
                                                .filter(|v| !v.is_null())
                                        };
                                        if let Some(state) = saved_state {
                                            dev_arc.load_state(&state).await;
                                        }
                                        log::info!(
                                            "[hotplug] {} re-initialized on wireless (attempt {})",
                                            dev_arc.id(),
                                            attempt + 1
                                        );
                                        broadcast_state(&app2).await;
                                        return Ok(());
                                    }
                                    Ok(false) => log::debug!(
                                        "[hotplug] {} still offline on wireless (attempt {})",
                                        dev_arc.id(),
                                        attempt + 1
                                    ),
                                    Err(e) => log::warn!(
                                        "[hotplug] {} wireless re-init error: {e}",
                                        dev_arc.id()
                                    ),
                                }
                            }
                            // All attempts failed — remove and close so engines stop writing
                            // to a dead transport. The receiver still holds the Arc, so the
                            // device can be re-added when the next 0x41 "came online" fires.
                            {
                                let mut devs = app2.devices.write().await;
                                devs.retain(|d| !Arc::ptr_eq(d, &dev_arc));
                            }
                            dev_arc.close().await;
                            broadcast_state(&app2).await;
                            notify::send(
                                &app2,
                                halod_shared::types::NotificationCode::WirelessReinitFailed {
                                    device: dev_arc.id().to_string(),
                                },
                            )
                            .await;
                            Ok::<_, anyhow::Error>(())
                        }
                        .await
                        {
                            log::error!("[hotplug] wireless re-init task failed: {e}");
                        }
                    });
                    broadcast_state(&app).await;
                } else {
                    // No wireless fallback — close and remove.
                    app.devices.write().await.retain(|d| !Arc::ptr_eq(d, &dev));
                    dev.close().await;
                    log::info!(
                        "Hotplug: wired key {key} gone, {} had no wireless fallback, removed",
                        dev.id()
                    );
                    broadcast_state(&app).await;
                }
            }
        }
        None => {}
    }
}

/// Checks whether `new_device` should be adopted by an existing wireless device as
/// its direct transport.  Returns `true` and inserts a `WiredOverride` tracking entry
/// when adoption succeeds; the caller should skip primary registration in that case.
pub(crate) async fn try_connect_direct(
    app: &Arc<AppState>,
    new_device: &Arc<dyn Device>,
    path: &str,
    pid: u16,
    key: String,
) -> bool {
    let Some(hw_serial) = new_device.hardware_serial() else {
        return false;
    };
    if new_device.as_transport_switchable().is_none() {
        return false;
    }

    let sibling = app
        .devices
        .read()
        .await
        .iter()
        .find(|d| {
            d.hardware_serial().as_deref() == Some(hw_serial.as_str())
                && d.as_transport_switchable().is_some()
                && !Arc::ptr_eq(d, new_device)
        })
        .cloned();

    if let Some(existing) = sibling {
        if let Some(switchable) = existing.as_transport_switchable() {
            match switchable.connect_direct(path, pid, app).await {
                Ok(()) => {
                    app.hid
                        .track(
                            key.clone(),
                            HidTrackingEntry::WiredOverride(Arc::clone(&existing)),
                        )
                        .await;
                    log::info!(
                        "Hotplug: {} adopted direct transport (key {key})",
                        existing.id()
                    );
                    return true;
                }
                Err(e) => log::error!("Hotplug: connect_direct failed for {}: {e}", existing.id()),
            }
        }
    }
    false
}

/// Creates, initializes, and registers one HID device (plus any hub children).
///
/// If an existing device with the same `hardware_serial()` implements `TransportSwitchable`
/// it adopts this HID path as its wired transport (Arc identity is preserved for engines).
/// Otherwise the new device is registered as a brand-new `Primary` entry.
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
    // scoped-specific (a plugin HID device always registers as a new primary).
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

    if scoped {
        // Evict a stale native device the plugin now shadows: its id differs, so
        // dedup won't, and both would otherwise bind the same hardware.
        if impl_.owning_plugin_id().is_some()
            && crate::registry::discovery::has_native_match(&handle)
        {
            if let Some(native) = crate::registry::discovery::make_device_native_only(handle) {
                let native_id = native.id().to_owned();
                native.close().await;
                crate::registry::usecases::registration::unregister_device_and_children(
                    app, &native_id,
                )
                .await;
            }
        }
    } else if try_connect_direct(app, &impl_, path, pid, key.clone()).await {
        return;
    }

    // Register as a new primary device via the centralised registration lifecycle.
    let registered =
        crate::registry::usecases::registration::register_device(app, impl_.clone()).await;
    if !registered {
        return;
    }

    if let Some(ctrl) = impl_.as_controller() {
        for child in ctrl.discover_children().await {
            crate::registry::usecases::registration::register_device(app, child).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        drivers::{CapabilityRef, TransportSwitchable},
    };
    use async_trait::async_trait;
    use halod_shared::types::ConnectionType;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    // pick_hid_devices iterates the real global DeviceDescriptor registry, so
    // these tests rely on the registered G560 descriptor (046D:0A78, interface 2).
    fn g560_entry(
        path: &str,
        iface: i32,
        usage_page: u16,
        usage: u16,
        serial: &str,
    ) -> HidDeviceInfo {
        HidDeviceInfo {
            vid: 0x046D,
            pid: 0x0A78,
            path: path.into(),
            iface,
            serial: serial.into(),
            usage_page,
            usage,
        }
    }

    // For devices_to_register tests, which operate on already-picked slices and
    // so don't need a descriptor match.
    fn fake_entry(path: &str, serial: &str) -> HidDeviceInfo {
        HidDeviceInfo {
            vid: 0x046D,
            pid: 0x0A78,
            path: path.into(),
            iface: 0,
            serial: serial.into(),
            usage_page: 0,
            usage: 0,
        }
    }

    #[test]
    fn picker_prefers_declared_collection() {
        // Windows: interface 2 splits into collections — only the G560 vendor
        // collection (FF43/0202) matches; the bogus one is filtered out.
        let entries = [
            g560_entry("bogus", 2, 0xFF00, 0x0001, "S1"),
            g560_entry("vendor", 2, 0xFF43, 0x0202, "S1"),
        ];
        let app = make_app();
        let picked = pick_hid_devices(&app.registry, &entries);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].path, "vendor");
    }

    #[test]
    fn picker_falls_back_to_first_on_linux_single_node() {
        // Linux hidraw: one node, usage 0/0 — accepted as the G560 fallback.
        let entries = [g560_entry("hidraw3", 2, 0, 0, "S1")];
        let app = make_app();
        let picked = pick_hid_devices(&app.registry, &entries);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].path, "hidraw3");
    }

    #[test]
    fn picker_falls_back_when_preferred_absent() {
        // Both entries match; the picker takes the first.
        let entries = [
            g560_entry("a", 2, 0xFF43, 0x0202, "S1"),
            g560_entry("b", 2, 0xFF43, 0x0202, "S1"),
        ];
        let app = make_app();
        let picked = pick_hid_devices(&app.registry, &entries);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].path, "a");
    }

    #[test]
    fn picker_resolves_two_devices_independently() {
        // Two physical G560s: each resolves to its own writable collection.
        // Groups form in the order of their first *matching* entry.
        let entries = [
            g560_entry("bogus_A", 2, 0xFF00, 0x0001, "AAA"), // no match
            g560_entry("bogus_B", 2, 0xFF00, 0x0001, "BBB"), // no match
            g560_entry("vendor_B", 2, 0xFF43, 0x0202, "BBB"), // matches — BBB group forms here
            g560_entry("vendor_A", 2, 0xFF43, 0x0202, "AAA"), // matches — AAA group forms here
        ];
        let app = make_app();
        let picked = pick_hid_devices(&app.registry, &entries);
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].path, "vendor_B"); // BBB group first matching entry
        assert_eq!(picked[1].path, "vendor_A"); // AAA group second
    }

    #[test]
    fn picker_respects_interface_filter() {
        // The G560 descriptor requires interface 2, so the iface-0 entry is
        // rejected and the iface-2 entry kept.
        let entries = [
            g560_entry("iface0", 0, 0xFF43, 0x0202, "S1"),
            g560_entry("iface2", 2, 0, 0, "S1"),
        ];
        let app = make_app();
        let picked = pick_hid_devices(&app.registry, &entries);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].path, "iface2");
    }

    #[test]
    fn picker_recognizes_a_plugin_only_device_with_no_native_descriptor() {
        let app = make_app();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugin_only_hid");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: plugin_only_hid\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 0x1E71\n    pid: 0x3012\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
        app.registry.load_all(tmp.path());

        let entry = HidDeviceInfo {
            vid: 0x1E71,
            pid: 0x3012, // plugin-only PID, no native descriptor
            path: "kraken".into(),
            iface: 0,
            serial: "S1".into(),
            usage_page: 0,
            usage: 0,
        };
        let entries = [entry];
        let picked = pick_hid_devices(&app.registry, &entries);
        assert_eq!(
            picked.len(),
            1,
            "a plugin-only HID device must survive the discovery pre-filter"
        );
    }

    #[test]
    fn devices_to_register_all_new_get_sequential_indices() {
        let a = fake_entry("pathA", "AAA");
        let b = fake_entry("pathB", "BBB");
        let picked = vec![&a, &b];
        let out = devices_to_register(&picked, &HashSet::new());
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].0.path.as_str(), out[0].1), ("pathA", 0));
        assert_eq!((out[1].0.path.as_str(), out[1].1), ("pathB", 1));
    }

    #[test]
    fn devices_to_register_skips_tracked_device() {
        // An already-tracked device must be skipped — re-registering it would
        // spawn a rival listener.
        let a = fake_entry("pathA", "AAA");
        let b = fake_entry("pathB", "BBB");
        let picked = vec![&a, &b];
        let mut tracked = HashSet::new();
        tracked.insert(hid_key(0x046D, 0x0A78, "AAA"));

        let out = devices_to_register(&picked, &tracked);
        assert_eq!(out.len(), 1, "tracked device must not be registered again");
        // Index continues past the tracked device of the same vid/pid.
        assert_eq!((out[0].0.path.as_str(), out[0].1), ("pathB", 1));
    }

    /// Minimal device that implements `TransportSwitchable` using atomic state,
    /// so tests can exercise hotplug logic without real HID hardware.
    struct MockSwitchable {
        id: &'static str,
        hw_serial: Option<&'static str>,
        is_wired: AtomicBool,
        /// Whether `disconnect_direct` should succeed (i.e. a wireless fallback exists).
        has_wireless_fallback: AtomicBool,
        adopt_called: AtomicBool,
        close_called: AtomicBool,
        initialize_count: AtomicU32,
    }

    impl MockSwitchable {
        fn wireless(id: &'static str, serial: &'static str) -> Arc<Self> {
            Arc::new(Self {
                id,
                hw_serial: Some(serial),
                is_wired: AtomicBool::new(false),
                has_wireless_fallback: AtomicBool::new(false),
                adopt_called: AtomicBool::new(false),
                close_called: AtomicBool::new(false),
                initialize_count: AtomicU32::new(0),
            })
        }

        fn wired(id: &'static str, serial: &'static str) -> Arc<Self> {
            Arc::new(Self {
                id,
                hw_serial: Some(serial),
                is_wired: AtomicBool::new(true),
                has_wireless_fallback: AtomicBool::new(true),
                adopt_called: AtomicBool::new(false),
                close_called: AtomicBool::new(false),
                initialize_count: AtomicU32::new(0),
            })
        }
    }

    #[async_trait]
    impl Device for MockSwitchable {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.id
        }
        fn vendor(&self) -> &str {
            "Test"
        }
        fn model(&self) -> &str {
            self.id
        }
        async fn initialize(&self) -> anyhow::Result<bool> {
            self.initialize_count.fetch_add(1, Ordering::Relaxed);
            Ok(true)
        }
        async fn close(&self) {
            self.close_called.store(true, Ordering::Relaxed);
        }
        fn hardware_serial(&self) -> Option<String> {
            self.hw_serial.map(String::from)
        }
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::TransportSwitchable(self)]
        }
        async fn wire_connection_type(&self) -> Option<ConnectionType> {
            if self.is_wired.load(Ordering::Relaxed) {
                Some(ConnectionType::Wired)
            } else {
                Some(ConnectionType::Wireless)
            }
        }
    }

    #[async_trait]
    impl TransportSwitchable for MockSwitchable {
        async fn connect_direct(
            &self,
            _path: &str,
            _pid: u16,
            _app: &Arc<AppState>,
        ) -> anyhow::Result<()> {
            self.adopt_called.store(true, Ordering::Relaxed);
            self.is_wired.store(true, Ordering::Relaxed);
            self.has_wireless_fallback.store(true, Ordering::Relaxed);
            Ok(())
        }
        async fn disconnect_direct(&self) -> bool {
            if self.has_wireless_fallback.load(Ordering::Relaxed) {
                self.is_wired.store(false, Ordering::Relaxed);
                true
            } else {
                false
            }
        }
        async fn is_direct(&self) -> bool {
            self.is_wired.load(Ordering::Relaxed)
        }
    }

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    // The broadcast-path `hid_tracked_ids` cache mirrors the tracking map across
    // track/untrack, so the serializer never has to lock the map itself.
    #[tokio::test]
    async fn hid_tracked_ids_cache_tracks_map() {
        let app = make_app();
        let dev = MockSwitchable::wireless("kbd", "DEAD0001");
        let dev_arc: Arc<dyn Device> = Arc::clone(&dev) as Arc<dyn Device>;

        assert!(app.hid.tracked_ids().await.is_empty());

        app.hid
            .track(
                "046d:c000:DEAD0001".to_string(),
                HidTrackingEntry::Primary(vec![Arc::clone(&dev_arc)]),
            )
            .await;
        assert!(app.hid.tracked_ids().await.contains("kbd"));

        app.hid.untrack("046d:c000:DEAD0001").await;
        assert!(app.hid.tracked_ids().await.is_empty());
    }

    // When a WiredOverride key disappears, the device stays in app.devices (Arc
    // identity preserved) but reverts to wireless mode.
    #[tokio::test]
    async fn wired_disconnects_device_stays_and_reverts() {
        let app = make_app();
        let dev = MockSwitchable::wired("mouse_wired", "AABB1122");

        let dev_arc: Arc<dyn Device> = Arc::clone(&dev) as Arc<dyn Device>;
        app.devices.write().await.push(Arc::clone(&dev_arc));
        app.hid
            .track(
                "c095:0000:wired".to_string(),
                HidTrackingEntry::WiredOverride(Arc::clone(&dev_arc)),
            )
            .await;

        let ptr_before = Arc::as_ptr(&dev_arc);

        handle_hid_key_removed(Arc::clone(&app), "c095:0000:wired".to_string()).await;

        let devices = app.devices.read().await;
        assert_eq!(devices.len(), 1, "device should still be in app.devices");
        assert_eq!(
            Arc::as_ptr(&devices[0]),
            ptr_before,
            "Arc identity must be preserved"
        );
        assert!(
            !dev.is_wired.load(Ordering::Relaxed),
            "device should have reverted to wireless"
        );
        assert!(
            !dev.close_called.load(Ordering::Relaxed),
            "close must not be called during revert"
        );
    }

    // A wired key for an already-registered wireless device adopts onto the
    // existing Arc and inserts a WiredOverride entry, skipping primary registration.
    #[tokio::test]
    async fn wireless_in_state_wired_appears_adopts_existing_arc() {
        let app = make_app();
        let wireless_dev = MockSwitchable::wireless("mouse_wireless", "AABB1122");
        let wireless_arc: Arc<dyn Device> = Arc::clone(&wireless_dev) as Arc<dyn Device>;
        app.devices.write().await.push(Arc::clone(&wireless_arc));

        let ptr_before = Arc::as_ptr(&wireless_arc);

        // What the factory would produce for the wired HID key.
        let new_wired_dev = MockSwitchable::wired("mouse_wired", "AABB1122");
        let new_wired_arc: Arc<dyn Device> = Arc::clone(&new_wired_dev) as Arc<dyn Device>;

        let adopted = try_connect_direct(
            &app,
            &new_wired_arc,
            "/dev/hidraw9",
            0xC095,
            "c095:0000:wired".to_string(),
        )
        .await;

        assert!(adopted, "adoption should succeed");
        assert!(
            wireless_dev.adopt_called.load(Ordering::Relaxed),
            "connect_direct must be called on the existing wireless device"
        );
        assert!(
            wireless_dev.is_wired.load(Ordering::Relaxed),
            "existing device should now be in wired mode"
        );

        let entry = app
            .hid
            .get("c095:0000:wired")
            .await
            .expect("WiredOverride entry must exist");
        let tracked_ptr = match &entry {
            HidTrackingEntry::WiredOverride(d) => Arc::as_ptr(d),
            _ => panic!("expected WiredOverride"),
        };
        assert_eq!(
            tracked_ptr, ptr_before,
            "WiredOverride must point to the original wireless Arc"
        );
    }

    // When disconnect_direct returns false, the device is removed and closed.
    #[tokio::test]
    async fn wired_only_disconnects_device_removed() {
        let app = make_app();
        let dev = MockSwitchable::wired("wired_only", "CCDD3344");
        dev.has_wireless_fallback.store(false, Ordering::Relaxed);

        let dev_arc: Arc<dyn Device> = Arc::clone(&dev) as Arc<dyn Device>;
        app.devices.write().await.push(Arc::clone(&dev_arc));
        app.hid
            .track(
                "c095:0000:wonly".to_string(),
                HidTrackingEntry::WiredOverride(Arc::clone(&dev_arc)),
            )
            .await;

        handle_hid_key_removed(Arc::clone(&app), "c095:0000:wonly".to_string()).await;

        assert!(
            app.devices.read().await.is_empty(),
            "device must be removed when there is no wireless fallback"
        );
        assert!(
            dev.close_called.load(Ordering::Relaxed),
            "close must be called when the device is removed"
        );
    }

    // A Primary device's HID key disappearing removes and closes it.
    #[tokio::test]
    async fn primary_key_removed_device_closed_and_removed() {
        let app = make_app();
        let dev = MockSwitchable::wireless("hub", "EEFF5566");
        // The Primary tracking entry drives behavior regardless of capability.
        let dev_arc: Arc<dyn Device> = Arc::clone(&dev) as Arc<dyn Device>;
        app.devices.write().await.push(Arc::clone(&dev_arc));
        app.hid
            .track(
                "0abc:0001:primary".to_string(),
                HidTrackingEntry::Primary(vec![Arc::clone(&dev_arc)]),
            )
            .await;

        handle_hid_key_removed(Arc::clone(&app), "0abc:0001:primary".to_string()).await;

        assert!(
            app.devices.read().await.is_empty(),
            "Primary device must be removed from app.devices"
        );
        assert!(
            dev.close_called.load(Ordering::Relaxed),
            "Primary device must be closed on removal"
        );
    }

    // If the device is removed from app.devices during the retry window, the
    // WiredOverride retry task aborts instead of burning through all attempts.
    #[tokio::test]
    async fn retry_aborts_when_device_removed_from_app() {
        let app = make_app();
        let dev = MockSwitchable::wired("mouse", "AABB1122");
        let dev_arc: Arc<dyn Device> = Arc::clone(&dev) as Arc<dyn Device>;

        app.devices.write().await.push(Arc::clone(&dev_arc));
        app.hid
            .track(
                "wired_key".to_string(),
                HidTrackingEntry::WiredOverride(Arc::clone(&dev_arc)),
            )
            .await;

        // Spawns a retry task that sleeps 1 s before its first attempt.
        handle_hid_key_removed(Arc::clone(&app), "wired_key".to_string()).await;

        // Remove the device before that 1 s elapses.
        app.devices
            .write()
            .await
            .retain(|d| !Arc::ptr_eq(d, &dev_arc));

        // Wait past the first retry window.
        tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

        assert_eq!(
            dev.initialize_count.load(Ordering::Relaxed),
            0,
            "initialize must not be called after the early-abort check"
        );
        assert!(
            app.devices.read().await.is_empty(),
            "device must not be re-added to app.devices after abort"
        );
    }

    // Across a full wired → wireless cycle the Arc an engine holds stays identical
    // to the one in app.devices.
    #[tokio::test]
    async fn arc_identity_preserved_across_full_cycle() {
        let app = make_app();
        let dev = MockSwitchable::wireless("mouse", "1234ABCD");
        let dev_arc: Arc<dyn Device> = Arc::clone(&dev) as Arc<dyn Device>;

        let engine_ref = Arc::clone(&dev_arc);
        app.devices.write().await.push(Arc::clone(&dev_arc));

        // Wired key appears → adoption.
        let new_wired: Arc<dyn Device> = MockSwitchable::wired("mouse_w", "1234ABCD");
        try_connect_direct(&app, &new_wired, "/dev/hidraw0", 0xC095, "k".to_string()).await;

        assert!(
            Arc::ptr_eq(&engine_ref, &app.devices.read().await[0]),
            "engine Arc must still point to the same device after wired adoption"
        );

        // Wired key disappears → revert.
        handle_hid_key_removed(Arc::clone(&app), "k".to_string()).await;

        assert!(
            Arc::ptr_eq(&engine_ref, &app.devices.read().await[0]),
            "engine Arc must still point to the same device after revert to wireless"
        );
        assert!(
            !dev.is_wired.load(Ordering::Relaxed),
            "device should be back in wireless mode"
        );
    }

    // Removing a wired-only Primary TransportSwitchable device triggers
    // rescan_children on controllers so it can be rediscovered via its receiver.
    struct MockController {
        rescan_called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Device for MockController {
        fn id(&self) -> &str {
            "controller"
        }
        fn name(&self) -> &str {
            "Controller"
        }
        fn vendor(&self) -> &str {
            "Test"
        }
        fn model(&self) -> &str {
            "Controller"
        }
        async fn initialize(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::Controller(self)]
        }
    }

    #[async_trait]
    impl crate::drivers::Controller for MockController {
        async fn rescan_children(&self) -> Vec<Arc<dyn Device>> {
            self.rescan_called.store(true, Ordering::Relaxed);
            vec![]
        }
    }

    #[tokio::test]
    async fn primary_switchable_removed_triggers_controller_rescan() {
        let app = make_app();

        // Wired-only keyboard: Primary entry, no wireless fallback.
        let keyboard = MockSwitchable::wired("keyboard", "DEAD1234");
        keyboard
            .has_wireless_fallback
            .store(false, Ordering::Relaxed);
        let kb_arc: Arc<dyn Device> = Arc::clone(&keyboard) as Arc<dyn Device>;
        app.devices.write().await.push(Arc::clone(&kb_arc));
        app.hid
            .track(
                "046d:c352:DEAD1234".to_string(),
                HidTrackingEntry::Primary(vec![Arc::clone(&kb_arc)]),
            )
            .await;

        let rescan_called = Arc::new(AtomicBool::new(false));
        let ctrl = Arc::new(MockController {
            rescan_called: Arc::clone(&rescan_called),
        });
        let ctrl_arc: Arc<dyn Device> = Arc::clone(&ctrl) as Arc<dyn Device>;
        app.devices.write().await.push(Arc::clone(&ctrl_arc));

        handle_hid_key_removed(Arc::clone(&app), "046d:c352:DEAD1234".to_string()).await;

        assert!(
            !app.devices
                .read()
                .await
                .iter()
                .any(|d| Arc::ptr_eq(d, &kb_arc)),
            "wired keyboard must be removed from app.devices"
        );

        // The rescan task sleeps 2 s before calling rescan_children.
        tokio::time::sleep(tokio::time::Duration::from_millis(2500)).await;

        assert!(
            rescan_called.load(Ordering::Relaxed),
            "rescan_children must be called on the controller after a TransportSwitchable Primary is removed"
        );
    }
}
