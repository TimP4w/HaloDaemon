// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::drivers::transports::{Transport, TransportEvent};
use crate::drivers::Metered;
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
async fn write_batch(
    dev: Arc<Mutex<hidapi::HidDevice>>,
    packets: Vec<Vec<u8>>,
    use_feature_report: bool,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let guard = dev.blocking_lock();
        for pkt in &packets {
            write_one(&guard, pkt, use_feature_report)?;
        }
        Ok(())
    })
    .await
    .context("spawn_blocking panicked")?
}

const INPUT_QUEUE_CAPACITY: usize = 256;
const INPUT_REPORT_MAX: usize = 4096;

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

struct QueuedReport {
    endpoint: InputEndpoint,
    data: Vec<u8>,
}

/// Host-owned bounded report queue. Reader threads are the only code touching
/// hidapi input handles; Lua request reads and event drains consume this queue.
struct HidReaders {
    reports: StdMutex<VecDeque<QueuedReport>>,
    /// Reports popped by an endpoint-agnostic `read_any` that the protocol
    /// layer handed back as not-its-own; delivered ahead of the live queue on
    /// the next event drain so arrival order is preserved.
    deferred: StdMutex<VecDeque<Vec<u8>>>,
    available: Condvar,
    wake: tokio::sync::watch::Sender<u64>,
}

impl HidReaders {
    fn new() -> Self {
        let (wake, _) = tokio::sync::watch::channel(0);
        Self {
            reports: StdMutex::new(VecDeque::with_capacity(INPUT_QUEUE_CAPACITY)),
            deferred: StdMutex::new(VecDeque::with_capacity(INPUT_QUEUE_CAPACITY)),
            available: Condvar::new(),
            wake,
        }
    }

    fn bump_wake(&self) {
        let next = self.wake.borrow().wrapping_add(1);
        self.wake.send_replace(next);
    }

    fn push(&self, report: QueuedReport) {
        let mut reports = self.reports.lock().unwrap();
        if reports.len() == INPUT_QUEUE_CAPACITY {
            reports.pop_front();
            log::debug!("[HidTransport] input queue full; dropping oldest report");
        }
        reports.push_back(report);
        self.available.notify_all();
        self.bump_wake();
    }

    /// Set a report aside for the event path (`drain`). Wakes the event watcher.
    fn defer(&self, data: Vec<u8>) {
        let mut deferred = self.deferred.lock().unwrap();
        if deferred.len() == INPUT_QUEUE_CAPACITY {
            deferred.pop_front();
            log::debug!("[HidTransport] deferred input queue full; dropping oldest report");
        }
        deferred.push_back(data);
        drop(deferred);
        self.bump_wake();
    }

    fn pop(&self, endpoint: InputEndpoint, timeout_ms: i32, size: usize) -> Vec<u8> {
        self.pop_matching(timeout_ms, size, |r| r.endpoint == endpoint)
    }

    /// Pop the next report from any endpoint (merged short/long queue), so a
    /// reply is matched wherever the collection split delivered it.
    fn pop_any(&self, timeout_ms: i32, size: usize) -> Vec<u8> {
        self.pop_matching(timeout_ms, size, |_| true)
    }

    fn pop_matching(
        &self,
        timeout_ms: i32,
        size: usize,
        accept: impl Fn(&QueuedReport) -> bool,
    ) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(0) as u64);
        let mut reports = self.reports.lock().unwrap();
        loop {
            if let Some(index) = reports.iter().position(&accept) {
                let mut data = reports
                    .remove(index)
                    .expect("position came from queue")
                    .data;
                data.truncate(size);
                return data;
            }
            if timeout_ms <= 0 {
                return Vec::new();
            }
            let now = Instant::now();
            if now >= deadline {
                return Vec::new();
            }
            let (next, wait) = self
                .available
                .wait_timeout(reports, deadline - now)
                .unwrap();
            reports = next;
            if wait.timed_out() {
                return Vec::new();
            }
        }
    }

    fn drain(&self, limit: usize) -> Vec<TransportEvent> {
        let mut events: Vec<TransportEvent> = {
            let mut deferred = self.deferred.lock().unwrap();
            let count = deferred.len().min(limit);
            deferred
                .drain(..count)
                .map(|data| TransportEvent {
                    endpoint: DEFERRED_ENDPOINT,
                    data,
                })
                .collect()
        };
        if events.len() >= limit {
            return events;
        }
        let mut reports = self.reports.lock().unwrap();
        let count = reports.len().min(limit - events.len());
        events.extend(reports.drain(..count).map(|report| TransportEvent {
            endpoint: report.endpoint.name(),
            data: report.data,
        }));
        events
    }
}

