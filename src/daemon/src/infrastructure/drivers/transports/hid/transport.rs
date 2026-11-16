// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use crate::infrastructure::drivers::transports::{
    HidTransport as HidTransportTrait, Transport, TransportEvent,
};
use crate::infrastructure::drivers::Metered;
use halod_shared::types::WriteRateLimit;
use halod_shared::types::WriteRateStatus;

pub(super) fn build_frame(data: &[u8], report_size: Option<usize>) -> Vec<u8> {
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
        let mut out = Vec::with_capacity(payload.len() + 1);
        out.push(0x00);
        out.extend_from_slice(&payload);
        out
    }
}

/// One HID collection: two file descriptors for the same device path.
///
/// Separate read/write fds mean the listener's `read_timeout` (holding
/// `read_dev` for up to `timeout_ms`) never blocks writes — critical for
/// high-frequency per-key RGB frames.
#[derive(Clone)]
struct HidIo {
    read_dev: Arc<Mutex<hidapi::HidDevice>>,
    write_dev: Arc<Mutex<hidapi::HidDevice>>,
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
            read_dev: Arc::new(Mutex::new(read_dev)),
            write_dev: Arc::new(Mutex::new(write_dev)),
        })
    }

    fn open_input(api: &hidapi::HidApi, path: &str, purpose: &str) -> Result<hidapi::HidDevice> {
        let cpath = std::ffi::CString::new(path)?;
        let dev = api
            .open_path(cpath.as_c_str())
            .with_context(|| format!("failed to open HID device ({purpose}) at {path}"))?;
        dev.set_blocking_mode(false)
            .context("failed to set non-blocking mode")?;
        Ok(dev)
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
        // Debug, not warn: a disconnected device fails every write, flooding the
        // log until hotplug removes it. The error is returned anyway.
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
fn write_batch(
    dev: &Mutex<hidapi::HidDevice>,
    packets: &[Vec<u8>],
    use_feature_report: bool,
) -> Result<()> {
    let guard = dev.lock();
    for pkt in packets {
        write_one(&guard, pkt, use_feature_report)?;
    }
    Ok(())
}

const EVENT_QUEUE_CAPACITY: usize = 256;
const INPUT_REPORT_MAX: usize = 4096;

/// Above the Linux hidraw ring (64 reports), which drops new arrivals once
/// full: a request handle left unread across fire-and-forget writes would
/// otherwise lose the reply to the next request.
const STALE_DRAIN_LIMIT: usize = 256;

fn drain_stale_reports(
    mut read: impl FnMut() -> Result<Vec<u8>>,
    mut sink: impl FnMut(Vec<u8>),
) -> Result<usize> {
    for drained in 0..STALE_DRAIN_LIMIT {
        let report = read()?;
        if report.is_empty() {
            return Ok(drained);
        }
        sink(report);
    }
    Ok(STALE_DRAIN_LIMIT)
}

/// Endpoint label carried by an event report the protocol layer deferred: the
/// original short/long collection is not recoverable once the bytes crossed the
/// `read_any` boundary, and the label is informational only.
const DEFERRED_ENDPOINT: &str = "deferred";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputEndpoint {
    Primary,
    Companion,
}

impl InputEndpoint {
    fn name(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Companion => "companion",
        }
    }
}

/// Event-only dispatcher queue. Request/reply reads use their original input
/// handles directly and never consume this queue. Event listeners are opened
/// lazily only after the plugin advertises an `event()` callback.
struct HidEvents {
    reports: Mutex<VecDeque<TransportEvent>>,
    wake: tokio::sync::watch::Sender<u64>,
}

impl HidEvents {
    fn new() -> Self {
        let (wake, _) = tokio::sync::watch::channel(0);
        Self {
            reports: Mutex::new(VecDeque::with_capacity(EVENT_QUEUE_CAPACITY)),
            wake,
        }
    }

    fn bump_wake(&self) {
        let next = self.wake.borrow().wrapping_add(1);
        self.wake.send_replace(next);
    }

    fn push(&self, endpoint: &'static str, data: Vec<u8>) {
        let mut reports = self.reports.lock();
        if reports.len() == EVENT_QUEUE_CAPACITY {
            reports.pop_front();
            log::debug!("[HidTransport] event queue full; dropping oldest event");
        }
        log::trace!(
            "[HidTransport] event push endpoint={endpoint} depth={} bytes={:02x?}",
            reports.len() + 1,
            data
        );
        reports.push_back(TransportEvent { endpoint, data });
        drop(reports);
        self.bump_wake();
    }

