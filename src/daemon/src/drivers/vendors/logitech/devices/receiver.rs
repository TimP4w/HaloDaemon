// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
/// Logitech Lightspeed USB Receiver — HID++ 1.0 Controller
///
/// Reference: Solaar (GPL-2.0-or-later) — receiver.py, hidpp10.py
use anyhow::Result;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex;

use halod_shared::types::{PairingSlot, PairingState, PairingStatus};

use crate::{
    drivers::{
        vendors::generic::devices::common::build_device_id,
        vendors::logitech::protocols::hidpp::{
            self,
            v1::{
                receiver::{decode_link_established, decode_pairing_lock},
                Hidpp10,
            },
            DirectReport, HidppChannel, HidppNotification, PkWriteCoordinator, RECEIVER_DEVNUM,
        },
        CapabilityRef, Controller, Device, DeviceCapability, DeviceType, PairingCapability,
        TransportSwitchable,
    },
    registry::discovery::{DeviceDescriptor, DiscoveryHandle},
    state::AppState,
};

use super::generic::LogitechDevice;
use crate::drivers::vendors::generic::devices::common::TaskHandle;

const RECEIVER_VID: u16 = 0x046D;
const RECEIVER_PID: u16 = 0xC547;
/// The receiver's HID++ vendor interface number.
///
/// Windows splits that interface into two HID collections (short/long reports);
/// the shared `hidpp::collection` resolver handles the split. Linux exposes a
/// single hidraw node.
const RECEIVER_HIDPP_INTERFACE: i32 = 2;
/// Number of pairing slots to probe. Lightspeed/Bolt receivers expose at most
/// six; slots are fixed, so every one is scanned regardless of how many
/// devices are currently connected.
const MAX_PAIRED_SLOTS: u8 = 6;

inventory::submit! {
    DeviceDescriptor {
        matches: |h| matches!(h, DiscoveryHandle::Hid {
            vid: RECEIVER_VID,
            pid: RECEIVER_PID,
            interface_number: Some(RECEIVER_HIDPP_INTERFACE),
            ..
        }),
        make: |h| {
            let DiscoveryHandle::Hid { path, serial, idx, .. } = h else {
                anyhow::bail!("descriptor matched non-HID handle");
            };
            Ok(Arc::new(LogitechReceiver::new(path, serial, idx)?))
        },
    }
}

pub struct LogitechReceiver {
    id: String,
    path: String,
    messenger: Arc<dyn HidppChannel>,
    /// Children of this receiver
    logitech_devices: Arc<Mutex<Vec<Arc<LogitechDevice>>>>,
    /// Shared write coordinator created in discover_children; held here so
    /// rescan_children (and the pairing-driven rescan) can give it to
    /// newly-registered devices. Wrapped in `Arc` so the notification watcher
    /// task can read it once `discover_children` has set it.
    coordinator: Arc<OnceLock<Arc<PkWriteCoordinator>>>,
    /// Handle for the per-key coordinator drain task spawned in
    /// `discover_children`. Aborted on `close()` so the task doesn't keep the
    /// process alive after shutdown.
    drain_task: Mutex<Option<TaskHandle>>,
    /// Handle for the notification-watcher task spawned in
    /// `start_notification_watcher`. Aborted on `close()` alongside the drain task.
    /// `std::sync::Mutex` because `start_notification_watcher` is synchronous.
    notify_watcher: std::sync::Mutex<Option<TaskHandle>>,
    /// Current pairing state, updated by the notification watcher and surfaced
    /// through the `Pairing` capability.
    pairing: Arc<Mutex<PairingPhase>>,
}

/// Daemon-side pairing state for one receiver. The `Error` variant carries a
/// human-readable message for the UI.
#[derive(Debug, Clone, Default)]
enum PairingPhase {
    #[default]
    Idle,
    Listening,
    Paired,
    Error(String),
}

impl PairingPhase {
    fn wire_state(&self) -> PairingState {
        match self {
            PairingPhase::Idle => PairingState::Idle,
            PairingPhase::Listening => PairingState::Listening,
            PairingPhase::Paired => PairingState::Paired,
            PairingPhase::Error(_) => PairingState::Error,
        }
    }

