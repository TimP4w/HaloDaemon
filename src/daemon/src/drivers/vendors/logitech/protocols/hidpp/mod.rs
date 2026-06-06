// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
/// HID++ 1.0 and 2.0 protocol implementation.
///
/// Reference: Solaar (GPL-2.0-or-later) — base.py, hidpp10.py, hidpp20.py
///   by Daniel Pavel and contributors
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, oneshot, Mutex};

use crate::drivers::transports::{hid::HidTransport, Transport};

pub mod controls;
pub mod dpi;
pub mod onboard;
pub mod rgb_effects;

pub use dpi::{parse_current_dpi, parse_dpi_list};

// ── Report IDs ────────────────────────────────────────────────────────────────

pub const HIDPP_SHORT: u8 = 0x10;
pub const HIDPP_LONG: u8 = 0x11;
pub const SHORT_LEN: usize = 7;
pub const LONG_LEN: usize = 20;

/// Number of consecutive listener read errors after which the reader task
/// concludes the device is gone and stops. At a 50 ms read timeout plus a
/// 10 ms backoff this is roughly ~1.2 s — slightly ahead of the 2 s hotplug
/// monitor, so the loop stops cleanly instead of spinning and flooding the log.
const MAX_CONSECUTIVE_READ_ERRORS: u32 = 20;

pub const RECEIVER_DEVNUM: u8 = 0xFF;

// ── HID++ 1.0 register addresses ──────────────────────────────────────────────

pub const REG_DEVICE_COUNT: u16 = 0x0002;
pub const REG_RECEIVER_INFO: u16 = 0x02B5;

pub const INFO_PAIRING: u8 = 0x20; // +devnum-1
pub const INFO_EXTENDED_PAIRING: u8 = 0x30; // +devnum-1
#[allow(dead_code)]
pub const INFO_DEVICE_NAME: u8 = 0x40; // +devnum-1

// ── HID++ 2.0 feature codes ───────────────────────────────────────────────────

pub mod feature {
    pub const ROOT: u16 = 0x0000;
    pub const FEATURE_SET: u16 = 0x0001;
    pub const FIRMWARE_VERSION: u16 = 0x0003;
    pub const DEVICE_NAME: u16 = 0x0005;
    pub const UNIFIED_BATTERY: u16 = 0x1004;
    pub const ADJUSTABLE_DPI: u16 = 0x2201;
    #[allow(dead_code)]
    pub const EXTENDED_ADJUSTABLE_DPI: u16 = 0x2202;
    pub const REPORT_RATE: u16 = 0x8060;
    pub const EXT_REPORT_RATE: u16 = 0x8061;
    pub const RGB_EFFECTS: u16 = 0x8071;
    pub const PER_KEY_LIGHTING_V2: u16 = 0x8081;
    pub const ONBOARD_PROFILES: u16 = 0x8100;
    pub const KEYBOARD_LAYOUT_2: u16 = 0x4540;
    pub const REPROG_CONTROLS_V4: u16 = 0x1b04;
    /// G-key / macro-key divert (gaming keyboards & mice).
    pub const GKEY: u16 = 0x8010;
    /// Lists the device's controls; used by newer remap-capable devices.
    #[allow(dead_code)]
    pub const CONTROL_LIST: u16 = 0x1b10;
    /// Per-control HID-usage reporting / remap (newer keyboards & mice).
    #[allow(dead_code)]
    pub const REPORT_HID_USAGE: u16 = 0x1bc0;
    /// Full key customization (analog / hot-swap keyboards).
    #[allow(dead_code)]
    pub const FULL_KEY_CUSTOMIZATION: u16 = 0x1b05;
    /// Host-side raw mouse-button reporting (gaming mice without REPROG_CONTROLS).
    pub const MOUSE_BUTTON_SPY: u16 = 0x8110;
}

// ── HID++ collection-path resolution ──────────────────────────────────────────

/// Resolving the short- and long-report HID collection paths of an HID++ device.
///
/// HID++ devices expose a vendor interface that Windows splits into two HID
/// top-level collections on usage page `0xFF00`: usage 1 carries short reports
/// (`0x10`), usage 2 carries long reports (`0x11`). Linux exposes a single
/// hidraw node carrying both, with usage info reported as 0.
///
/// Both the Lightspeed receiver and directly-connected (wired) HID++ devices
/// need this resolution, so it lives in the protocol module they share.
pub mod collection {
    use anyhow::Result;