    fn defer(&self, data: Vec<u8>) {
        self.push(DEFERRED_ENDPOINT, data);
    }

    fn drain(&self, limit: usize) -> Vec<TransportEvent> {
        let mut reports = self.reports.lock();
        let count = reports.len().min(limit);
        let events = reports.drain(..count).collect();
        let reports_remaining = !reports.is_empty();
        drop(reports);
        if reports_remaining {
            self.bump_wake();
        }
        events
    }
}

fn spawn_event_reader(dev: hidapi::HidDevice, endpoint: InputEndpoint, events: Weak<HidEvents>) {
    let _ = std::thread::Builder::new()
        .name(format!("halod-hid-event-{}", endpoint.name()))
        .spawn(move || loop {
            let Some(events) = events.upgrade() else {
                break;
            };
            let mut buf = vec![0; INPUT_REPORT_MAX];
            let result = dev.read_timeout(&mut buf, 100);
            match result {
                Ok(0) => {}
                Ok(n) => {
                    buf.truncate(n);
                    log::trace!(
                        "[HidTransport] {} event reader read {n} bytes",
                        endpoint.name()
                    );
                    events.push(endpoint.name(), buf);
                }
                Err(error) => {
                    log::debug!(
                        "[HidTransport] {} event reader stopped: {error}",
                        endpoint.name()
                    );
                    break;
                }
            }
        });
}

fn read_io(dev: &Mutex<hidapi::HidDevice>, size: usize, timeout_ms: i32) -> Result<Vec<u8>> {
    let guard = dev.lock();
    let mut buf = vec![0u8; size];
    let n = guard
        .read_timeout(&mut buf, timeout_ms)
        .map_err(|e| anyhow::anyhow!("HID read error: {e}"))?;
    buf.truncate(n);
    Ok(buf)
}