fn spawn_reader(
    dev: Arc<Mutex<hidapi::HidDevice>>,
    endpoint: InputEndpoint,
    readers: Weak<HidReaders>,
) {
    let _ = std::thread::Builder::new()
        .name(format!("halod-hid-{}", endpoint.name()))
        .spawn(move || loop {
            let Some(readers) = readers.upgrade() else {
                break;
            };
            let mut buf = vec![0; INPUT_REPORT_MAX];
            let result = dev.blocking_lock().read_timeout(&mut buf, 100);
            match result {
                Ok(0) => {}
                Ok(n) => {
                    buf.truncate(n);
                    readers.push(QueuedReport {
                        endpoint,
                        data: buf,
                    });
                }
                Err(error) => {
                    log::debug!("[HidTransport] {} reader stopped: {error}", endpoint.name());
                    break;
                }
            }
        });
}

/// The raw HID handles gated behind [`Metered`]: a `short` handle and, for
/// devices that Windows splits into two collections, an optional `long`
/// handle.
struct HidState {
    primary: HidIo,
    companion: Option<HidIo>,
}

/// Async HID transport with optional platform-aware padding.
///
/// The primary and optional companion collections are addressed explicitly;
/// protocol code decides which one to use.
#[derive(Clone)]
pub struct HidTransport {
    io: Metered<HidState>,
    readers: Arc<HidReaders>,
    report_size: Option<usize>,
    timeout_ms: i32,
    /// When true, writes use `send_feature_report()` (HIDIOCSFEATURE) instead of
    /// `write()` (output report); for devices whose vendor interface only accepts
    /// feature reports.
    use_feature_report: bool,
}

impl HidTransport {
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
        let readers = Arc::new(HidReaders::new());
        spawn_reader(
            Arc::clone(&primary.read_dev),
            InputEndpoint::Primary,
            Arc::downgrade(&readers),
        );
        Ok(Self {
            io: Metered::new(
                HidState {
                    primary,
                    companion: None,
                },
                limit,
            ),
            readers,
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
        let readers = Arc::new(HidReaders::new());
        spawn_reader(
            Arc::clone(&primary.read_dev),
            InputEndpoint::Primary,
            Arc::downgrade(&readers),
        );
        if let Some(companion) = &companion {
            spawn_reader(
                Arc::clone(&companion.read_dev),
                InputEndpoint::Companion,
                Arc::downgrade(&readers),
            );
        }
        Ok(Self {
            io: Metered::new(HidState { primary, companion }, limit),
            readers,
            report_size,
            timeout_ms,
            use_feature_report,
        })
    }

    fn frame(&self, data: &[u8]) -> Vec<u8> {
        build_frame(data, self.report_size)
    }
}

#[async_trait]
impl Transport for HidTransport {
    async fn write(&self, data: &[u8]) -> Result<()> {
        let framed = self.frame(data);
        let state = self.io.write_access(framed.len()).await?;
        let dev = Arc::clone(&state.primary.write_dev);
        let use_feature_report = self.use_feature_report;
        tokio::task::spawn_blocking(move || {
            write_one(&dev.blocking_lock(), &framed, use_feature_report)
        })
        .await
        .context("spawn_blocking panicked")?
    }

    async fn read(&self, size: usize) -> Result<Vec<u8>> {
        let readers = self.readers.clone();
        let timeout_ms = self.timeout_ms;
        tokio::task::spawn_blocking(move || readers.pop(InputEndpoint::Primary, timeout_ms, size))
            .await
            .context("spawn_blocking panicked")
    }

    async fn write_then_read(&self, data: &[u8], size: usize) -> Result<Vec<u8>> {
        let framed = self.frame(data);
        let state = self.io.write_access(framed.len()).await?;
        let use_feature_report = self.use_feature_report;
        let write_dev = Arc::clone(&state.primary.write_dev);
        tokio::task::spawn_blocking(move || {
            let wguard = write_dev.blocking_lock();
            write_one(&wguard, &framed, use_feature_report)
        })
        .await
        .context("spawn_blocking panicked")??;
        self.read(size).await
    }