    /// Vendor usage page used by HID++ short/long collections.
    pub const HIDPP_USAGE_PAGE: u16 = 0xFF00;
    /// Usage of the short-report (`0x10`) collection.
    pub const HIDPP_USAGE_SHORT: u16 = 1;
    /// Usage of the long-report (`0x11`) collection.
    pub const HIDPP_USAGE_LONG: u16 = 2;

    /// One enumerated HID collection — the subset of `hidapi::DeviceInfo` the
    /// path picker needs. Kept separate so `select_hidpp_paths` is hardware-free
    /// and unit-testable.
    pub struct HidEntry {
        pub path: String,
        pub iface: i32,
        pub usage_page: u16,
        pub usage: u16,
    }

    /// Pick the short- and long-report collection paths from enumerated entries.
    ///
    /// Among entries on `interface` (which excludes `-1` pseudo-devices): the
    /// short path is the usage-1 collection, the long path the usage-2
    /// collection. When usage info is absent (Linux hidraw) or both resolve to
    /// the same node, the long path is dropped and the caller opens a single
    /// handle.
    pub fn select_hidpp_paths(
        entries: &[HidEntry],
        interface: i32,
        fallback_path: &str,
    ) -> (String, Option<String>) {
        let on_iface: Vec<&HidEntry> =
            entries.iter().filter(|e| e.iface == interface).collect();

        let short = on_iface
            .iter()
            .find(|e| e.usage_page == HIDPP_USAGE_PAGE && e.usage == HIDPP_USAGE_SHORT)
            .map(|e| e.path.clone())
            .or_else(|| {
                // No usage-1 match (Linux): use the path discovery handed us when
                // it is itself an on-interface entry, else the first one.
                if on_iface.iter().any(|e| e.path == fallback_path) {
                    Some(fallback_path.to_string())
                } else {
                    on_iface.first().map(|e| e.path.clone())
                }
            })
            .unwrap_or_else(|| fallback_path.to_string());

        let long = on_iface
            .iter()
            .find(|e| e.usage_page == HIDPP_USAGE_PAGE && e.usage == HIDPP_USAGE_LONG)
            .map(|e| e.path.clone())
            .filter(|p| p != &short);

        (short, long)
    }

    /// Re-enumerate HID devices to resolve an HID++ device's short/long
    /// collection paths. Filters by `(vid, pid)` and, when given, `serial`;
    /// `fallback_path` is the path discovery matched.
    pub fn resolve_hidpp_paths(
        vid: u16,
        pid: u16,
        interface: i32,
        fallback_path: &str,
        serial: Option<&str>,
    ) -> Result<(String, Option<String>)> {
        let api = hidapi::HidApi::new()?;
        let serial_filter = serial.filter(|s| !s.is_empty());
        let entries: Vec<HidEntry> = api
            .device_list()
            .filter(|d| d.vendor_id() == vid && d.product_id() == pid)
            .filter(|d| match serial_filter {
                Some(s) => d.serial_number() == Some(s),
                None => true,
            })
            .map(|d| HidEntry {
                path: d.path().to_string_lossy().into_owned(),
                iface: d.interface_number(),
                usage_page: d.usage_page(),
                usage: d.usage(),
            })
            .collect();
        Ok(select_hidpp_paths(&entries, interface, fallback_path))
    }
}

// ── Notification ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct HidppNotification {
    pub devnum: u8,
    pub sub_id: u8,
    pub address: u8,
    pub data: Vec<u8>,
}

// ── Inflight entry ────────────────────────────────────────────────────────────

struct InflightRequest {
    devnum: u8,
    sub_id: u8,
    tx: oneshot::Sender<Result<Vec<u8>>>,
}

// ── HidppMessenger ────────────────────────────────────────────────────────────

/// Owns the raw HID transport and multiplexes request/response.
///
/// Only one request is in-flight at a time (serialised by `request_lock`).
/// Unsolicited notifications are broadcast on `notify_tx`.
#[derive(Clone)]
pub struct HidppMessenger<T: Transport = HidTransport> {
    transport: Arc<T>,
    /// Serialises all write + wait-for-response cycles.
    request_lock: Arc<Mutex<()>>,
    /// The single in-flight request waiting for a reply.
    inflight: Arc<Mutex<Option<InflightRequest>>>,
    pub notify_tx: broadcast::Sender<HidppNotification>,
    /// Set to true to stop the listener task.
    stop_flag: Arc<AtomicBool>,
}