    fn error_message(&self) -> Option<String> {
        match self {
            PairingPhase::Error(m) => Some(m.clone()),
            _ => None,
        }
    }
}

/// Decide the next pairing phase from a decoded `0x4A` lock-status, returning
/// the phase and whether a rescan should be kicked off. Kept pure so the lock
/// open → listening → (error | scan) transitions can be unit-tested without a
/// live receiver.
fn next_pairing_phase(
    status: super::super::protocols::hidpp::v1::receiver::PairingLockStatus,
) -> (PairingPhase, bool) {
    if status.open {
        (PairingPhase::Listening, false)
    } else if let Some(err) = status.error {
        (PairingPhase::Error(err.message().to_string()), false)
    } else {
        // Lock closed cleanly — a device likely paired. Rescan to pick it up;
        // the phase stays put until the scan resolves it (Paired or Idle).
        (PairingPhase::Listening, true)
    }
}

impl LogitechReceiver {
    pub fn new(path: &str, serial: Option<&str>, index: usize) -> Result<Self> {
        // The receiver speaks HID++ over the same split short/long collections as
        // a wired device, so it reuses the protocol's wired-open path.
        let messenger: Arc<dyn HidppChannel> = hidpp::open_wired(
            RECEIVER_VID,
            RECEIVER_PID,
            RECEIVER_HIDPP_INTERFACE,
            path,
            serial,
            DirectReport::ShortLong,
        )?;
        Ok(Self {
            id: build_device_id("logitech_receiver", serial, index),
            path: path.to_string(),
            messenger,
            logitech_devices: Arc::new(Mutex::new(Vec::new())),
            coordinator: Arc::new(OnceLock::new()),
            drain_task: Mutex::new(None),
            notify_watcher: std::sync::Mutex::new(None),
            pairing: Arc::new(Mutex::new(PairingPhase::default())),
        })
    }

    /// HID++ 1.0 handle bound to the receiver itself (devnum 0xFF).
    fn hidpp1(&self) -> Hidpp10 {
        receiver_hidpp1(&self.messenger)
    }

    async fn notify_devices(&self) {
        self.hidpp1().notify_devices().await;
    }

    async fn read_device_count(&self) -> u8 {
        self.hidpp1().device_count().await
    }

    fn start_notification_watcher(&self, app: Arc<AppState>) {
        let mut rx = self.messenger.subscribe_notifications();
        let ctx = NotifyCtx {
            messenger: Arc::clone(&self.messenger),
            logitech_devices: Arc::clone(&self.logitech_devices),
            coordinator: Arc::clone(&self.coordinator),
            pairing: Arc::clone(&self.pairing),
            scanning: Arc::new(AtomicBool::new(false)),
        };

        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(notif) => {
                        handle_notification(&notif, &ctx, app.clone()).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // The receiver's messenger multiplexes per-key RGB write
                        // ACKs onto this same channel, so a running canvas effect
                        // can flood it and lag the watcher (doubly so on Windows,
                        // which reads the split short/long collections on two
                        // tasks). Lag is not a pairing event — kicking off a
                        // pairing scan here would re-probe the slots mid-write,
                        // mis-read the extended-pairing serial, and register a
                        // duplicate device. Real pair/connect events still arrive
                        // via the 0x4A / 0x41 notifications, so just drop the gap.
                        log::debug!("[LogitechReceiver] Notification channel lagged {n}");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            log::warn!("[LogitechReceiver] Notification watcher exited unexpectedly");
        });
        *self.notify_watcher.lock().unwrap() = Some(TaskHandle::new(handle));
    }
}

/// Shared state the notification watcher needs to route notifications and react
/// to pairing events.
#[derive(Clone)]
struct NotifyCtx {
    messenger: Arc<dyn HidppChannel>,
    logitech_devices: Arc<Mutex<Vec<Arc<LogitechDevice>>>>,
    coordinator: Arc<OnceLock<Arc<PkWriteCoordinator>>>,
    pairing: Arc<Mutex<PairingPhase>>,
    /// True while a pairing rescan loop is running, so overlapping pairing
    /// notifications don't spawn duplicate scans.
    scanning: Arc<AtomicBool>,
}

