use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::{
    discovery::{make_device, DeviceDescriptor, DiscoveryHandle, TransportScanner},
    drivers::{transports::Transport, Device},
    ipc::broadcast_state,
    notify,
    state::{AppState, HidTrackingEntry},
};

struct SendHidDevice(hidapi::HidDevice);
// SAFETY: hidapi::HidDevice is not Send because it stores raw pointers, but we
// only ever access it from within spawn_blocking closures that hold the Mutex.
unsafe impl Send for SendHidDevice {}
unsafe impl Sync for SendHidDevice {}

fn build_frame(data: &[u8], report_size: Option<usize>) -> Vec<u8> {
    let Some(report_size) = report_size else {
        return data.to_vec();
    };
    let mut payload = data.to_vec();
    if cfg!(target_os = "windows") {
        let target = report_size + 1;
        if payload.len() < target {
            payload.resize(target, 0x00);
        }
        payload
    } else {
        if payload.len() < report_size {
            payload.resize(report_size, 0x00);
        }
        let mut out = Vec::with_capacity(report_size + 1);
        out.push(0x00);
        out.extend_from_slice(&payload[..report_size]);
        out
    }
}

/// HID++ long-report report ID. See `route_to_long`.
// TODO: move this to logitech protocol
const HIDPP_LONG_REPORT_ID: u8 = 0x11;

/// Returns true when a packet with `report_id` must use the long-report handle.
///
/// Only HID++ long reports (`0x11`) route to the long handle, and only when one
/// was opened (`has_long`). Every other report — and every single-handle device
/// — uses the short handle. This is the single cross-platform routing rule: on
/// Linux `has_long` is always false, so everything goes through one handle.
// TODO: move this to logitech protocol
fn route_to_long(report_id: u8, has_long: bool) -> bool {
    has_long && report_id == HIDPP_LONG_REPORT_ID
}

/// One HID collection: two file descriptors for the same device path.
///
/// On Linux, hidraw supports concurrent read and write on independent fds.
/// Keeping them separate means the listener's `read_timeout` (which holds
/// `read_dev` for up to `timeout_ms`) never blocks writes, which is critical
/// for high-frequency per-key RGB frames.
#[derive(Clone)]
struct HidIo {
    read_dev: Arc<Mutex<SendHidDevice>>,
    write_dev: Arc<Mutex<SendHidDevice>>,
}

impl HidIo {
    /// Open `path` twice — separate read/write fds.
    fn open(api: &hidapi::HidApi, path: &str) -> Result<Self> {
        let cpath = std::ffi::CString::new(path)?;
        let read_dev = api
            .open_path(cpath.as_c_str())
            .with_context(|| format!("failed to open HID device (read) at {path}"))?;
        let write_dev = api
            .open_path(cpath.as_c_str())
            .with_context(|| format!("failed to open HID device (write) at {path}"))?;
        read_dev
            .set_blocking_mode(false)
            .context("failed to set non-blocking mode")?;
        write_dev
            .set_blocking_mode(false)
            .context("failed to set non-blocking mode")?;
        Ok(Self {
            read_dev: Arc::new(Mutex::new(SendHidDevice(read_dev))),
            write_dev: Arc::new(Mutex::new(SendHidDevice(write_dev))),
        })
    }
}

/// Write one already-framed packet, via a feature report or an output report.
fn write_one(dev: &hidapi::HidDevice, pkt: &[u8], use_feature_report: bool) -> Result<()> {
    let result = if use_feature_report {
        dev.send_feature_report(pkt)
            .map_err(|e| anyhow::anyhow!("HID feature write error: {}", e))
    } else {
        match dev.write(pkt) {
            Ok(0) => Err(anyhow::anyhow!("HID write returned 0 bytes")),
            Ok(_) => Ok(()),
            Err(e) => Err(anyhow::anyhow!("HID write error: {}", e)),
        }
    };
    if let Err(ref e) = result {
        // Debug, not warn: the error is returned to the caller, which decides
        // how to surface it. A disconnected device fails every write, so a
        // warn here would flood the log until hotplug removes the device.
        log::debug!(
            "[HidTransport] {} (feature_report={}, {} bytes: {:02x?})",
            e,
            use_feature_report,
            pkt.len(),
            pkt
        );
    }
    result
}

/// Write a batch of already-framed packets back-to-back under a single lock.
async fn write_batch(
    dev: Arc<Mutex<SendHidDevice>>,
    packets: Vec<Vec<u8>>,
    use_feature_report: bool,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let guard = dev.blocking_lock();
        for pkt in &packets {
            write_one(&guard.0, pkt, use_feature_report)?;
        }
        Ok(())
    })
    .await
    .context("spawn_blocking panicked")?
}

/// Read one packet from `dev` with `timeout_ms`. Empty vec on timeout.
async fn read_io(dev: Arc<Mutex<SendHidDevice>>, size: usize, timeout_ms: i32) -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let guard = dev.blocking_lock();
        let mut buf = vec![0u8; size];
        let n = guard
            .0
            .read_timeout(&mut buf, timeout_ms)
            .map_err(|e| anyhow::anyhow!("HID read error: {}", e))?;
        buf.truncate(n);
        Ok(buf)
    })
    .await
    .context("spawn_blocking panicked")?
}

