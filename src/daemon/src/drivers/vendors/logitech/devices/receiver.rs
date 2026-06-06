// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
/// Logitech Lightspeed USB Receiver — HID++ 1.0 Controller
///
/// Reference: Solaar (GPL-2.0-or-later) — receiver.py, hidpp10.py
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::{
    discovery::{DeviceDescriptor, DiscoveryHandle},
    drivers::{
        transports::hid::HidTransport,
        vendors::generic::devices::common::build_device_id,
        vendors::logitech::protocols::hidpp::{
            collection, HidppMessenger, HidppNotification, INFO_EXTENDED_PAIRING, INFO_PAIRING,
            RECEIVER_DEVNUM, REG_DEVICE_COUNT, REG_RECEIVER_INFO,
        },
        CapabilityRef, Controller, Device, DeviceCapability, DeviceType, TransportSwitchable,
    },
    ipc::broadcast_state,
    state::AppState,
};

use super::device::{LogitechDevice, PkWriteCoordinator};

// ── Receiver identity & HID++ collection layout ───────────────────────────────

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

// ── Device registration ───────────────────────────────────────────────────────

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

// ── Struct ────────────────────────────────────────────────────────────────────

pub struct LogitechReceiver {
    id: String,
    path: String,
    messenger: Arc<HidppMessenger>,
    /// Typed list kept separately for notification routing.
    logitech_devices: Arc<Mutex<Vec<Arc<LogitechDevice>>>>,
    /// Type-erased list for Controller/serialize.
    children: Arc<Mutex<Vec<Arc<dyn Device>>>>,
    /// Shared write coordinator created in discover_children; held here so
    /// rescan_children can give it to newly-registered devices.
    coordinator: std::sync::OnceLock<Arc<PkWriteCoordinator>>,
}

impl LogitechReceiver {
    pub fn new(path: &str, serial: Option<&str>, index: usize) -> Result<Self> {
        // Windows exposes the HID++ interface as two collections — short reports
        // (0x10) and long reports (0x11) live on separate device paths. Resolve
        // both and open a dual-handle transport; on Linux only one path exists
        // and the transport collapses to a single handle.
        let (short_path, long_path) = collection::resolve_hidpp_paths(
            RECEIVER_VID,
            RECEIVER_PID,
            RECEIVER_HIDPP_INTERFACE,
            path,
            serial,
        )?;
        // Short read timeout (50 ms) so the listener loop releases the device Mutex
        // frequently enough that concurrent writes are not blocked for seconds.
        let transport = HidTransport::open_dual(
            &short_path,
            long_path.as_deref().unwrap_or(""),
            None,
            50,
            false,
        )?;
        let messenger = Arc::new(HidppMessenger::new(transport));
        Ok(Self {
            id: build_device_id("logitech_receiver", serial, index),
            path: short_path,
            messenger,
            logitech_devices: Arc::new(Mutex::new(Vec::new())),
            children: Arc::new(Mutex::new(Vec::new())),
            coordinator: std::sync::OnceLock::new(),
        })
    }

    async fn notify_devices(&self) {
        if let Err(e) = self
            .messenger
            .hidpp10_write(RECEIVER_DEVNUM, REG_DEVICE_COUNT, &[0x02])
            .await
        {
            log::warn!("[LogitechReceiver] notify_devices failed: {e}");
        }
    }

    /// The count is in param byte 1 (data[1]), not byte 0.
    async fn read_device_count(&self) -> u8 {
        match self
            .messenger
            .hidpp10_read(RECEIVER_DEVNUM, REG_DEVICE_COUNT, &[])
            .await
        {
            Ok(data) => data.get(1).copied().unwrap_or(0),
            Err(e) => {
                log::warn!("[LogitechReceiver] Failed to read device count: {e}");
                0
            }
        }
    }

    async fn read_paired_device_info(&self, n: u8) -> Option<PairedDeviceInfo> {
        let pairing_param = INFO_PAIRING + n - 1;
        let pair = self
            .messenger
            .hidpp10_read(RECEIVER_DEVNUM, REG_RECEIVER_INFO, &[pairing_param])
            .await
            .ok()?;

        if pair.len() < 8 {
            return None;
        }

        // WPID is bytes[3:5] big-endian (Solaar: extract_wpid reverses pair[3:5])
        let wpid = ((pair[3] as u16) << 8) | (pair[4] as u16);
        if wpid == 0 || wpid == 0xFFFF {
            return None;
        }

        let ext_param = INFO_EXTENDED_PAIRING + n - 1;
        let serial = self
            .messenger
            .hidpp10_read(RECEIVER_DEVNUM, REG_RECEIVER_INFO, &[ext_param])
            .await
            .ok()
            .and_then(|ext| parse_extended_serial(&ext));

        Some(PairedDeviceInfo {
            devnum: n,
            wpid,
            serial,
        })
    }