/// How many times to re-nudge + re-probe the slots after a pairing event. The
/// receiver can take up to ~1 s to commit a new pairing to its info registers,
/// and a just-paired device may not answer feature reads on the first try.
const PAIRING_SCAN_ATTEMPTS: u8 = 8;
const PAIRING_SCAN_INTERVAL: Duration = Duration::from_millis(500);

impl NotifyCtx {
    /// Spawn a background loop that nudges the receiver and re-probes the pairing
    /// slots until the newly paired device registers (or attempts run out). Runs
    /// off the watcher task so further notifications keep flowing, and is guarded
    /// so concurrent pairing notifications don't stack duplicate scans.
    fn spawn_pairing_scan(&self, app: Arc<AppState>) {
        if self.coordinator.get().is_none() {
            return; // discovery hasn't finished; nothing to scan into yet
        }
        if self.scanning.swap(true, Ordering::SeqCst) {
            return; // a scan loop is already running
        }
        let ctx = self.clone();
        tokio::spawn(async move {
            // Reset the guard even if the scan panics, so a single failed scan
            // doesn't wedge the receiver into never scanning again.
            let _guard = ScanGuard(Arc::clone(&ctx.scanning));
            ctx.run_pairing_scan(&app).await;
        });
    }

    async fn run_pairing_scan(&self, app: &Arc<AppState>) {
        let Some(coordinator) = self.coordinator.get() else {
            return;
        };
        let hidpp1 = receiver_hidpp1(&self.messenger);
        for attempt in 1..=PAIRING_SCAN_ATTEMPTS {
            // Ask the receiver to rebroadcast connection status and commit its
            // pairing table, then give it a moment before probing the slots.
            hidpp1.notify_devices().await;
            tokio::time::sleep(PAIRING_SCAN_INTERVAL).await;
            let new_devices =
                scan_new_slots(&self.messenger, &self.logitech_devices, coordinator).await;
            let mut any_registered = false;
            for device in new_devices {
                if crate::registry::usecases::registration::register_device(app, device).await {
                    any_registered = true;
                }
            }
            if any_registered {
                log::info!("[LogitechReceiver] Paired device registered on attempt {attempt}");
                *self.pairing.lock().await = PairingPhase::Paired;
                app.broadcast_state().await;
                return;
            }
        }
        log::warn!(
            "[LogitechReceiver] No new device registered after {PAIRING_SCAN_ATTEMPTS} pairing rescans"
        );
        // Don't leave the UI stuck on "Listening" — settle back to Idle so the
        // user can retry. Preserve a terminal Error/Paired set elsewhere.
        let mut p = self.pairing.lock().await;
        if matches!(*p, PairingPhase::Listening) {
            *p = PairingPhase::Idle;
        }
        drop(p);
        app.broadcast_state().await;
    }
}

/// Resets the `scanning` flag on drop so a panicking scan task can't leave the
/// receiver permanently unable to start another pairing rescan.
struct ScanGuard(Arc<AtomicBool>);

impl Drop for ScanGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