    async fn write_companion(&self, data: &[u8]) -> Result<()> {
        let framed = self.frame(data);
        let state = self.io.write_access(framed.len()).await?;
        let companion = state
            .companion
            .as_ref()
            .context("companion HID collection is not available")?;
        let dev = Arc::clone(&companion.write_dev);
        let use_feature_report = self.use_feature_report;
        tokio::task::spawn_blocking(move || {
            write_one(&dev.blocking_lock(), &framed, use_feature_report)
        })
        .await
        .context("spawn_blocking panicked")?
    }

    async fn read_companion(&self, size: usize) -> Result<Vec<u8>> {
        if self.io.read_access().companion.is_none() {
            anyhow::bail!("companion HID collection is not available");
        }
        let readers = self.readers.clone();
        let timeout_ms = self.timeout_ms;
        tokio::task::spawn_blocking(move || readers.pop(InputEndpoint::Companion, timeout_ms, size))
            .await
            .context("spawn_blocking panicked")
    }

    async fn write_then_read_companion(&self, data: &[u8], size: usize) -> Result<Vec<u8>> {
        let framed = self.frame(data);
        let state = self.io.write_access(framed.len()).await?;
        let companion = state
            .companion
            .as_ref()
            .context("companion HID collection is not available")?;
        let write_dev = Arc::clone(&companion.write_dev);
        let use_feature_report = self.use_feature_report;
        tokio::task::spawn_blocking(move || {
            let wguard = write_dev.blocking_lock();
            write_one(&wguard, &framed, use_feature_report)
        })
        .await
        .context("spawn_blocking panicked")??;
        self.read_companion(size).await
    }

    async fn write_many(&self, packets: &[Vec<u8>]) -> Result<()> {
        let total_len: usize = packets.iter().map(Vec::len).sum();
        let state = self.io.write_access(total_len).await?;

        let framed = packets.iter().map(|packet| self.frame(packet)).collect();
        write_batch(
            Arc::clone(&state.primary.write_dev),
            framed,
            self.use_feature_report,
        )
        .await
    }

    async fn write_many_companion(&self, packets: &[Vec<u8>]) -> Result<()> {
        let total_len: usize = packets.iter().map(Vec::len).sum();
        let state = self.io.write_access(total_len).await?;
        let companion = state
            .companion
            .as_ref()
            .context("companion HID collection is not available")?;
        let write_dev = Arc::clone(&companion.write_dev);
        let framed = packets.iter().map(|packet| self.frame(packet)).collect();
        write_batch(write_dev, framed, self.use_feature_report).await
    }

    /// Send a feature report and read back the response via `get_feature_report`.
    ///
    /// For drivers whose vendor interface answers `HIDIOCSFEATURE` via
    /// `HIDIOCGFEATURE` rather than the interrupt-IN endpoint. Both ioctls share
    /// the fd under the write lock, with a 1 ms gap for the device to process.
    ///
    /// The blocking ioctl is wrapped in a 500 ms timeout so a hung device
    /// doesn't permanently tie up a Tokio blocking thread (UH31). The rate
    /// gate runs before the timeout so a configured delay never reads as a
    /// hung device.
    async fn feature_exchange(&self, data: &[u8], response_size: usize) -> Result<Vec<u8>> {
        let framed = self.frame(data);
        let state = self.io.write_access(framed.len()).await?;
        let dev = Arc::clone(&state.primary.write_dev);
        timeout(Duration::from_millis(500), async {
            tokio::task::spawn_blocking(move || {
                let guard = dev.blocking_lock();
                guard
                    .send_feature_report(&framed)
                    .map_err(|e| anyhow::anyhow!("feature write error: {}", e))?;
                std::thread::sleep(Duration::from_millis(1));
                let mut buf = vec![0u8; response_size + 1];
                buf[0] = 0x00;
                let n = guard
                    .get_feature_report(&mut buf)
                    .map_err(|e| anyhow::anyhow!("feature read error: {}", e))?;
                buf.truncate(n);
                Ok(buf)
            })
            .await
            .context("spawn_blocking panicked")?
        })
        .await
        .map_err(|_| anyhow::anyhow!("feature exchange timed out"))?
    }