/// Async HID transport with optional platform-aware padding.
///
/// Holds a `short` handle and, for HID++ devices that Windows splits into two
/// collections, an optional `long` handle. Writes route by report ID
/// (`route_to_long`); single-handle devices and Linux always use `short`.
#[derive(Clone)]
pub struct HidTransport {
    short: HidIo,
    long: Option<HidIo>,
    report_size: Option<usize>,
    timeout_ms: i32,
    /// When true, writes use `send_feature_report()` (ioctl HIDIOCSFEATURE) instead of
    /// `write()` (output report). Logitech HID++ devices — both receiver-based and
    /// wired — use output reports (`false`); the flag exists for devices whose
    /// vendor interface only accepts feature reports.
    use_feature_report: bool,
}

impl HidTransport {
    /// Open a HID device by path.
    ///
    /// `report_size` controls framing:
    /// - `None`    — raw passthrough; no padding, no prepended report-ID byte.
    ///               Used by the HID++ flow where the protocol layer builds full frames.
    /// - `Some(N)` — platform-aware padding:
    ///               Linux:   prepend `0x00` then pad payload to `N` bytes → `N+1` bytes written.
    ///               Windows: pad to `N+1` total (caller's first byte is the report ID).
    ///
    /// `use_feature_report`: `true` only for devices whose vendor interface accepts feature reports but not output reports;
    pub fn open(
        path: &str,
        report_size: Option<usize>,
        timeout_ms: i32,
        use_feature_report: bool,
    ) -> Result<Self> {
        let api = hidapi::HidApi::new().context("failed to create HidApi")?;
        let short = HidIo::open(&api, path)?;
        Ok(Self {
            short,
            long: None,
            report_size,
            timeout_ms,
            use_feature_report,
        })
    }

    /// Open a HID++ device that Windows splits into two HID collections.
    ///
    /// `short_path` carries short reports (`0x10`); `long_path` carries long
    /// reports (`0x11`). When `long_path` is empty or equal to `short_path`
    /// — Linux hidraw exposes one node carrying both report IDs — only the
    /// short handle is opened and the transport behaves exactly like `open`.
    pub fn open_dual(
        short_path: &str,
        long_path: &str,
        report_size: Option<usize>,
        timeout_ms: i32,
        use_feature_report: bool,
    ) -> Result<Self> {
        let api = hidapi::HidApi::new().context("failed to create HidApi")?;
        let short = HidIo::open(&api, short_path)?;
        let long = if long_path.is_empty() || long_path == short_path {
            None
        } else {
            Some(HidIo::open(&api, long_path)?)
        };
        Ok(Self {
            short,
            long,
            report_size,
            timeout_ms,
            use_feature_report,
        })
    }

    /// Pick the handle a packet with `report_id` must use.
    fn pick_io(&self, report_id: u8) -> &HidIo {
        if route_to_long(report_id, self.long.is_some()) {
            self.long
                .as_ref()
                .expect("route_to_long guarantees a long handle")
        } else {
            &self.short
        }
    }

    fn frame(&self, data: &[u8]) -> Vec<u8> {
        build_frame(data, self.report_size)
    }

    /// Background task: re-enumerates HID devices every 2 seconds.
    /// Adds newly connected devices and removes disconnected ones.
    pub async fn hotplug_monitor(app: Arc<AppState>) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            let live: Vec<HidDeviceInfo> = match tokio::task::spawn_blocking(|| {
                hidapi::HidApi::new().map(|api| {
                    api.device_list()
                        .map(|i| HidDeviceInfo {
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
                        })
                        .collect()
                })
            })
            .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    log::debug!("Hotplug: HID enumeration failed: {e}");
                    continue;
                }
                Err(e) => {
                    log::debug!("Hotplug: spawn_blocking panicked: {e}");
                    continue;
                }
            };

            let live_keys: HashSet<String> = live
                .iter()
                .map(|i| hid_key(i.vid, i.pid, &i.serial))
                .collect();
            let tracked_keys: HashSet<String> = app
                .hid_device_tracking
                .lock()
                .await
                .keys()
                .cloned()
                .collect();

            // Remove devices no longer present in the HID enumeration
            for key in tracked_keys
                .difference(&live_keys)
                .cloned()
                .collect::<Vec<_>>()
            {
                handle_hid_key_removed(Arc::clone(&app), key).await;
            }

            // Re-snapshot tracking after the removals above so a key freed this
            // cycle can be re-added in the same pass.
            let tracked_keys: HashSet<String> = app
                .hid_device_tracking
                .lock()
                .await
                .keys()
                .cloned()
                .collect();

            // The picker yields one entry per physical device (vid,pid,serial),
            // already collection-resolved; `devices_to_register` then drops the
            // already-tracked ones and assigns each new device its index.
            let picked = pick_hid_devices(&live);
            for (info, idx) in devices_to_register(&picked, &tracked_keys) {
                let serial_opt = if info.serial.is_empty() {
                    None
                } else {
                    Some(info.serial.as_str())
                };
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
}

#[async_trait]
impl Transport for HidTransport {
    async fn write(&self, data: &[u8]) -> Result<()> {
        let framed = self.frame(data);
        let report_id = framed.first().copied().unwrap_or(0);
        let dev = Arc::clone(&self.pick_io(report_id).write_dev);
        let use_feature_report = self.use_feature_report;
        tokio::task::spawn_blocking(move || {
            write_one(&dev.blocking_lock().0, &framed, use_feature_report)
        })
        .await
        .context("spawn_blocking panicked")?
    }

    async fn read(&self, size: usize) -> Result<Vec<u8>> {
        read_io(Arc::clone(&self.short.read_dev), size, self.timeout_ms).await
    }