async fn handle_notification(notif: &HidppNotification, ctx: &NotifyCtx, app: Arc<AppState>) {
    // Receiver-level pairing-lock status (`0x4A`). Addressed to the receiver,
    // not a child, so it is handled before per-child routing.
    if notif.sub_id == 0x4A {
        let status = decode_pairing_lock(notif.address, &notif.data);
        let (phase, scan) = next_pairing_phase(status);
        if let PairingPhase::Error(msg) = &phase {
            log::warn!("[LogitechReceiver] Pairing failed: {msg}");
        }
        // Don't overwrite Paired with Listening — if the pairing-scan loop has
        // already resolved the device, a trailing 0x4A close notification should
        // not flip the UI back to "listening". Only upgrade to Error or from
        // Idle/Listening to Listening.
        {
            let mut p = ctx.pairing.lock().await;
            let should_overwrite = match (&phase, &*p) {
                (PairingPhase::Error(_), _) => true,
                (PairingPhase::Listening, PairingPhase::Paired) => false,
                _ => true,
            };
            if should_overwrite {
                *p = phase;
            }
        }
        if scan {
            ctx.spawn_pairing_scan(app.clone());
        }
        app.broadcast_state().await;
        return;
    }

    // Snapshot device list to avoid holding the lock across .await.
    let kids = { ctx.logitech_devices.lock().await.clone() };
    let mut found: Option<Arc<LogitechDevice>> = None;
    for d in kids.iter() {
        if d.devnum().await == notif.devnum {
            found = Some(Arc::clone(d));
            break;
        }
    }

    let Some(dev) = found else {
        // A `0x41` connection notification from a device number we don't track
        // yet means a device just paired in. Rescan to register it.
        if notif.sub_id == 0x41 {
            log::info!(
                "[LogitechReceiver] Connection notification from unknown devnum {:#04x}; rescanning",
                notif.devnum
            );
            ctx.spawn_pairing_scan(app.clone());
            app.broadcast_state().await;
        }
        return;
    };

    match notif.sub_id {
        0x41 => {
            // The 0x41 "device connection" notification is sent for both
            // connect and disconnect; the link state lives in the payload.
            let connected = decode_link_established(&notif.data);
            // Suppress "came online" wireless notifications while the device has
            // adopted a wired transport. The hotplug monitor handles the revert.
            if connected && dev.is_direct().await {
                log::debug!(
                    "[LogitechReceiver] Online notification suppressed for {} (device is on wired transport)",
                    dev.id()
                );
                return;
            }
            if dev.set_online(connected, &app).await {
                log::info!(
                    "[LogitechReceiver] Device {} {}",
                    dev.id(),
                    if connected {
                        "came online"
                    } else {
                        "went offline"
                    }
                );
                if connected {
                    // Re-add to app.devices if the failed-wireless-reinit path removed
                    // it. The Arc stays alive in logitech_devices so we can do this.
                    {
                        let dev_dyn: Arc<dyn Device> = Arc::clone(&dev) as Arc<dyn Device>;
                        if app.add_device_if_absent(dev_dyn).await {
                            log::info!(
                                "[LogitechReceiver] Re-adding {} to app.devices after reconnect",
                                dev.id()
                            );
                        }
                    }
                    // Reinitialise hardware state and re-apply the last user-set RGB.
                    // `handle_notification` is already on the notification-watcher task, so
                    // awaiting directly avoids spawning a third task that can silently panic.
                    let dev2 = Arc::clone(&dev);
                    let app2 = Arc::clone(&app);
                    dev2.reinitialize_and_reapply(app2).await;
                } else {
                    app.broadcast_state().await;
                }
            }
        }
        sub_id => {
            if dev
                .handle_feature_notification(sub_id, notif.address, &notif.data, Some(&app))
                .await
            {
                app.broadcast_state().await;
            }
        }
    }
}

#[async_trait]
impl Device for LogitechReceiver {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        "Logitech Lightspeed Receiver"
    }
    fn vendor(&self) -> &str {
        "Logitech"
    }
    fn model(&self) -> &str {
        "Lightspeed Receiver"
    }

    async fn initialize(&self) -> Result<bool> {
        self.messenger.start_listener();
        log::info!("[LogitechReceiver] Initialized at {}", self.path);
        Ok(true)
    }

    async fn close(&self) {
        // Abort the per-key coordinator drain task and notification watcher
        // before stopping the messenger listener, so tasks don't try to write
        // to or read from a closed transport.
        self.drain_task.lock().await.take();
        self.notify_watcher.lock().unwrap().take();
        self.messenger.stop_listener();
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Dongle
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![
            CapabilityRef::Controller(self),
            CapabilityRef::Pairing(self),
        ]
    }

    fn as_post_register_hook(&self) -> Option<&dyn crate::drivers::PostRegisterHook> {
        Some(self)
    }

    fn write_rate_status(&self) -> Option<halod_shared::types::WriteRateStatus> {
        self.messenger.rate_status()
    }
}

#[async_trait]
impl crate::drivers::PostRegisterHook for LogitechReceiver {
    async fn on_registered(&self, app: Arc<crate::state::AppState>) {
        self.start_notification_watcher(app);
    }
}