    async fn read_nonblocking(&self, size: usize) -> Result<Vec<u8>> {
        Ok(self.readers.pop(InputEndpoint::Primary, 0, size))
    }

    async fn read_any(&self, size: usize) -> Result<Vec<u8>> {
        let readers = self.readers.clone();
        let timeout_ms = self.timeout_ms;
        tokio::task::spawn_blocking(move || readers.pop_any(timeout_ms, size))
            .await
            .context("spawn_blocking panicked")
    }

    async fn defer_event(&self, data: &[u8]) -> Result<()> {
        self.readers.defer(data.to_vec());
        Ok(())
    }

    fn has_companion(&self) -> bool {
        self.io.read_access().companion.is_some()
    }

    fn event_receiver(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.readers.wake.subscribe())
    }

    async fn drain_events(&self, limit: usize) -> Result<Vec<TransportEvent>> {
        Ok(self.readers.drain(limit))
    }

    fn rate_status(&self) -> WriteRateStatus {
        self.io.status()
    }

    fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.io.set_limit(limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_queue_is_bounded_and_drops_the_oldest_report() {
        let readers = HidReaders::new();
        for value in 0..=INPUT_QUEUE_CAPACITY {
            readers.push(QueuedReport {
                endpoint: InputEndpoint::Primary,
                data: vec![value as u8],
            });
        }
        let drained = readers.drain(INPUT_QUEUE_CAPACITY + 1);
        assert_eq!(drained.len(), INPUT_QUEUE_CAPACITY);
        assert_eq!(drained[0].data, vec![1]);
    }

    #[test]
    fn endpoint_reads_preserve_other_endpoint_reports_for_event_drain() {
        let readers = HidReaders::new();
        readers.push(QueuedReport {
            endpoint: InputEndpoint::Companion,
            data: vec![0x11],
        });
        readers.push(QueuedReport {
            endpoint: InputEndpoint::Primary,
            data: vec![0x10],
        });
        assert_eq!(readers.pop(InputEndpoint::Primary, 0, 64), vec![0x10]);
        let remaining = readers.drain(8);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].endpoint, "companion");
        assert_eq!(remaining[0].data, vec![0x11]);
    }

    // `pop_any` merges both endpoints so a reply is matched wherever the
    // short/long collection split delivered it, in arrival (FIFO) order.
    #[test]
    fn pop_any_returns_next_report_from_either_endpoint_in_order() {
        let readers = HidReaders::new();
        readers.push(QueuedReport {
            endpoint: InputEndpoint::Companion,
            data: vec![0x11, 0xaa],
        });
        readers.push(QueuedReport {
            endpoint: InputEndpoint::Primary,
            data: vec![0x10, 0xbb],
        });
        assert_eq!(readers.pop_any(0, 64), vec![0x11, 0xaa]);
        assert_eq!(readers.pop_any(0, 64), vec![0x10, 0xbb]);
        assert!(readers.pop_any(0, 64).is_empty());
    }

    #[test]
    fn pop_any_truncates_to_size() {
        let readers = HidReaders::new();
        readers.push(QueuedReport {
            endpoint: InputEndpoint::Primary,
            data: vec![1, 2, 3, 4],
        });
        assert_eq!(readers.pop_any(0, 2), vec![1, 2]);
    }

    // A deferred report is delivered ahead of newer live reports so arrival
    // order is preserved, and it must never come back through `pop_any`.
    #[test]
    fn deferred_reports_drain_before_live_and_not_via_pop_any() {
        let readers = HidReaders::new();
        readers.defer(vec![0xde, 0xad]);
        readers.push(QueuedReport {
            endpoint: InputEndpoint::Primary,
            data: vec![0x10, 0x01],
        });
        // A deferred packet is for the event path only, never a request read.
        assert_eq!(readers.pop_any(0, 64), vec![0x10, 0x01]);
        let drained = readers.drain(8);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].endpoint, DEFERRED_ENDPOINT);
        assert_eq!(drained[0].data, vec![0xde, 0xad]);
    }

    #[test]
    fn defer_bumps_the_event_wake_watch() {
        let readers = HidReaders::new();
        let mut rx = readers.wake.subscribe();
        assert!(!rx.has_changed().unwrap());
        readers.defer(vec![0x01]);
        assert!(rx.has_changed().unwrap());
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