    async fn write_then_read(&self, data: &[u8], size: usize) -> Result<Vec<u8>> {
        let framed = self.frame(data);
        let report_id = framed.first().copied().unwrap_or(0);
        let timeout_ms = self.timeout_ms;
        let use_feature_report = self.use_feature_report;
        let io = self.pick_io(report_id);
        let write_dev = Arc::clone(&io.write_dev);
        let read_dev = Arc::clone(&io.read_dev);
        tokio::task::spawn_blocking(move || {
            let wguard = write_dev.blocking_lock();
            write_one(&wguard.0, &framed, use_feature_report)?;
            drop(wguard);
            let rguard = read_dev.blocking_lock();
            let mut buf = vec![0u8; size];
            let n = rguard
                .0
                .read_timeout(&mut buf, timeout_ms)
                .map_err(|e| anyhow::anyhow!("HID read error: {}", e))?;
            buf.truncate(n);
            Ok(buf)
        })
        .await
        .context("spawn_blocking panicked")?
    }

    async fn write_many(&self, packets: &[Vec<u8>]) -> Result<()> {
        let has_long = self.long.is_some();
        // Partition framed packets by destination handle. Order is preserved
        // within each handle; per-key RGB batches are all long reports so they
        // stay in a single group.
        let mut short_pkts: Vec<Vec<u8>> = Vec::new();
        let mut long_pkts: Vec<Vec<u8>> = Vec::new();
        for p in packets {
            let framed = self.frame(p);
            let report_id = framed.first().copied().unwrap_or(0);
            if route_to_long(report_id, has_long) {
                long_pkts.push(framed);
            } else {
                short_pkts.push(framed);
            }
        }
        if !short_pkts.is_empty() {
            write_batch(
                Arc::clone(&self.short.write_dev),
                short_pkts,
                self.use_feature_report,
            )
            .await?;
        }
        if !long_pkts.is_empty() {
            if let Some(long) = &self.long {
                write_batch(
                    Arc::clone(&long.write_dev),
                    long_pkts,
                    self.use_feature_report,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Send a feature report and read back the response via `get_feature_report`.
    ///
    /// Used by drivers whose vendor interface responds to
    /// `HIDIOCSFEATURE` with data readable via `HIDIOCGFEATURE` rather than
    /// echoing the reply on the interrupt-IN endpoint.  Both ioctl calls share
    /// the same fd so they are executed atomically under the write lock with a
    /// 1 ms gap to let the device process the command.
    async fn feature_exchange(&self, data: &[u8], response_size: usize) -> Result<Vec<u8>> {
        let framed = self.frame(data);
        let dev = Arc::clone(&self.short.write_dev);
        tokio::task::spawn_blocking(move || {
            let guard = dev.blocking_lock();
            guard
                .0
                .send_feature_report(&framed)
                .map_err(|e| anyhow::anyhow!("feature write error: {}", e))?;
            std::thread::sleep(std::time::Duration::from_millis(1));
            let mut buf = vec![0u8; response_size + 1];
            buf[0] = 0x00; // report ID
            let n = guard
                .0
                .get_feature_report(&mut buf)
                .map_err(|e| anyhow::anyhow!("feature read error: {}", e))?;
            buf.truncate(n);
            Ok(buf)
        })
        .await
        .context("spawn_blocking panicked")?
    }

    async fn read_nonblocking(&self, size: usize) -> Result<Vec<u8>> {
        read_io(Arc::clone(&self.short.read_dev), size, 0).await
    }

    /// Read one packet from the long-report handle.
    ///
    /// Returns an empty vec when no long handle is open (single-handle / Linux);
    /// callers must guard with `has_long_handle` to avoid a tight spin loop.
    async fn read_long(&self, size: usize) -> Result<Vec<u8>> {
        match &self.long {
            Some(long) => read_io(Arc::clone(&long.read_dev), size, self.timeout_ms).await,
            None => Ok(Vec::new()),
        }
    }

    fn has_long_handle(&self) -> bool {
        self.long.is_some()
    }
}

async fn discover(app: Arc<AppState>) -> Result<()> {
    let api = match hidapi::HidApi::new() {
        Ok(api) => api,
        Err(e) => {
            log::error!("Failed to initialize HIDAPI: {}", e);
            return Ok(());
        }
    };

    let entries: Vec<HidDeviceInfo> = api
        .device_list()
        .map(|i| HidDeviceInfo {
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
        })
        .collect();

    // Discovery runs at startup and on every manual "Scan now"; skip HID
    // devices already registered so a rescan never opens a second handle to
    // hardware that is already in use (which would spawn a rival listener
    // and corrupt HID++ messaging for both).
    let tracked_keys: HashSet<String> = app
        .hid_device_tracking
        .lock()
        .await
        .keys()
        .cloned()
        .collect();
    let picked = pick_hid_devices(&entries);
    for (info, idx) in devices_to_register(&picked, &tracked_keys) {
        log::debug!(
            "Checking HID {:04x}:{:04x} path={} iface={}",
            info.vid,
            info.pid,
            &info.path,
            info.iface
        );
        let serial = if info.serial.is_empty() {
            None
        } else {
            Some(info.serial.as_str())
        };
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

/// Resolve enumerated HID entries to one entry per physical device,
/// keyed by `(vid, pid, serial)`.
///
/// Uses `DeviceDescriptor::matches` to filter entries. When multiple entries
/// exist for one physical device (Windows HID collections), the one whose
/// usage_page/usage satisfies a descriptor's `matches()` is preferred; otherwise
/// the first entry in the group wins. Result order follows enumeration
/// first-occurrence so device `idx` assignment stays stable.
fn pick_hid_devices<'a>(entries: &'a [HidDeviceInfo]) -> Vec<&'a HidDeviceInfo> {
    let mut order: Vec<(u16, u16, String)> = Vec::new();
    let mut groups: HashMap<(u16, u16, String), Vec<&HidDeviceInfo>> = HashMap::new();

    for e in entries {
        // Build a probe handle to check if any descriptor matches this entry.
        let probe = DiscoveryHandle::Hid {
            vid: e.vid,
            pid: e.pid,
            path: "",
            serial: None,
            idx: 0,
            usage_page: e.usage_page,
            usage: e.usage,
            interface_number: Some(e.iface),
        };
        if inventory::iter::<DeviceDescriptor>().all(|d| !(d.matches)(&probe)) {
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
            // Among candidates for this physical device, pick the one whose
            // usage_page/usage actually satisfies a descriptor's matches() —
            // important on Windows where one USB interface splits into several
            // collections.
            candidates
                .iter()
                .copied()
                .find(|e| {
                    let probe = DiscoveryHandle::Hid {
                        vid: e.vid,
                        pid: e.pid,
                        path: "",
                        serial: None,
                        idx: 0,
                        usage_page: e.usage_page,
                        usage: e.usage,
                        interface_number: Some(e.iface),
                    };
                    inventory::iter::<DeviceDescriptor>().any(|d| (d.matches)(&probe))
                })
                .unwrap_or(candidates[0])
        })
        .collect()
}

fn hid_key(vid: u16, pid: u16, serial: &str) -> String {
    format!("{vid:04x}:{pid:04x}:{serial}")
}

/// From the picker's output, the entries that are not already tracked, each
/// tagged with the device index it should receive.
///
/// `discover` (startup + every manual "Scan now") and `hotplug_monitor` both
/// re-enumerate every HID device. Without this filter a rediscovery pass would
/// register a *second* `Device` for hardware that is already open — for the
/// Logitech receiver that means a duplicate HID++ listener fighting the original
/// for incoming reports, which corrupts messaging for both. Indices continue past
/// the already-tracked devices of the same `(vid, pid)` so device IDs stay unique
/// and monotonic across passes.
fn devices_to_register<'a>(
    picked: &[&'a HidDeviceInfo],
    tracked_keys: &HashSet<String>,
) -> Vec<(&'a HidDeviceInfo, usize)> {
    // Seed idx counters from already-tracked devices so index stays monotonic.
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
    let entry = app.hid_device_tracking.lock().await.remove(&key);
    match entry {
        Some(HidTrackingEntry::Primary(arcs)) => {
            let to_close: Vec<Arc<dyn Device>> = {
                let mut devs = app.devices.lock().await;
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
            broadcast_state(Arc::clone(&app)).await;

            // A wired TransportSwitchable device (e.g. a Logitech keyboard connected
            // via USB) may now be available through its paired wireless receiver. The
            // receiver's pairing table only shows the slot once the cable is gone, so
            // trigger a slot rescan on every registered controller after a short delay.
            let any_switchable = to_close
                .iter()
                .any(|d| d.as_transport_switchable().is_some());
            if any_switchable {
                let app2 = Arc::clone(&app);
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    let controllers: Vec<Arc<dyn Device>> = app2
                        .devices
                        .lock()
                        .await
                        .iter()
                        .filter(|d| d.as_controller().is_some())
                        .cloned()
                        .collect();
                    for ctrl_dev in controllers {
                        if let Some(ctrl) = ctrl_dev.as_controller() {
                            ctrl.rescan_children(Arc::clone(&app2)).await;
                        }
                    }
                });
            }
        }
        Some(HidTrackingEntry::WiredOverride(dev)) => {
            // Wired path gone — try reverting to wireless transport.
            // The device Arc stays in app.devices throughout.
            if let Some(switchable) = dev.as_transport_switchable() {
                if switchable.revert_to_wireless().await {
                    log::info!(
                        "Hotplug: wired key {key} gone, {} reverted to wireless",
                        dev.id()
                    );
                    let dev_arc = Arc::clone(&dev);
                    let app2 = Arc::clone(&app);
                    tokio::spawn(async move {
                        for attempt in 0..6u8 {
                            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                            // Abort if the Primary handler already removed this device
                            // (e.g. receiver disconnected in the same hotplug cycle).
                            if !app2
                                .devices
                                .lock()
                                .await
                                .iter()
                                .any(|d| Arc::ptr_eq(d, &dev_arc))
                            {
                                log::info!(
                                    "[hotplug] {} no longer registered; wireless re-init aborted",
                                    dev_arc.id()
                                );
                                return;
                            }
                            match dev_arc.initialize().await {
                                Ok(true) => {
                                    // Re-apply persisted settings
                                    let saved_state = {
                                        let cfg = app2.config.read().await;
                                        cfg.active_profile_data()
                                            .device_states
                                            .get(&dev_arc.id())
                                            .cloned()
                                    };
                                    if let Some(state) = saved_state {
                                        dev_arc.load_state(&state).await;
                                    }
                                    log::info!(
                                        "[hotplug] {} re-initialized on wireless (attempt {})",
                                        dev_arc.id(),
                                        attempt + 1
                                    );
                                    broadcast_state(Arc::clone(&app2)).await;
                                    return;
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
                        // All 6 attempts failed — remove and close the device so engines
                        // stop writing to a transport that no longer responds.
                        // The receiver's logitech_devices still holds the Arc so the
                        // device can be re-added when the next 0x41 "came online" fires.
                        {
                            let mut devs = app2.devices.lock().await;
                            devs.retain(|d| !Arc::ptr_eq(d, &dev_arc));
                        }
                        dev_arc.close().await;
                        broadcast_state(Arc::clone(&app2)).await;
                        notify::warn(
                            &app2,
                            "Device did not return to wireless",
                            format!(
                                "{} did not come online after 6 wireless re-init attempts.",
                                dev_arc.id()
                            ),
                        )
                        .await;
                    });
                    broadcast_state(Arc::clone(&app)).await;
                } else {
                    // No wireless fallback — close and remove
                    app.devices.lock().await.retain(|d| !Arc::ptr_eq(d, &dev));
                    dev.close().await;
                    log::info!(
                        "Hotplug: wired key {key} gone, {} had no wireless fallback, removed",
                        dev.id()
                    );
                    broadcast_state(Arc::clone(&app)).await;
                }
            }
        }
        None => {}
    }
}

/// Checks whether `new_device` should be adopted by an existing wireless device as
/// its wired transport.  Returns `true` and inserts a `WiredOverride` tracking entry
/// when adoption succeeds; the caller should skip primary registration in that case.
pub(crate) async fn try_adopt_wired_transport(
    app: &Arc<AppState>,
    new_device: &Arc<dyn Device>,
    path: &str,
    pid: u16,
    key: String,
) -> bool {
    let hw_serial = match new_device.hardware_serial() {
        Some(s) => s,
        None => return false,
    };
    if new_device.as_transport_switchable().is_none() {
        return false;
    }

    let sibling = app
        .devices
        .lock()
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
            match switchable.adopt_wired_transport(path, pid).await {
                Ok(()) => {
                    app.hid_device_tracking.lock().await.insert(
                        key.clone(),
                        HidTrackingEntry::WiredOverride(Arc::clone(&existing)),
                    );
                    log::info!(
                        "Hotplug: {} adopted wired transport (key {key})",
                        existing.id()
                    );
                    return true;
                }
                Err(e) => log::error!(
                    "Hotplug: adopt_wired_transport failed for {}: {e}",
                    existing.id()
                ),
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
    let impl_ = match make_device(handle) {
        Some(d) => d,
        None => return,
    };
    let serial_key = serial.filter(|s| !s.is_empty()).unwrap_or("").to_string();
    let key = hid_key(vid, pid, &serial_key);

    if try_adopt_wired_transport(app, &impl_, path, pid, key.clone()).await {
        return;
    }

    // No transport switch — register as a new primary device via the centralised
    // lifecycle (dedup → disabled check → init → load state → push → after_register).
    let registered = crate::usecases::registration::register_device(app, impl_.clone()).await;
    if !registered {
        return;
    }
    let parent_idx = app.devices.lock().await.len() - 1;

    if let Some(ctrl) = impl_.as_controller() {
        ctrl.discover_children(app.clone()).await;
    }

    let arcs: Vec<Arc<dyn Device>> = app.devices.lock().await[parent_idx..]
        .iter()
        .cloned()
        .collect();
    app.hid_device_tracking
        .lock()
        .await
        .insert(key, HidTrackingEntry::Primary(arcs));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        drivers::{CapabilityRef, TransportSwitchable},
    };
    use async_trait::async_trait;
    use halod_protocol::types::ConnectionType;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    // ── Frame helpers ─────────────────────────────────────────────────────────

    #[test]
    fn frame_raw_passthrough() {
        assert_eq!(build_frame(&[0x10, 0x02], None), vec![0x10, 0x02]);
    }

    // Only HID++ long reports (0x11) route to the long handle, and only when
    // one is open. Everything else uses the short handle on both platforms.
    #[test]
    fn route_to_long_only_long_reports_with_long_handle() {
        assert!(
            route_to_long(0x11, true),
            "long report + long handle → long"
        );
        assert!(
            !route_to_long(0x10, true),
            "short report → short even with long handle"
        );
        assert!(!route_to_long(0x11, false), "no long handle → short");
        assert!(
            !route_to_long(0x10, false),
            "short report, no long handle → short"
        );
    }

    // The hidraw (non-Windows) branch prepends a 0x00 report-ID byte and emits
    // exactly `report_size` payload bytes, padding or truncating to fit.
    #[cfg(target_os = "linux")]
    #[test]
    fn frame_linux_short_data_pads() {
        let result = build_frame(&[0x10, 0x02], Some(4));
        assert_eq!(result, vec![0x00, 0x10, 0x02, 0x00, 0x00]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn frame_linux_exact_size() {
        let result = build_frame(&[0x10, 0x02], Some(2));
        assert_eq!(result, vec![0x00, 0x10, 0x02]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn frame_linux_long_data_truncated() {
        let result = build_frame(&[0x10, 0x02, 0x03], Some(2));
        assert_eq!(result, vec![0x00, 0x10, 0x02]);
    }

    // The Windows branch keeps the caller's report-ID byte in place and pads
    // the buffer to `report_size + 1`; it never prepends a byte or truncates.
    #[cfg(target_os = "windows")]
    #[test]
    fn frame_windows_short_data_pads() {
        let result = build_frame(&[0x10, 0x02], Some(4));
        assert_eq!(result, vec![0x10, 0x02, 0x00, 0x00, 0x00]);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn frame_windows_exact_size() {
        let result = build_frame(&[0x10, 0x02], Some(2));
        assert_eq!(result, vec![0x10, 0x02, 0x00]);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn frame_windows_long_data_kept() {
        let result = build_frame(&[0x10, 0x02, 0x03], Some(2));
        assert_eq!(result, vec![0x10, 0x02, 0x03]);
    }

    // ── Device picker ─────────────────────────────────────────────────────────
    // pick_hid_devices uses inventory::iter::<DeviceDescriptor>() globally, so
    // these tests exercise the real registered descriptors (e.g. G560 at
    // 046D:0A78, interface 2). Entries that don't match any registered descriptor
    // are filtered out.

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

    // Helper for devices_to_register tests only — uses a fake VID/PID that
    // won't match any real descriptor, so pick_hid_devices can't be used, but
    // devices_to_register operates on already-picked slices.
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
        // Windows: interface 2 splits into several collections — the G560
        // descriptor's vendor collection (usage_page=FF43, usage=0202) matches;
        // the bogus collection entry is filtered out as non-matching.
        let entries = [
            g560_entry("bogus", 2, 0xFF00, 0x0001, "S1"),
            g560_entry("vendor", 2, 0xFF43, 0x0202, "S1"),
        ];
        let picked = pick_hid_devices(&entries);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].path, "vendor");
    }

    #[test]
    fn picker_falls_back_to_first_on_linux_single_node() {
        // Linux hidraw: one node, usage 0 — the G560 descriptor accepts usage
        // 0/0 as a fallback (matches clause: usage_page==0 && usage==0).
        let entries = [g560_entry("hidraw3", 2, 0, 0, "S1")];
        let picked = pick_hid_devices(&entries);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].path, "hidraw3");
    }

    #[test]
    fn picker_falls_back_when_preferred_absent() {
        // Both entries match interface 2 for G560 but neither has the vendor
        // collection usage. The picker falls back to the first matching entry.
        let entries = [
            g560_entry("a", 2, 0xFF43, 0x0202, "S1"),
            g560_entry("b", 2, 0xFF43, 0x0202, "S1"),
        ];
        let picked = pick_hid_devices(&entries);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].path, "a");
    }

    #[test]
    fn picker_resolves_two_devices_independently() {
        // Two physical G560s (distinct serials): each resolves to its own
        // writable collection, in first-matched-entry order. Non-matching
        // entries (bogus usage page) are filtered out before group ordering,
        // so groups form in the order of their first *matching* entry.
        let entries = [
            g560_entry("bogus_A", 2, 0xFF00, 0x0001, "AAA"), // no match
            g560_entry("bogus_B", 2, 0xFF00, 0x0001, "BBB"), // no match
            g560_entry("vendor_B", 2, 0xFF43, 0x0202, "BBB"), // matches — BBB group forms here
            g560_entry("vendor_A", 2, 0xFF43, 0x0202, "AAA"), // matches — AAA group forms here
        ];
        let picked = pick_hid_devices(&entries);
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].path, "vendor_B"); // BBB group first matching entry
        assert_eq!(picked[1].path, "vendor_A"); // AAA group second
    }

    #[test]
    fn picker_respects_interface_filter() {
        // The G560 descriptor requires interface 2. An iface-0 entry with the
        // vendor collection usage is not matched; the iface-2 entry (fallback
        // usage 0/0) is matched and kept.
        let entries = [
            g560_entry("iface0", 0, 0xFF43, 0x0202, "S1"),
            g560_entry("iface2", 2, 0, 0, "S1"),
        ];
        let picked = pick_hid_devices(&entries);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].path, "iface2");
    }

    // ── devices_to_register ───────────────────────────────────────────────────

    #[test]
    fn devices_to_register_all_new_get_sequential_indices() {
        // Nothing tracked yet (startup): both devices register, indices from 0.
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
        // A manual "Scan now" re-enumerates an already-open device.
        // It must be skipped — re-registering it would spawn a rival listener.
        let a = fake_entry("pathA", "AAA");
        let b = fake_entry("pathB", "BBB");
        let picked = vec![&a, &b];
        let mut tracked = HashSet::new();
        tracked.insert(hid_key(0x046D, 0x0A78, "AAA"));

        let out = devices_to_register(&picked, &tracked);
        assert_eq!(out.len(), 1, "tracked device must not be registered again");
        // The surviving new device's index continues past the tracked one of
        // the same vid/pid so device IDs stay unique across rescans.
        assert_eq!((out[0].0.path.as_str(), out[0].1), ("pathB", 1));
    }

    // ── Mock device ───────────────────────────────────────────────────────────

    /// Minimal device that implements `TransportSwitchable` using atomic state,
    /// so tests can exercise hotplug logic without real HID hardware.
    struct MockSwitchable {
        id: &'static str,
        hw_serial: Option<&'static str>,
        /// Tracks whether the device is currently on its wired transport.
        is_wired: AtomicBool,
        /// Whether `revert_to_wireless` should succeed (i.e. a wireless fallback exists).
        has_wireless_fallback: AtomicBool,
        /// Set to true whenever `adopt_wired_transport` is called.
        adopt_called: AtomicBool,
        /// Set to true whenever `close` is called.
        close_called: AtomicBool,
        /// Counts how many times `initialize` was called.
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
                // Wired device starts with a wireless fallback available
                has_wireless_fallback: AtomicBool::new(true),
                adopt_called: AtomicBool::new(false),
                close_called: AtomicBool::new(false),
                initialize_count: AtomicU32::new(0),
            })
        }
    }

    #[async_trait]
    impl Device for MockSwitchable {
        fn id(&self) -> String {
            self.id.to_string()
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
        async fn adopt_wired_transport(&self, _path: &str, _pid: u16) -> anyhow::Result<()> {
            self.adopt_called.store(true, Ordering::Relaxed);
            self.is_wired.store(true, Ordering::Relaxed);
            // Record that a wireless fallback now exists (mirrors LogitechDevice behaviour)
            self.has_wireless_fallback.store(true, Ordering::Relaxed);
            Ok(())
        }
        async fn revert_to_wireless(&self) -> bool {
            if self.has_wireless_fallback.load(Ordering::Relaxed) {
                self.is_wired.store(false, Ordering::Relaxed);
                true
            } else {
                false
            }
        }
        async fn is_using_wired_transport(&self) -> bool {
            self.is_wired.load(Ordering::Relaxed)
        }
    }

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    // ── Test 1: wired device disconnects, reverts to wireless ─────────────────
    // The device was using a wired transport (WiredOverride entry). When the wired
    // key disappears, `revert_to_wireless` is called and the device STAYS in
    // app.devices (Arc identity preserved) but switches to wireless mode.

    #[tokio::test]
    async fn wired_disconnects_device_stays_and_reverts() {
        let app = make_app();
        let dev = MockSwitchable::wired("mouse_wired", "AABB1122");

        let dev_arc: Arc<dyn Device> = Arc::clone(&dev) as Arc<dyn Device>;
        app.devices.lock().await.push(Arc::clone(&dev_arc));
        app.hid_device_tracking.lock().await.insert(
            "c095:0000:wired".to_string(),
            HidTrackingEntry::WiredOverride(Arc::clone(&dev_arc)),
        );

        let ptr_before = Arc::as_ptr(&dev_arc);

        handle_hid_key_removed(Arc::clone(&app), "c095:0000:wired".to_string()).await;

        let devices = app.devices.lock().await;
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

    // ── Test 2: wireless device in state, wired key appears — transport adopted ─
    // The wireless device is already in app.devices. A new wired HID key appears
    // for the same physical device. `try_adopt_wired_transport` should call
    // `adopt_wired_transport` on the EXISTING Arc, insert a WiredOverride entry,
    // and return true (primary registration should be skipped).

    #[tokio::test]
    async fn wireless_in_state_wired_appears_adopts_existing_arc() {
        let app = make_app();
        let wireless_dev = MockSwitchable::wireless("mouse_wireless", "AABB1122");
        let wireless_arc: Arc<dyn Device> = Arc::clone(&wireless_dev) as Arc<dyn Device>;
        app.devices.lock().await.push(Arc::clone(&wireless_arc));

        let ptr_before = Arc::as_ptr(&wireless_arc);

        // `new_device` is what the factory would produce for the wired HID key
        let new_wired_dev = MockSwitchable::wired("mouse_wired", "AABB1122");
        let new_wired_arc: Arc<dyn Device> = Arc::clone(&new_wired_dev) as Arc<dyn Device>;

        let adopted = try_adopt_wired_transport(
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
            "adopt_wired_transport must be called on the existing wireless device"
        );
        assert!(
            wireless_dev.is_wired.load(Ordering::Relaxed),
            "existing device should now be in wired mode"
        );

        let tracking = app.hid_device_tracking.lock().await;
        let entry = tracking
            .get("c095:0000:wired")
            .expect("WiredOverride entry must exist");
        let tracked_ptr = match entry {
            HidTrackingEntry::WiredOverride(d) => Arc::as_ptr(d),
            _ => panic!("expected WiredOverride"),
        };
        assert_eq!(
            tracked_ptr, ptr_before,
            "WiredOverride must point to the original wireless Arc"
        );
    }

    // ── Test 3: WiredOverride with no wireless fallback — device removed ───────
    // If `revert_to_wireless` returns false (device was wired-only, no wireless
    // connection recorded), the device must be removed from app.devices and closed.

    #[tokio::test]
    async fn wired_only_disconnects_device_removed() {
        let app = make_app();
        let dev = MockSwitchable::wired("wired_only", "CCDD3344");
        dev.has_wireless_fallback.store(false, Ordering::Relaxed);

        let dev_arc: Arc<dyn Device> = Arc::clone(&dev) as Arc<dyn Device>;
        app.devices.lock().await.push(Arc::clone(&dev_arc));
        app.hid_device_tracking.lock().await.insert(
            "c095:0000:wonly".to_string(),
            HidTrackingEntry::WiredOverride(Arc::clone(&dev_arc)),
        );

        handle_hid_key_removed(Arc::clone(&app), "c095:0000:wonly".to_string()).await;

        assert!(
            app.devices.lock().await.is_empty(),
            "device must be removed when there is no wireless fallback"
        );
        assert!(
            dev.close_called.load(Ordering::Relaxed),
            "close must be called when the device is removed"
        );
    }

    // ── Test 4: Primary entry removed — device removed and closed ─────────────
    // A non-switchable (Primary) device's HID key disappears. It must be removed
    // from app.devices and closed.

    #[tokio::test]
    async fn primary_key_removed_device_closed_and_removed() {
        let app = make_app();
        let dev = MockSwitchable::wireless("hub", "EEFF5566");
        // Clear transport-switchable capability to simulate a plain Primary device
        // (we can just insert it as Primary; the tracking type drives behavior).
        let dev_arc: Arc<dyn Device> = Arc::clone(&dev) as Arc<dyn Device>;
        app.devices.lock().await.push(Arc::clone(&dev_arc));
        app.hid_device_tracking.lock().await.insert(
            "0abc:0001:primary".to_string(),
            HidTrackingEntry::Primary(vec![Arc::clone(&dev_arc)]),
        );

        handle_hid_key_removed(Arc::clone(&app), "0abc:0001:primary".to_string()).await;

        assert!(
            app.devices.lock().await.is_empty(),
            "Primary device must be removed from app.devices"
        );
        assert!(
            dev.close_called.load(Ordering::Relaxed),
            "Primary device must be closed on removal"
        );
    }

    // ── Test 5: retry aborts early when device removed from app.devices ──────
    // The Primary handler for the receiver may remove the device from
    // app.devices in the same hotplug cycle that triggers the WiredOverride
    // retry. The task should abort after the first sleep rather than burning
    // through all 6 attempts on a device that's already been cleaned up.

    #[tokio::test]
    async fn retry_aborts_when_device_removed_from_app() {
        let app = make_app();
        let dev = MockSwitchable::wired("mouse", "AABB1122");
        let dev_arc: Arc<dyn Device> = Arc::clone(&dev) as Arc<dyn Device>;

        app.devices.lock().await.push(Arc::clone(&dev_arc));
        app.hid_device_tracking.lock().await.insert(
            "wired_key".to_string(),
            HidTrackingEntry::WiredOverride(Arc::clone(&dev_arc)),
        );

        // Trigger the WiredOverride handler; it spawns a retry task that
        // sleeps 1 s before its first attempt.
        handle_hid_key_removed(Arc::clone(&app), "wired_key".to_string()).await;

        // Simulate the Primary handler removing the device (before 1 s elapses).
        app.devices
            .lock()
            .await
            .retain(|d| !Arc::ptr_eq(d, &dev_arc));

        // Wait past the first retry window; if abort logic is missing, one
        // initialize() call would land here.
        tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

        assert_eq!(
            dev.initialize_count.load(Ordering::Relaxed),
            0,
            "initialize must not be called after the early-abort check"
        );
        assert!(
            app.devices.lock().await.is_empty(),
            "device must not be re-added to app.devices after abort"
        );
    }

    // ── Test 6: Arc identity preserved; engine sees continuous device ──────────
    // After a full wired → wireless → wired cycle the Arc the engine holds is
    // identical to the one stored in app.devices.

    #[tokio::test]
    async fn arc_identity_preserved_across_full_cycle() {
        let app = make_app();
        let dev = MockSwitchable::wireless("mouse", "1234ABCD");
        let dev_arc: Arc<dyn Device> = Arc::clone(&dev) as Arc<dyn Device>;

        // Engines / subscribers hold a clone of the Arc
        let engine_ref = Arc::clone(&dev_arc);
        app.devices.lock().await.push(Arc::clone(&dev_arc));

        // Wired key appears → adoption
        let new_wired: Arc<dyn Device> = MockSwitchable::wired("mouse_w", "1234ABCD");
        try_adopt_wired_transport(&app, &new_wired, "/dev/hidraw0", 0xC095, "k".to_string()).await;

        assert!(
            Arc::ptr_eq(&engine_ref, &app.devices.lock().await[0]),
            "engine Arc must still point to the same device after wired adoption"
        );

        // Wired key disappears → revert
        handle_hid_key_removed(Arc::clone(&app), "k".to_string()).await;

        assert!(
            Arc::ptr_eq(&engine_ref, &app.devices.lock().await[0]),
            "engine Arc must still point to the same device after revert to wireless"
        );
        assert!(
            !dev.is_wired.load(Ordering::Relaxed),
            "device should be back in wireless mode"
        );
    }

    // ── Test 7: Primary TransportSwitchable removed → controller rescan triggered
    // When a wired-only keyboard (Primary tracking entry, TransportSwitchable) is
    // removed, controllers must have rescan_children called so the device can be
    // re-discovered through its wireless receiver.

    struct MockController {
        rescan_called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Device for MockController {
        fn id(&self) -> String {
            "controller".to_string()
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
        async fn rescan_children(&self, _app: Arc<crate::state::AppState>) {
            self.rescan_called.store(true, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn primary_switchable_removed_triggers_controller_rescan() {
        let app = make_app();

        // A wired-only keyboard — Primary entry, TransportSwitchable, no wireless fallback.
        let keyboard = MockSwitchable::wired("keyboard", "DEAD1234");
        keyboard
            .has_wireless_fallback
            .store(false, Ordering::Relaxed);
        let kb_arc: Arc<dyn Device> = Arc::clone(&keyboard) as Arc<dyn Device>;
        app.devices.lock().await.push(Arc::clone(&kb_arc));
        app.hid_device_tracking.lock().await.insert(
            "046d:c352:DEAD1234".to_string(),
            HidTrackingEntry::Primary(vec![Arc::clone(&kb_arc)]),
        );

        // A receiver controller already registered in app.devices.
        let rescan_called = Arc::new(AtomicBool::new(false));
        let ctrl = Arc::new(MockController {
            rescan_called: Arc::clone(&rescan_called),
        });
        let ctrl_arc: Arc<dyn Device> = Arc::clone(&ctrl) as Arc<dyn Device>;
        app.devices.lock().await.push(Arc::clone(&ctrl_arc));

        handle_hid_key_removed(Arc::clone(&app), "046d:c352:DEAD1234".to_string()).await;

        // Keyboard must be removed.
        assert!(
            !app.devices
                .lock()
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