impl LogitechReceiver {
    /// Probe all six pairing slots and register any devices not already known.
    /// Thin wrapper over the free [`scan_new_slots`] so `&self` callers keep a
    /// terse call site; the watcher task uses the free function directly.
    async fn scan_new_slots(&self, coordinator: &Arc<PkWriteCoordinator>) -> Vec<Arc<dyn Device>> {
        scan_new_slots(&self.messenger, &self.logitech_devices, coordinator).await
    }
}

/// HID++ 1.0 handle bound to the receiver itself (devnum `0xFF`).
fn receiver_hidpp1(messenger: &Arc<dyn HidppChannel>) -> Hidpp10 {
    Hidpp10::new(Arc::clone(messenger), RECEIVER_DEVNUM)
}

/// Whether a probed pairing slot's `devnum` is already tracked. Kept pure so the
/// slot-identity dedup (by devnum, robust to a flaky serial read) is unit-testable
/// without a live receiver.
fn slot_already_tracked(known_devnums: &[u8], devnum: u8) -> bool {
    known_devnums.contains(&devnum)
}

/// Probe all six pairing slots and return any devices not already tracked.
///
/// Newly-seen devices are added to `logitech_devices` (for notification routing
/// and `Controller::to_wire`), then returned so the caller can run them through
/// the registration lifecycle. Devices already in `logitech_devices` are silently
/// skipped (idempotent).
async fn scan_new_slots(
    messenger: &Arc<dyn HidppChannel>,
    logitech_devices: &Arc<Mutex<Vec<Arc<LogitechDevice>>>>,
    coordinator: &Arc<PkWriteCoordinator>,
) -> Vec<Arc<dyn Device>> {
    let mut result = Vec::new();
    let hidpp1 = receiver_hidpp1(messenger);

    for n in 1..=MAX_PAIRED_SLOTS {
        let Some(info) = hidpp1.paired_info(n).await else {
            continue;
        };

        let test_handle = DiscoveryHandle::LogitechSlot {
            devnum: info.devnum,
            wpid: info.wpid,
            serial: info.serial.as_deref(),
            messenger: Arc::clone(messenger),
        };
        if !inventory::iter::<crate::registry::discovery::DeviceDescriptor>()
            .any(|d| (d.matches)(&test_handle))
        {
            log::info!(
                "[LogitechReceiver] Slot {n}: WPID {:#06x} not recognised, skipping",
                info.wpid
            );
            continue;
        }

        let device = Arc::new(LogitechDevice::new(
            info.devnum,
            info.wpid,
            info.serial.as_deref(),
            super::generic::profile::wpid_device_type(info.wpid),
            Arc::clone(messenger),
            Arc::clone(coordinator),
        ));

        let device_dyn: Arc<dyn Device> = Arc::clone(&device) as Arc<dyn Device>;

        // Always track in logitech_devices for 0x41 notification routing —
        // even when the device is offline at startup (initialize returns
        // Ok(false)), the slot must be in logitech_devices so the watcher
        // can find it when the "came online" notification arrives later.
        // Hold the lock across the already_known check *and* the push so
        // concurrent rescan_children / pairing-scan calls cannot both observe
        // `already_known = false` and double-insert the same device.
        //
        // Dedup by devnum (the pairing slot), not the device id: a slot hosts
        // exactly one device, but the id embeds the extended-pairing serial,
        // whose read can transiently fail (e.g. while the bus is busy with RGB
        // writes) and yield a different fallback id for the same slot — which
        // would otherwise register a duplicate.
        {
            let mut guard = logitech_devices.lock().await;
            let mut known_devnums = Vec::with_capacity(guard.len());
            for d in guard.iter() {
                known_devnums.push(d.devnum().await);
            }
            if !slot_already_tracked(&known_devnums, info.devnum) {
                guard.push(Arc::clone(&device));
                result.push(device_dyn);
            }
        }
    }

    result
}

#[async_trait]
impl Controller for LogitechReceiver {
    async fn to_wire(&self) -> Option<DeviceCapability> {
        let children = self.logitech_devices.lock().await.clone();
        if children.is_empty() {
            return None;
        }
        let mut wires = Vec::with_capacity(children.len());
        for child in &children {
            wires.push(child.serialize().await);
        }
        Some(DeviceCapability::Children(wires))
    }