impl<T: Transport + 'static> HidppMessenger<T> {
    pub fn new(transport: T) -> Self {
        let (notify_tx, _) = broadcast::channel(64);
        Self {
            transport: Arc::new(transport),
            request_lock: Arc::new(Mutex::new(())),
            inflight: Arc::new(Mutex::new(None)),
            notify_tx,
            stop_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal the listener task to stop. Safe to call multiple times.
    pub fn stop_listener(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }

    /// Start the background listener task(s) (call once after construction).
    ///
    /// Always reads the short handle. When the transport has a dedicated long
    /// handle (Windows splits the HID++ interface into two collections), a
    /// second task reads it too — a short register request gets its reply as a
    /// long report on the long collection, and both feed the same routing.
    pub fn start_listener(&self) {
        self.spawn_reader(false);
        if self.transport.has_long_handle() {
            self.spawn_reader(true);
        }
    }

    /// Spawn one reader task. `long` selects the long handle via `read_long`.
    fn spawn_reader(&self, long: bool) {
        let transport = Arc::clone(&self.transport);
        let inflight = Arc::clone(&self.inflight);
        let notify_tx = self.notify_tx.clone();
        let stop_flag = Arc::clone(&self.stop_flag);

        tokio::spawn(async move {
            // Consecutive read errors. A powered-off device fails every read;
            // counting them lets the loop log once and stop instead of
            // spinning until the hotplug monitor catches up ~2 s later.
            let mut consecutive_errors: u32 = 0;
            loop {
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let read_res = if long {
                    transport.read_long(LONG_LEN).await
                } else {
                    transport.read(LONG_LEN).await
                };
                let buf: Vec<u8> = match read_res {
                    Ok(b) if !b.is_empty() => {
                        consecutive_errors = 0;
                        b
                    }
                    Ok(_) => {
                        consecutive_errors = 0;
                        continue;
                    }
                    Err(e) => {
                        consecutive_errors += 1;
                        match classify_read_error(&e.to_string(), consecutive_errors) {
                            ReadErrorAction::Stop => {
                                log::debug!(
                                    "[HID++] Listener stopping: persistent read errors \
                                     (device likely disconnected): {e}"
                                );
                                break;
                            }
                            ReadErrorAction::LogAndRetry => {
                                log::debug!("[HID++] Listener read error: {e}");
                                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                                continue;
                            }
                            ReadErrorAction::Retry => {
                                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                                continue;
                            }
                        }
                    }
                };
                dispatch_packet(&buf, &inflight, &notify_tx).await;
            }
        });
    }

    // ── Internal send+wait ────────────────────────────────────────────────────

    async fn send_and_wait(
        &self,
        devnum: u8,
        sub_id: u8,
        func_byte: u8,
        params: &[u8],
        long: bool,
    ) -> Result<Vec<u8>> {
        // Serialise: only one request in-flight at a time
        let _lock = self.request_lock.lock().await;

        let (tx, rx) = oneshot::channel();
        *self.inflight.lock().await = Some(InflightRequest { devnum, sub_id, tx });

        let pkt = build_packet(devnum, sub_id, func_byte, params, long);
        if let Err(e) = self.transport.write(&pkt).await {
            self.inflight.lock().await.take();
            return Err(e);
        }

        match tokio::time::timeout(tokio::time::Duration::from_secs(2), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => bail!("HID++ response channel closed"),
            Err(_) => {
                self.inflight.lock().await.take();
                bail!(
                    "HID++ timeout: devnum={devnum:#04x} sub_id={sub_id:#04x} func={func_byte:#04x}"
                )
            }
        }
    }

    // ── HID++ 1.0 register access ─────────────────────────────────────────────

    /// Read a HID++ 1.0 register (9-bit address).
    pub async fn hidpp10_read(&self, devnum: u8, register: u16, params: &[u8]) -> Result<Vec<u8>> {
        let sub_id = 0x81u8 | (((register >> 8) as u8) & 0x02);
        let address = (register & 0xFF) as u8;
        let mut p = [0u8; 3];
        let n = params.len().min(3);
        p[..n].copy_from_slice(&params[..n]);
        self.send_and_wait(devnum, sub_id, address, &p, false).await
    }

    /// Write a HID++ 1.0 register (fire-and-forget).
    pub async fn hidpp10_write(&self, devnum: u8, register: u16, params: &[u8]) -> Result<()> {
        let sub_id = 0x80u8 | (((register >> 8) as u8) & 0x02);
        let address = (register & 0xFF) as u8;
        let mut p = [0u8; 3];
        let n = params.len().min(3);
        p[..n].copy_from_slice(&params[..n]);
        let pkt = build_packet(devnum, sub_id, address, &p, false);
        self.transport.write(&pkt).await
    }

    // ── HID++ 2.0 feature requests ────────────────────────────────────────────

    /// Call a HID++ 2.0 feature function on a device (devnum 1–6).
    /// `function` is the 4-bit function code (0x0–0xF).
    pub async fn feature_request(
        &self,
        devnum: u8,
        feature_index: u8,
        function: u8,
        params: &[u8],
    ) -> Result<Vec<u8>> {
        // `function` is already the full high-nibble byte (0x00, 0x10, 0x20, …);
        // just stamp sw_id=1 into the low nibble.
        let func_byte = function | 0x01;
        let long = params.len() > 3 || feature_index > 0;
        self.send_and_wait(devnum, feature_index, func_byte, params, long).await
    }

    /// Send multiple HID++ 2.0 feature commands in a single `spawn_blocking` call.
    ///
    /// All packets are written back-to-back inside one blocking-thread dispatch,
    /// eliminating per-packet async overhead. Use this for per-key RGB frames where
    /// N batches + a COMMIT would otherwise require N+1 separate dispatches.
    ///
    /// Fire-and-forget: `request_lock` is held only long enough to write the
    /// packets and no responses are awaited. The device ACKs arrive asynchronously
    /// and are routed to `notify_tx` as unmatched notifications. Do NOT mix this
    /// with a subsequent `feature_request` for the same feature index in the same
    /// logical transaction — the orphaned ACKs will resolve that request's
    /// in-flight slot prematurely.
    pub async fn feature_send_many_fire(
        &self,
        packets: Vec<Vec<u8>>,
    ) -> Result<()> {
        let _lock = self.request_lock.lock().await;
        self.transport.write_many(&packets).await
    }

    /// Send a raw HID++ long vendor packet (fire-and-forget).
    ///
    /// Builds `[0x11, devnum, sub_id, address, params..., 0x00...]` and writes it
    /// without waiting for a response. Use for vendor commands that don't follow
    /// the register-access (0x80/0x81) or feature-request sub_id conventions.
    pub async fn hidpp_long_fire(
        &self,
        devnum: u8,
        sub_id: u8,
        address: u8,
        params: &[u8],
    ) -> Result<()> {
        let pkt = build_packet(devnum, sub_id, address, params, true);
        let _lock = self.request_lock.lock().await;
        self.transport.write(&pkt).await
    }

    /// Look up a feature code via ROOT. Returns its index, or None if absent.
    pub async fn get_feature_index(&self, devnum: u8, feature_code: u16) -> Result<Option<u8>> {
        let params = [(feature_code >> 8) as u8, (feature_code & 0xFF) as u8];
        match self.feature_request(devnum, 0x00, 0x00, &params).await {
            Ok(reply) if reply.first().copied().unwrap_or(0) != 0 => Ok(Some(reply[0])),
            Ok(_) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Enumerate all features via FEATURE_SET (0x0001).
    /// Returns feature_code → feature_index.
    pub async fn enumerate_features(&self, devnum: u8) -> Result<HashMap<u16, u8>> {
        let fs_index = match self.get_feature_index(devnum, feature::FEATURE_SET).await? {
            Some(i) => i,
            None => return Ok(HashMap::new()),
        };

        let count_reply = self.feature_request(devnum, fs_index, 0x00, &[]).await?;
        let count = count_reply.first().copied().unwrap_or(0) as usize;
        log::debug!("[HID++ dev={devnum:#04x}] FEATURE_SET at index {fs_index}, count={count}");

        let mut table = HashMap::with_capacity(count + 2);
        table.insert(feature::ROOT, 0u8);
        table.insert(feature::FEATURE_SET, fs_index);

        let mut errors = 0usize;
        for i in 1..=count {
            let reply = match self.feature_request(devnum, fs_index, 0x10, &[i as u8]).await {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("[HID++ dev={devnum:#04x}] Feature enum at index {i}: {e}");
                    errors += 1;
                    continue;
                }
            };
            if reply.len() >= 2 {
                let code = ((reply[0] as u16) << 8) | (reply[1] as u16);
                if code != 0 {
                    log::debug!("[HID++ dev={devnum:#04x}] Feature[{i}] = {code:#06x}");
                    table.insert(code, i as u8);
                }
            }
        }
        // A device that has just woken often answers the FEATURE_SET count query
        // but returns BUSY (error 0x07) for the individual feature reads. The
        // resulting partial table has no DEVICE_NAME and no real features, so
        // accepting it would register the device with the fallback name
        // "Logitech Device" and zero capabilities. Fail instead, so the caller's
        // reconnect retry loop keeps waiting until the device is fully awake.
        if errors > 0 {
            bail!(
                "[HID++ dev={devnum:#04x}] feature enumeration incomplete: \
                 {errors}/{count} reads failed (device still waking)"
            );
        }
        log::debug!("[HID++ dev={devnum:#04x}] Total features: {}", table.len());
        Ok(table)
    }
}

// ── Packet dispatch ───────────────────────────────────────────────────────────

/// Route one received HID++ packet: resolve the in-flight request it answers,
/// or broadcast it as an unsolicited notification. Shared by both the short-
/// and long-handle reader tasks.
async fn dispatch_packet(
    buf: &[u8],
    inflight: &Mutex<Option<InflightRequest>>,
    notify_tx: &broadcast::Sender<HidppNotification>,
) {
    if buf.len() < 4 {
        return;
    }
    let report_id = buf[0];
    if report_id != HIDPP_SHORT && report_id != HIDPP_LONG {
        return;
    }

    let devnum = buf[1];
    let sub_id = buf[2];
    let address = buf[3];

    // HID++ error responses use sub_id 0x8F (wired) or 0xFF (Lightspeed wireless).
    let is_error = sub_id == 0x8F || sub_id == 0xFF;

    let mut guard = inflight.lock().await;
    let matched = match guard.as_ref() {
        // Match: same devnum AND (same sub_id OR it was an error)
        Some(req) => req.devnum == devnum && (req.sub_id == sub_id || is_error),
        None => false,
    };

    if matched {
        let req = guard.take().unwrap();
        drop(guard);
        if is_error {
            // Error packet layout: [devnum, 0x8F/0xFF, feature_idx, func_byte, err_code, ...]
            let err_feature = address;
            let err_func = buf.get(4).copied().unwrap_or(0);
            let error_code = buf.get(5).copied().unwrap_or(0);
            let _ = req.tx.send(Err(anyhow::anyhow!(
                "HID++ error: sub={sub_id:#04x} feature={err_feature:#04x} func={err_func:#04x} code={error_code:#04x}"
            )));
        } else {
            let _ = req.tx.send(Ok(buf[4..].to_vec()));
        }
    } else {
        drop(guard);
        // Unsolicited notification
        log::trace!(
            "[HID++] notification dev={devnum:#04x} sub={sub_id:#04x} addr={address:#04x} data={:02x?}",
            &buf[4..]
        );
        let _ = notify_tx.send(HidppNotification {
            devnum,
            sub_id,
            address,
            data: buf[4..].to_vec(),
        });
    }
}

// ── Packet builder ────────────────────────────────────────────────────────────

pub fn build_packet(devnum: u8, sub_id: u8, func_byte: u8, params: &[u8], long: bool) -> Vec<u8> {
    if long {
        let mut pkt = vec![0u8; LONG_LEN];
        pkt[0] = HIDPP_LONG;
        pkt[1] = devnum;
        pkt[2] = sub_id;
        pkt[3] = func_byte;
        let n = params.len().min(LONG_LEN - 4);
        pkt[4..4 + n].copy_from_slice(&params[..n]);
        pkt
    } else {
        let mut pkt = vec![0u8; SHORT_LEN];
        pkt[0] = HIDPP_SHORT;
        pkt[1] = devnum;
        pkt[2] = sub_id;
        pkt[3] = func_byte;
        let n = params.len().min(SHORT_LEN - 4);
        pkt[4..4 + n].copy_from_slice(&params[..n]);
        pkt
    }
}

// ── Listener read-error classification ───────────────────────────────────────

/// What the reader loop should do after a failed read.
#[derive(Debug, PartialEq, Eq)]
enum ReadErrorAction {
    /// Log the error once, then back off and retry.
    LogAndRetry,
    /// Silently back off and retry (a repeat of an already-logged error).
    Retry,
    /// Stop the reader task — the device is (or looks) disconnected.
    Stop,
}

/// Decide how to react to a listener read error.
///
/// `msg` is the error's display string; `consecutive_errors` is the run length
/// of read errors so far (1 for the first failure of a run). An explicit
/// disconnect message stops immediately; otherwise a sustained run of failures
/// is treated as a disconnect once it reaches `MAX_CONSECUTIVE_READ_ERRORS`.
fn classify_read_error(msg: &str, consecutive_errors: u32) -> ReadErrorAction {
    if msg.contains("disconnected") || msg.contains("poll error") {
        return ReadErrorAction::Stop;
    }
    if consecutive_errors >= MAX_CONSECUTIVE_READ_ERRORS {
        return ReadErrorAction::Stop;
    }
    if consecutive_errors <= 1 {
        ReadErrorAction::LogAndRetry
    } else {
        ReadErrorAction::Retry
    }
}

// ── CRC16 (Logitech onboard profile) ─────────────────────────────────────────

pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod crc16_tests {
    use super::crc16;

    // CRC-16/CCITT-FALSE: init 0xFFFF, poly 0x1021, no reflection, no xorout.
    // The standard check value for the ASCII string "123456789" is 0x29B1.
    #[test]
    fn crc16_matches_ccitt_false_check_vector() {
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }

    #[test]
    fn crc16_of_empty_input_is_initial_value() {
        assert_eq!(crc16(&[]), 0xFFFF);
    }
}

#[cfg(test)]
mod collection_tests {
    use super::collection::*;

    fn entry(path: &str, iface: i32, usage_page: u16, usage: u16) -> HidEntry {
        HidEntry { path: path.to_string(), iface, usage_page, usage }
    }

    // Windows: interface 2 is split into Col01 (usage 1, short) and Col02
    // (usage 2, long). The picker must return distinct short/long paths.
    #[test]
    fn windows_two_collections_split_short_and_long() {
        let entries = vec![
            entry("col01", 2, HIDPP_USAGE_PAGE, HIDPP_USAGE_SHORT),
            entry("col02", 2, HIDPP_USAGE_PAGE, HIDPP_USAGE_LONG),
        ];
        // Discovery may have matched either collection — result is the same.
        let (short, long) = select_hidpp_paths(&entries, 2, "col02");
        assert_eq!(short, "col01");
        assert_eq!(long.as_deref(), Some("col02"));
    }

    // Linux hidraw: one node per interface, usage info absent (0). The single
    // path becomes the short handle and there is no long handle.
    #[test]
    fn linux_single_path_no_long_handle() {
        let entries = vec![entry("hidraw3", 2, 0, 0)];
        let (short, long) = select_hidpp_paths(&entries, 2, "hidraw3");
        assert_eq!(short, "hidraw3");
        assert_eq!(long, None);
    }

    // interface_number == -1 pseudo-devices must never be selected.
    #[test]
    fn pseudo_interface_entries_ignored() {
        let entries = vec![
            entry("ghost", -1, HIDPP_USAGE_PAGE, HIDPP_USAGE_SHORT),
            entry("col01", 2, HIDPP_USAGE_PAGE, HIDPP_USAGE_SHORT),
            entry("col02", 2, HIDPP_USAGE_PAGE, HIDPP_USAGE_LONG),
        ];
        let (short, long) = select_hidpp_paths(&entries, 2, "col01");
        assert_eq!(short, "col01");
        assert_eq!(long.as_deref(), Some("col02"));
    }

    // A usage-2 entry resolving to the same path as the short handle is dropped.
    #[test]
    fn long_path_equal_to_short_is_dropped() {
        let entries = vec![
            entry("only", 2, HIDPP_USAGE_PAGE, HIDPP_USAGE_SHORT),
            entry("only", 2, HIDPP_USAGE_PAGE, HIDPP_USAGE_LONG),
        ];
        let (short, long) = select_hidpp_paths(&entries, 2, "only");
        assert_eq!(short, "only");
        assert_eq!(long, None);
    }

    // The picker considers only entries on the requested interface; collections
    // of an unrelated interface on the same usage page are never selected.
    #[test]
    fn respects_interface_parameter() {
        let entries = vec![
            entry("if1-short", 1, HIDPP_USAGE_PAGE, HIDPP_USAGE_SHORT),
            entry("if1-long", 1, HIDPP_USAGE_PAGE, HIDPP_USAGE_LONG),
            entry("if2-short", 2, HIDPP_USAGE_PAGE, HIDPP_USAGE_SHORT),
            entry("if2-long", 2, HIDPP_USAGE_PAGE, HIDPP_USAGE_LONG),
        ];
        let (short, long) = select_hidpp_paths(&entries, 2, "if2-short");
        assert_eq!(short, "if2-short");
        assert_eq!(long.as_deref(), Some("if2-long"));

        let (short, long) = select_hidpp_paths(&entries, 1, "if1-short");
        assert_eq!(short, "if1-short");
        assert_eq!(long.as_deref(), Some("if1-long"));
    }

    // A wired Logitech device (e.g. G502 X Plus) on interface 2 splits into two
    // collections on Windows just like the receiver — both must resolve.
    #[test]
    fn wired_device_two_collections() {
        let entries = vec![
            entry("g502-col01", 2, HIDPP_USAGE_PAGE, HIDPP_USAGE_SHORT),
            entry("g502-col02", 2, HIDPP_USAGE_PAGE, HIDPP_USAGE_LONG),
        ];
        let (short, long) = select_hidpp_paths(&entries, 2, "g502-col01");
        assert_eq!(short, "g502-col01");
        assert_eq!(long.as_deref(), Some("g502-col02"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hidpp_frame_short() {
        let pkt = build_packet(0x01, 0x00, 0x01, &[0xAB, 0xCD], false);
        assert_eq!(pkt.len(), SHORT_LEN);
        assert_eq!(pkt[0], HIDPP_SHORT);
        assert_eq!(pkt[1], 0x01);
        assert_eq!(pkt[2], 0x00);
        assert_eq!(pkt[3], 0x01);
        assert_eq!(pkt[4], 0xAB);
        assert_eq!(pkt[5], 0xCD);
        assert_eq!(pkt[6], 0x00);
    }

    #[test]
    fn test_hidpp_frame_long() {
        let pkt = build_packet(0x02, 0x10, 0x21, &[1, 2, 3, 4, 5], true);
        assert_eq!(pkt.len(), LONG_LEN);
        assert_eq!(pkt[0], HIDPP_LONG);
        assert_eq!(pkt[1], 0x02);
        assert_eq!(pkt[2], 0x10);
        assert_eq!(pkt[3], 0x21);
        assert_eq!(&pkt[4..9], &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_classify_read_error_disconnect_substring() {
        assert_eq!(
            classify_read_error("device disconnected", 1),
            ReadErrorAction::Stop
        );
        assert_eq!(
            classify_read_error("hidapi poll error: -1", 1),
            ReadErrorAction::Stop
        );
    }

    #[test]
    fn test_classify_read_error_first_error_logs() {
        assert_eq!(
            classify_read_error("HID read error: hidapi error: ", 1),
            ReadErrorAction::LogAndRetry
        );
    }

    #[test]
    fn test_classify_read_error_repeat_is_silent() {
        assert_eq!(
            classify_read_error("HID read error: hidapi error: ", 2),
            ReadErrorAction::Retry
        );
        assert_eq!(
            classify_read_error(
                "HID read error: hidapi error: ",
                MAX_CONSECUTIVE_READ_ERRORS - 1
            ),
            ReadErrorAction::Retry
        );
    }

    #[test]
    fn test_classify_read_error_threshold_stops() {
        assert_eq!(
            classify_read_error(
                "HID read error: hidapi error: ",
                MAX_CONSECUTIVE_READ_ERRORS
            ),
            ReadErrorAction::Stop
        );
    }
}