    fn start_notification_watcher(&self, app: Arc<AppState>) {
        let mut rx = self.messenger.notify_tx.subscribe();
        let logitech_devices = Arc::clone(&self.logitech_devices);

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(notif) => {
                        handle_notification(&notif, &logitech_devices, app.clone()).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::debug!("[LogitechReceiver] Notification channel lagged {n}");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

async fn handle_notification(
    notif: &HidppNotification,
    logitech_devices: &Mutex<Vec<Arc<LogitechDevice>>>,
    app: Arc<AppState>,
) {
    let kids = logitech_devices.lock().await;
    let mut found: Option<Arc<LogitechDevice>> = None;
    for d in kids.iter() {
        if d.devnum().await == notif.devnum {
            found = Some(Arc::clone(d));
            break;
        }
    }
    drop(kids);
    let dev = found;

    let Some(dev) = dev else { return };

    match notif.sub_id {
        0x41 => {
            // The 0x41 "device connection" notification is sent for both
            // connect and disconnect; the link state lives in the payload.
            let connected = decode_link_established(&notif.data);
            // Suppress "came online" wireless notifications while the device has
            // adopted a wired transport. The hotplug monitor handles the revert.
            if connected && dev.is_using_wired_transport().await {
                log::debug!(
                    "[LogitechReceiver] Online notification suppressed for {} (device is on wired transport)",
                    dev.id()
                );
                return;
            }
            if dev.set_online(connected).await {
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
                        let mut devs = app.devices.lock().await;
                        if !devs.iter().any(|d| Arc::ptr_eq(d, &dev_dyn)) {
                            log::info!(
                                "[LogitechReceiver] Re-adding {} to app.devices after reconnect",
                                dev.id()
                            );
                            devs.push(dev_dyn);
                        }
                    }
                    // Reinitialise hardware state and re-apply the last user-set RGB
                    // in a background task so the notification watcher is not blocked.
                    let dev2 = Arc::clone(&dev);
                    let app2 = Arc::clone(&app);
                    tokio::spawn(async move { dev2.reinitialize_and_reapply(app2).await });
                } else {
                    broadcast_state(app).await;
                }
            }
        }
        sub_id => {
            if dev
                .handle_feature_notification(sub_id, notif.address, &notif.data)
                .await
            {
                broadcast_state(app).await;
            }
        }
    }
}

/// Decode the link state from an HID++ 1.0 receiver `0x41` device-connection
/// notification. The receiver sends `0x41` for both connect and disconnect;
/// bit `0x40` of the first data byte is "link not established" — set on
/// power-off, clear on power-on. (`link_established = !(data[0] & 0x40)`)
fn decode_link_established(data: &[u8]) -> bool {
    data.first().map_or(false, |&b| b & 0x40 == 0)
}

/// Parse the 4-byte serial from an extended-pairing reply.
/// Returns `None` for all-zero or all-`0xFF` payloads (unset slot sentinels).
fn parse_extended_serial(ext: &[u8]) -> Option<String> {
    if ext.len() < 5 {
        return None;
    }
    let b = &ext[1..5];
    if b == [0xFF, 0xFF, 0xFF, 0xFF] || b == [0, 0, 0, 0] {
        return None;
    }
    Some(format!("{:02X}{:02X}{:02X}{:02X}", b[0], b[1], b[2], b[3]))
}

struct PairedDeviceInfo {
    devnum: u8,
    wpid: u16,
    serial: Option<String>,
}

// ── Device trait ──────────────────────────────────────────────────────────────

#[async_trait]
impl Device for LogitechReceiver {
    fn id(&self) -> String {
        self.id.clone()
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
        self.messenger.stop_listener();
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Hub
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![CapabilityRef::Controller(self)]
    }
}

// ── Slot scanning ─────────────────────────────────────────────────────────────

impl LogitechReceiver {
    /// Probe all six pairing slots and register any devices not already known.
    ///
    /// Idempotent: `register_device` deduplicates by ID, so already-registered
    /// devices are silently skipped. Newly-registered devices are added to
    /// both `logitech_devices` (for notification routing) and `children`.
    async fn scan_new_slots(
        &self,
        app: &Arc<AppState>,
        coordinator: &Arc<PkWriteCoordinator>,
    ) -> Vec<Arc<dyn Device>> {
        let mut result = Vec::new();

        for n in 1..=MAX_PAIRED_SLOTS {
            let Some(info) = self.read_paired_device_info(n).await else {
                continue;
            };

            let test_handle = DiscoveryHandle::LogitechSlot {
                devnum: info.devnum,
                wpid: info.wpid,
                serial: info.serial.as_deref(),
                messenger: Arc::clone(&self.messenger),
            };
            if !inventory::iter::<crate::discovery::DeviceDescriptor>()
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
                Arc::clone(&self.messenger),
            ));

            device.set_pk_coordinator(Arc::clone(coordinator));

            let device_dyn: Arc<dyn Device> = Arc::clone(&device) as Arc<dyn Device>;

            // Always track in logitech_devices for 0x41 notification routing —
            // even when the device is offline at startup (initialize returns
            // Ok(false)), the slot must be in logitech_devices so the watcher
            // can find it when the "came online" notification arrives later.
            let already_known = self
                .logitech_devices
                .lock()
                .await
                .iter()
                .any(|d| d.id() == device.id());