    async fn discover_children(&self) -> Vec<Arc<dyn Device>> {
        // Ask receiver to broadcast connection status for all paired devices
        self.notify_devices().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        let count = self.read_device_count().await;
        log::info!("[LogitechReceiver] {} paired device(s) reported", count);

        // Shared coordinator: one background task writes all devices' per-key packets
        // together so mouse and keyboard always show the same canvas frame in sync.
        let coordinator = Arc::new(PkWriteCoordinator::new());
        // Store so rescan_children can give the same coordinator to late-discovered devices.
        let _ = self.coordinator.set(Arc::clone(&coordinator));

        // Scan every pairing slot regardless of the reported count: a device
        // powered off at startup is not counted but must still be created
        // (offline) so its later "came online" notification has a device to
        // update. Empty slots return None from read_paired_device_info.
        let new_devices = self.scan_new_slots(&coordinator).await;
        let mut result = Vec::new();
        for device in new_devices {
            log::info!("[LogitechReceiver] discovered {}", device.name());
            result.push(device);
        }

        // Single background task for the whole receiver: drains all pending device
        // writes and sends them in one feature_send_many_fire call. A brief yield
        // after waking lets concurrent write_frame calls from the canvas JoinSet all
        // post before we drain, so every device writes the same canvas frame together.
        let coord = Arc::clone(&coordinator);
        let messenger = Arc::clone(&self.messenger);
        let handle = tokio::spawn(async move {
            loop {
                coord.notify.notified().await;
                tokio::task::yield_now().await;
                let mut guard = coord.pending.lock().await;
                if guard.is_empty() {
                    continue;
                }
                let all_packets: Vec<Vec<u8>> = guard.drain().flat_map(|(_, pkts)| pkts).collect();
                drop(guard);
                if let Err(e) = messenger.feature_send_many_fire(all_packets).await {
                    log::warn!("[PkWriteCoordinator] write failed: {e}");
                }
            }
        });
        *self.drain_task.lock().await = Some(TaskHandle::new(handle));

        result
    }

    /// Re-probe pairing slots for devices not yet registered. Called when a wired
    /// sibling (same serial) was just removed, so the device can now appear wirelessly.
    /// Does not restart listeners or respawn the write coordinator task.
    async fn rescan_children(&self) -> Vec<Arc<dyn Device>> {
        let Some(coordinator) = self.coordinator.get() else {
            return vec![];
        };

        // Give the receiver a moment to update its pairing table after the
        // wired device disappeared; the slot may still read empty for ~1 s.
        self.notify_devices().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        self.scan_new_slots(coordinator).await
    }
}

#[async_trait]
impl PairingCapability for LogitechReceiver {
    async fn start_pairing(&self, timeout_secs: u8) -> Result<()> {
        log::info!("[LogitechReceiver] Opening pairing lock for {timeout_secs}s");
        *self.pairing.lock().await = PairingPhase::Listening;
        self.hidpp1().open_pairing_lock(timeout_secs).await
    }

    async fn stop_pairing(&self) -> Result<()> {
        log::info!("[LogitechReceiver] Closing pairing lock");
        *self.pairing.lock().await = PairingPhase::Idle;
        self.hidpp1().close_pairing_lock().await
    }