fn read_any_io(
    primary: &Mutex<hidapi::HidDevice>,
    companion: Option<&Mutex<hidapi::HidDevice>>,
    size: usize,
    timeout_ms: i32,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(0) as u64);
    loop {
        for dev in std::iter::once(primary).chain(companion) {
            let guard = dev.lock();
            let mut buf = vec![0u8; size];
            let n = guard
                .read_timeout(&mut buf, 0)
                .map_err(|e| anyhow::anyhow!("HID read error: {e}"))?;
            if n != 0 {
                buf.truncate(n);
                return Ok(buf);
            }
        }
        if timeout_ms <= 0 || Instant::now() >= deadline {
            return Ok(Vec::new());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The raw HID handles gated behind [`Metered`]: a `short` handle and, for
/// devices that Windows splits into two collections, an optional `long`
/// handle.
struct HidState {
    primary: HidIo,
    companion: Option<HidIo>,
}

/// HID transport with optional platform-aware padding.
///
/// The primary and optional companion collections are addressed explicitly;
/// protocol code decides which one to use.
#[derive(Clone)]
pub struct HidTransport {
    io: Metered<HidState>,
    events: Arc<HidEvents>,
    primary_path: Arc<str>,
    companion_path: Option<Arc<str>>,
    event_listener_started: Arc<Mutex<bool>>,
    report_size: Option<usize>,
    timeout_ms: i32,
    /// When true, writes use `send_feature_report()` (HIDIOCSFEATURE) instead of
    /// `write()` (output report); for devices whose vendor interface only accepts
    /// feature reports.
    use_feature_report: bool,
}

/// Which GET_REPORT ioctl a scoped report read issues.
#[derive(Clone, Copy)]
enum ReportKind {
    Feature,
    Input,
}

impl HidTransport {
    /// Read a feature or input report for `report_id`. Shares the write handle
    /// under the write lock (a GET_REPORT ioctl must not race a concurrent
    /// write on the same fd) and the same hang timeout as `feature_exchange`.
    fn get_report(&self, report_id: u8, size: usize, kind: ReportKind) -> Result<Vec<u8>> {
        let state = self.io.write_access(0)?;
        let mut buf = vec![0u8; size + 1];
        buf[0] = report_id;
        let guard = state.primary.write_dev.lock();
        let n = match kind {
            ReportKind::Feature => guard.get_feature_report(&mut buf),
            ReportKind::Input => guard.get_input_report(&mut buf),
        }
        .map_err(|e| anyhow::anyhow!("HID report read error: {}", e))?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Open a HID device by path.
    ///
    /// `report_size` controls framing:
    /// - `None` — raw passthrough; no padding, no prepended report-ID byte. Used
    ///   by the HID++ flow where the protocol layer builds full frames.
    /// - `Some(N)` — platform-aware padding. Linux: prepend `0x00` then pad the
    ///   payload to `N` bytes (`N+1` written). Windows: pad to `N+1` total
    ///   (caller's first byte is the report ID).
    pub fn open(
        path: &str,
        report_size: Option<usize>,
        timeout_ms: i32,
        use_feature_report: bool,
        limit: Option<WriteRateLimit>,
    ) -> Result<Self> {
        let api = hidapi::HidApi::new().context("failed to create HidApi")?;
        let primary = HidIo::open(&api, path)?;
        Ok(Self {
            io: Metered::new(
                HidState {
                    primary,
                    companion: None,
                },
                limit,
            ),
            events: Arc::new(HidEvents::new()),
            primary_path: Arc::from(path),
            companion_path: None,
            event_listener_started: Arc::new(Mutex::new(false)),
            report_size,
            timeout_ms,
            use_feature_report,
        })
    }

    /// Open a device that Windows splits into two HID collections.
    ///
    /// `primary_path` is the discovery-matched collection and `companion_path`
    /// is an optional second collection. When the companion path is empty or
    /// equal to the primary path, only one handle is opened and the
    /// transport behaves exactly like `open`.
    pub fn open_dual(
        primary_path: &str,
        companion_path: &str,
        report_size: Option<usize>,
        timeout_ms: i32,
        use_feature_report: bool,
        limit: Option<WriteRateLimit>,
    ) -> Result<Self> {
        let api = hidapi::HidApi::new().context("failed to create HidApi")?;
        let primary = HidIo::open(&api, primary_path)?;
        let companion = if companion_path.is_empty() || companion_path == primary_path {
            None
        } else {
            Some(HidIo::open(&api, companion_path)?)
        };
        Ok(Self {
            io: Metered::new(HidState { primary, companion }, limit),
            events: Arc::new(HidEvents::new()),
            primary_path: Arc::from(primary_path),
            companion_path: (!companion_path.is_empty() && companion_path != primary_path)
                .then(|| Arc::from(companion_path)),
            event_listener_started: Arc::new(Mutex::new(false)),
            report_size,
            timeout_ms,
            use_feature_report,
        })
    }

    fn frame(&self, data: &[u8]) -> Vec<u8> {
        build_frame(data, self.report_size)
    }

    fn begin_write(&self, len: usize) -> Result<&HidState> {
        let state = self.io.write_access(len)?;
        let listener_started = *self.event_listener_started.lock();
        let mut buf = vec![0u8; INPUT_REPORT_MAX];
        let mut drained = 0;
        for io in std::iter::once(&state.primary).chain(state.companion.iter()) {
            let Some(guard) = io.read_dev.try_lock() else {
                continue;
            };
            drained += drain_stale_reports(
                || {
                    let n = guard
                        .read_timeout(&mut buf, 0)
                        .map_err(|e| anyhow::anyhow!("HID read error: {e}"))?;
                    Ok(buf[..n].to_vec())
                },
                |report| {
                    if !listener_started {
                        self.events.defer(report);
                    }
                },
            )?;
        }
        if drained > 0 {
            log::trace!(
                "[HidTransport] dropped {drained} stale input reports before a write on {}",
                self.primary_path
            );
        }
        Ok(state)
    }

    fn start_event_listener(&self) -> Result<()> {
        let mut started = self.event_listener_started.lock();
        if *started {
            log::trace!(
                "[HidTransport] event listener already started for {}",
                self.primary_path
            );
            return Ok(());
        }
        log::debug!(
            "[HidTransport] starting event listener for {}",
            self.primary_path
        );
        let api = hidapi::HidApi::new().context("failed to create HidApi for event listener")?;
        let primary = HidIo::open_input(&api, &self.primary_path, "event")?;
        spawn_event_reader(
            primary,
            InputEndpoint::Primary,
            Arc::downgrade(&self.events),
        );
        if let Some(path) = &self.companion_path {
            let companion = HidIo::open_input(&api, path, "companion event")?;
            spawn_event_reader(
                companion,
                InputEndpoint::Companion,
                Arc::downgrade(&self.events),
            );
        }
        *started = true;
        Ok(())
    }
}

impl Transport for HidTransport {
    fn write(&self, data: &[u8]) -> Result<()> {
        let framed = self.frame(data);
        let state = self.begin_write(framed.len())?;
        write_one(
            &state.primary.write_dev.lock(),
            &framed,
            self.use_feature_report,
        )
    }

    fn read(&self, size: usize) -> Result<Vec<u8>> {
        read_io(
            &self.io.read_access().primary.read_dev,
            size,
            self.timeout_ms,
        )
    }

    fn write_many(&self, packets: &[Vec<u8>]) -> Result<()> {
        let total_len: usize = packets.iter().map(Vec::len).sum();
        let state = self.begin_write(total_len)?;
        let framed: Vec<_> = packets.iter().map(|packet| self.frame(packet)).collect();
        write_batch(&state.primary.write_dev, &framed, self.use_feature_report)
    }

    fn as_hid(&self) -> Option<&dyn HidTransportTrait> {
        Some(self)
    }

    fn rate_status(&self) -> WriteRateStatus {
        self.io.status()
    }

    fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.io.set_limit(limit);
    }

    fn event_receiver(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.events.wake.subscribe())
    }

    fn drain_events(&self, limit: usize) -> Result<Vec<TransportEvent>> {
        Ok(self.events.drain(limit))
    }

    fn enable_event_listener(&self) -> Result<()> {
        self.start_event_listener()
    }
}

impl HidTransport {
    fn companion(state: &HidState) -> Result<&HidIo> {
        state
            .companion
            .as_ref()
            .context("companion HID collection is not available")
    }
}

impl HidTransportTrait for HidTransport {
    fn write_companion(&self, data: &[u8]) -> Result<()> {
        let framed = self.frame(data);
        let companion = Self::companion(self.begin_write(framed.len())?)?;
        write_one(
            &companion.write_dev.lock(),
            &framed,
            self.use_feature_report,
        )
    }

    fn read_companion(&self, size: usize) -> Result<Vec<u8>> {
        let companion = Self::companion(self.io.read_access())?;
        read_io(&companion.read_dev, size, self.timeout_ms)
    }

    fn write_many_companion(&self, packets: &[Vec<u8>]) -> Result<()> {
        let total_len: usize = packets.iter().map(Vec::len).sum();
        let companion = Self::companion(self.begin_write(total_len)?)?;
        let framed: Vec<_> = packets.iter().map(|packet| self.frame(packet)).collect();
        write_batch(&companion.write_dev, &framed, self.use_feature_report)
    }

    /// Send a feature report and read back the response via `get_feature_report`.
    ///
    /// For drivers whose vendor interface answers `HIDIOCSFEATURE` via
    /// `HIDIOCGFEATURE` rather than the interrupt-IN endpoint. Both ioctls share
    /// the fd under the write lock, with a 1 ms gap for the device to process.
    fn feature_exchange(&self, data: &[u8], response_size: usize) -> Result<Vec<u8>> {
        let framed = self.frame(data);
        let state = self.begin_write(framed.len())?;
        let guard = state.primary.write_dev.lock();
        guard
            .send_feature_report(&framed)
            .map_err(|e| anyhow::anyhow!("feature write error: {}", e))?;
        std::thread::sleep(Duration::from_millis(1));
        let mut buf = vec![0u8; response_size + 1];
        let n = guard
            .get_feature_report(&mut buf)
            .map_err(|e| anyhow::anyhow!("feature read error: {}", e))?;
        buf.truncate(n);
        Ok(buf)
    }

    fn send_feature_report(&self, data: &[u8]) -> Result<()> {
        let framed = self.frame(data);
        let state = self.begin_write(framed.len())?;
        state
            .primary
            .write_dev
            .lock()
            .send_feature_report(&framed)
            .map_err(|e| anyhow::anyhow!("feature write error: {}", e))
    }

    fn get_feature_report(&self, report_id: u8, size: usize) -> Result<Vec<u8>> {
        self.get_report(report_id, size, ReportKind::Feature)
    }

    fn get_input_report(&self, report_id: u8, size: usize) -> Result<Vec<u8>> {
        self.get_report(report_id, size, ReportKind::Input)
    }

    fn read_nonblocking(&self, size: usize) -> Result<Vec<u8>> {
        read_io(&self.io.read_access().primary.read_dev, size, 0)
    }

    fn read_any(&self, size: usize) -> Result<Vec<u8>> {
        let state = self.io.read_access();
        read_any_io(
            &state.primary.read_dev,
            state.companion.as_ref().map(|io| &*io.read_dev),
            size,
            self.timeout_ms,
        )
    }

    fn defer_event(&self, data: &[u8]) -> Result<()> {
        // Before the independent listener starts (notably during initialize),
        // preserve an unsolicited report encountered by a request read. Once
        // active, the listener receives its own copy; deferring the request
        // handle's copy would dispatch the same event twice.
        if !*self.event_listener_started.lock() {
            self.events.defer(data.to_vec());
        }
        Ok(())
    }

    fn has_companion(&self) -> bool {
        self.io.read_access().companion.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_queue_is_bounded_and_drops_the_oldest_event() {
        let events = HidEvents::new();
        for value in 0..=EVENT_QUEUE_CAPACITY {
            events.push(InputEndpoint::Primary.name(), vec![value as u8]);
        }
        let drained = events.drain(EVENT_QUEUE_CAPACITY + 1);
        assert_eq!(drained.len(), EVENT_QUEUE_CAPACITY);
        assert_eq!(drained[0].data, vec![1]);
    }

    #[test]
    fn deferred_report_enters_only_the_event_queue() {
        let events = HidEvents::new();
        events.defer(vec![0xde, 0xad]);
        let drained = events.drain(8);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].endpoint, DEFERRED_ENDPOINT);
        assert_eq!(drained[0].data, vec![0xde, 0xad]);
    }

    #[test]
    fn stale_drain_forwards_every_queued_report_in_order_and_stops_at_the_first_empty_read() {
        let mut source = VecDeque::from(vec![vec![1], vec![2], vec![3], vec![], vec![4]]);
        let mut sink = Vec::new();
        let drained = drain_stale_reports(
            || Ok(source.pop_front().unwrap_or_default()),
            |report| sink.push(report),
        )
        .unwrap();
        assert_eq!(drained, 3);
        assert_eq!(sink, vec![vec![1], vec![2], vec![3]]);
        assert_eq!(source, VecDeque::from(vec![vec![4]]));
    }

    #[test]
    fn stale_drain_terminates_on_an_endless_source_after_clearing_a_full_hidraw_ring() {
        let mut reads = 0;
        let drained = drain_stale_reports(
            || {
                reads += 1;
                Ok(vec![0xaa])
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(drained, reads);
        assert!(drained >= 64);
    }

    #[test]
    fn stale_drain_propagates_a_read_error_without_forwarding_partial_input() {
        let mut sink = Vec::new();
        let result =
            drain_stale_reports(|| Err(anyhow::anyhow!("gone")), |report| sink.push(report));
        assert!(result.is_err());
        assert!(sink.is_empty());
    }

    #[test]
    fn defer_bumps_the_event_wake_watch() {
        let events = HidEvents::new();
        let rx = events.wake.subscribe();
        assert!(!rx.has_changed().unwrap());
        events.defer(vec![0x01]);
        assert!(rx.has_changed().unwrap());
    }

    #[test]
    fn bounded_drain_rewakes_for_the_remaining_tail() {
        let events = HidEvents::new();
        let mut rx = events.wake.subscribe();
        for value in 0..3 {
            events.push(InputEndpoint::Primary.name(), vec![value]);
        }

        // Model the event task acknowledging the producer wake before it asks
        // the serialized plugin worker to drain one fair-sized batch.
        rx.borrow_and_update();
        let first = events.drain(1);
        assert_eq!(first.len(), 1);
        assert!(rx.has_changed().unwrap());
        assert_eq!(events.drain(8).len(), 2);
    }

    #[test]
    fn frame_raw_passthrough() {
        assert_eq!(build_frame(&[0x10, 0x02], None), vec![0x10, 0x02]);
    }

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
    fn frame_linux_long_data_kept() {
        let result = build_frame(&[0x10, 0x02, 0x03], Some(2));
        assert_eq!(result, vec![0x00, 0x10, 0x02, 0x03]);
    }

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
}