            let registered =
                crate::usecases::registration::register_device(app, Arc::clone(&device_dyn)).await;

            if registered {
                log::info!(
                    "[LogitechReceiver] Slot {n}: {} (WPID {:#06x}) registered",
                    device.name(),
                    info.wpid,
                );
                result.push(Arc::clone(&device_dyn));
            }

            if !already_known {
                if !registered {
                    log::info!(
                        "[LogitechReceiver] Slot {n}: {} (WPID {:#06x}) offline — tracking for reconnect",
                        device.name(),
                        info.wpid,
                    );
                }
                self.logitech_devices.lock().await.push(Arc::clone(&device));
                self.children.lock().await.push(device_dyn);
            }
        }

        result
    }
}

// ── Controller trait ──────────────────────────────────────────────────────────

#[async_trait]
impl Controller for LogitechReceiver {
    async fn to_wire(&self) -> Option<DeviceCapability> {
        let children = self.children.lock().await.clone();
        if children.is_empty() {
            return None;
        }
        let mut wires = Vec::with_capacity(children.len());
        for child in &children {
            wires.push(child.serialize().await);
        }
        Some(DeviceCapability::Children(wires))
    }

    async fn discover_children(&self, app: Arc<AppState>) -> Vec<Arc<dyn Device>> {
        self.start_notification_watcher(app.clone());

        // Ask receiver to broadcast connection status for all paired devices
        self.notify_devices().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        let count = self.read_device_count().await;
        log::info!("[LogitechReceiver] {} paired device(s) reported", count);

        // Shared coordinator: one background task writes all devices' per-key packets
        // together so mouse and keyboard always show the same canvas frame in sync.
        let coordinator = PkWriteCoordinator::new();
        // Store so rescan_children can give the same coordinator to late-discovered devices.
        let _ = self.coordinator.set(Arc::clone(&coordinator));

        // Scan every pairing slot regardless of the reported count: a device
        // powered off at startup is not counted but must still be created
        // (offline) so its later "came online" notification has a device to
        // update. Empty slots return None from read_paired_device_info.
        let result = self.scan_new_slots(&app, &coordinator).await;

        // Single background task for the whole receiver: drains all pending device
        // writes and sends them in one feature_send_many_fire call. A brief yield
        // after waking lets concurrent write_frame calls from the canvas JoinSet all
        // post before we drain, so every device writes the same canvas frame together.
        let coord = Arc::clone(&coordinator);
        let messenger = Arc::clone(&self.messenger);
        tokio::spawn(async move {
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

        result
    }

    /// Re-probe pairing slots for devices not yet registered. Called when a wired
    /// sibling (same serial) was just removed, so the device can now appear wirelessly.
    /// Does not restart listeners or respawn the write coordinator task.
    async fn rescan_children(&self, app: Arc<AppState>) {
        let Some(coordinator) = self.coordinator.get() else {
            return;
        };

        // Give the receiver a moment to update its pairing table after the
        // wired device disappeared; the slot may still read empty for ~1 s.
        self.notify_devices().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let new_devices = self.scan_new_slots(&app, coordinator).await;
        if !new_devices.is_empty() {
            crate::ipc::broadcast_state(app).await;
        }
    }
}

// The HID++ short/long collection picker tests live with the shared resolver
// in `drivers::protocols::hidpp` (module `collection_tests`).

#[cfg(test)]
mod tests {
    use super::{decode_link_established, parse_extended_serial};

    // Live captures from a Lightspeed receiver: the `0x41` notification is sent
    // for both power-off and power-on; bit 0x40 of data[0] is "link not
    // established". The trailing bytes vary per device and are irrelevant.
    #[test]
    fn decode_power_off_is_disconnected() {
        assert!(!decode_link_established(&[0x71, 0xb0, 0x40])); // device 1 off
        assert!(!decode_link_established(&[0x72, 0x99, 0x40])); // device 2 off
    }

    #[test]
    fn decode_power_on_is_connected() {
        assert!(decode_link_established(&[0xb1, 0xb0, 0x40])); // device 1 on
        assert!(decode_link_established(&[0xb2, 0x99, 0x40])); // device 2 on
    }

    #[test]
    fn decode_empty_payload_is_disconnected() {
        assert!(!decode_link_established(&[]));
    }

    #[test]
    fn parse_extended_serial_returns_hex_string() {
        // Byte 0 is ignored; bytes 1–4 are the serial.
        let ext = [0x00u8, 0xAB, 0xCD, 0xEF, 0x12, 0x00];
        assert_eq!(parse_extended_serial(&ext), Some("ABCDEF12".to_string()));
    }

    #[test]
    fn parse_extended_serial_rejects_all_ff_sentinel() {
        let ext = [0x00u8, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(parse_extended_serial(&ext), None);
    }

    #[test]
    fn parse_extended_serial_rejects_all_zero_sentinel() {
        let ext = [0x00u8, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(parse_extended_serial(&ext), None);
    }

    #[test]
    fn parse_extended_serial_rejects_short_payload() {
        assert_eq!(parse_extended_serial(&[0x00, 0xAB, 0xCD]), None);
    }
}