    async fn unpair(&self, slot: u8) -> Result<Option<Arc<dyn Device>>> {
        log::info!("[LogitechReceiver] Unpairing slot {slot}");
        // For these Unifying-style receivers the pairing slot is the device
        // number, so the wire param and the local lookup key are the same value.
        self.hidpp1().unpair(slot).await?;

        // Drop the device tracked at this slot from the receiver's own
        // registry; the caller removes it from the app registry and closes it.
        let mut removed: Option<Arc<dyn Device>> = None;

        // Snapshot the device list to avoid holding the logitech_devices lock
        // across .await calls (devnum() acquires the transport lock).
        let snapshot = { self.logitech_devices.lock().await.clone() };
        let mut keep = Vec::with_capacity(snapshot.len());
        for d in snapshot {
            if d.devnum().await == slot {
                removed = Some(Arc::clone(&d) as Arc<dyn Device>);
            } else {
                keep.push(d);
            }
        }
        *self.logitech_devices.lock().await = keep;

        if removed.is_none() {
            log::warn!("[LogitechReceiver] Unpair slot {slot}: no tracked device");
        }
        Ok(removed)
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        let phase = self.pairing.lock().await.clone();
        // Snapshot the device list to avoid holding the lock across .await calls.
        let devices = { self.logitech_devices.lock().await.clone() };
        let mut slots = Vec::with_capacity(devices.len());
        for dev in &devices {
            slots.push(PairingSlot {
                slot: dev.devnum().await,
                device_id: dev.id().to_owned(),
                name: dev.wire_device_name().await,
                connected: dev.wire_device_connected().await,
            });
        }
        slots.sort_by_key(|s| s.slot);
        Some(DeviceCapability::Pairing(PairingStatus {
            state: phase.wire_state(),
            error: phase.error_message(),
            max_slots: MAX_PAIRED_SLOTS,
            slots,
        }))
    }
}

// The receiver register/notification codecs (decode_link_established,
// parse_extended_serial) and their tests live with the protocol in
// `protocols::hidpp::v1::receiver`. The HID++ short/long collection picker tests
// live with the shared resolver in `protocols::hidpp` (module `collection_tests`).

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::vendors::logitech::protocols::hidpp::v1::receiver::{
        PairingError, PairingLockStatus,
    };

    fn lock(open: bool, error: Option<PairingError>) -> PairingLockStatus {
        PairingLockStatus { open, error }
    }

    // Lock open ⇒ listening, no rescan.
    #[test]
    fn next_phase_open_is_listening() {
        let (phase, scan) = next_pairing_phase(lock(true, None));
        assert!(matches!(phase, PairingPhase::Listening));
        assert!(!scan);
    }

    // Lock closed with an error ⇒ surface the error, no rescan.
    #[test]
    fn next_phase_error_carries_message() {
        let (phase, scan) = next_pairing_phase(lock(false, Some(PairingError::TooManyDevices)));
        match phase {
            PairingPhase::Error(msg) => assert_eq!(msg, PairingError::TooManyDevices.message()),
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(!scan);
    }

    // Lock closed cleanly ⇒ stay listening and kick off a rescan to register the
    // device that just paired.
    #[test]
    fn next_phase_clean_close_triggers_scan() {
        let (phase, scan) = next_pairing_phase(lock(false, None));
        assert!(matches!(phase, PairingPhase::Listening));
        assert!(scan);
    }

    #[test]
    fn wire_state_maps_each_phase() {
        assert_eq!(PairingPhase::Idle.wire_state(), PairingState::Idle);
        assert_eq!(
            PairingPhase::Listening.wire_state(),
            PairingState::Listening
        );
        assert_eq!(PairingPhase::Paired.wire_state(), PairingState::Paired);
        assert_eq!(
            PairingPhase::Error("x".into()).wire_state(),
            PairingState::Error
        );
    }

    #[test]
    fn error_message_only_for_error_phase() {
        assert_eq!(PairingPhase::Idle.error_message(), None);
        assert_eq!(
            PairingPhase::Error("boom".into()).error_message(),
            Some("boom".to_string())
        );
    }

    // A slot whose devnum is already tracked is a duplicate and must be skipped,
    // even if the serial read failed this pass and produced a different id.
    #[test]
    fn slot_already_tracked_matches_by_devnum() {
        assert!(slot_already_tracked(&[1, 2, 5], 2));
        assert!(!slot_already_tracked(&[1, 2, 5], 3));
        assert!(!slot_already_tracked(&[], 1));
    }

    // The ScanGuard must clear the flag on drop so a panicking scan can't wedge
    // the receiver into never scanning again.
    #[test]
    fn scan_guard_resets_flag_on_drop() {
        let flag = Arc::new(AtomicBool::new(true));
        {
            let _g = ScanGuard(Arc::clone(&flag));
        }
        assert!(!flag.load(Ordering::SeqCst));
    }
}
