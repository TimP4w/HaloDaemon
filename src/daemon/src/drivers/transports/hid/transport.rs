use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::drivers::transports::Transport;
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

/// Default routing hook: never route to the long handle. Vendor protocols that
/// split reports across two collections supply their own via `with_routing`.
fn route_short_only(_report_id: u8) -> bool {
    false
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

/// Read one packet from `dev` with `timeout_ms`. Empty vec on timeout.
async fn read_io(
    dev: Arc<Mutex<hidapi::HidDevice>>,
    size: usize,
    timeout_ms: i32,
) -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let guard = dev.blocking_lock();
        let mut buf = vec![0u8; size];
        let n = guard
            .read_timeout(&mut buf, timeout_ms)
            .map_err(|e| anyhow::anyhow!("HID read error: {}", e))?;
        buf.truncate(n);
        Ok(buf)
    })
    .await
    .context("spawn_blocking panicked")?
}

/// The raw HID handles gated behind [`Metered`]: a `short` handle and, for
/// devices that Windows splits into two collections, an optional `long`
/// handle.
struct HidState {
    short: HidIo,
    long: Option<HidIo>,
}

/// Async HID transport with optional platform-aware padding.
///
/// Writes route by report ID via the `route_to_long` hook (supplied by the
/// protocol, see [`Self::with_routing`]); single-handle devices and the
/// identity hook always use `short`.
#[derive(Clone)]
pub struct HidTransport {
    io: Metered<HidState>,
    report_size: Option<usize>,
    timeout_ms: i32,
    /// When true, writes use `send_feature_report()` (HIDIOCSFEATURE) instead of
    /// `write()` (output report); for devices whose vendor interface only accepts
    /// feature reports.
    use_feature_report: bool,
    /// Per-device routing: true means a packet with this report ID must use the
    /// `long` handle. Defaults to [`route_short_only`] (identity / short-only);
    /// the protocol that opened a dual handle supplies the real predicate.
    route_to_long: fn(u8) -> bool,
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
        let short = HidIo::open(&api, path)?;
        Ok(Self {
            io: Metered::new(HidState { short, long: None }, limit),
            report_size,
            timeout_ms,
            use_feature_report,
            route_to_long: route_short_only,
        })
    }

    /// Open a device that Windows splits into two HID collections.
    ///
    /// `short_path` carries short reports; `long_path` carries long reports. When
    /// `long_path` is empty or equal to `short_path` — Linux hidraw exposes one
    /// node carrying both report IDs — only the short handle is opened and the
    /// transport behaves exactly like `open`.
    ///
    /// The returned transport routes every packet to `short` until the protocol
    /// installs a routing predicate via [`Self::with_routing`].
    pub fn open_dual(
        short_path: &str,
        long_path: &str,
        report_size: Option<usize>,
        timeout_ms: i32,
        use_feature_report: bool,
        limit: Option<WriteRateLimit>,
    ) -> Result<Self> {
        let api = hidapi::HidApi::new().context("failed to create HidApi")?;
        let short = HidIo::open(&api, short_path)?;
        let long = if long_path.is_empty() || long_path == short_path {
            None
        } else {
            Some(HidIo::open(&api, long_path)?)
        };
        Ok(Self {
            io: Metered::new(HidState { short, long }, limit),
            report_size,
            timeout_ms,
            use_feature_report,
            route_to_long: route_short_only,
        })
    }

    /// Install the per-device report-ID routing predicate (true → long handle).
    /// Only takes effect when a long handle is open; routes to `short` otherwise.
    #[must_use]
    pub fn with_routing(mut self, route_to_long: fn(u8) -> bool) -> Self {
        self.route_to_long = route_to_long;
        self
    }

    /// True when `report_id` must use the long handle (and one is open).
    fn routes_long(&self, state: &HidState, report_id: u8) -> bool {
        state.long.is_some() && (self.route_to_long)(report_id)
    }

    /// Pick the handle a packet with `report_id` must use.
    fn pick_io<'a>(&self, state: &'a HidState, report_id: u8) -> &'a HidIo {
        if self.routes_long(state, report_id) {
            state
                .long
                .as_ref()
                .expect("routes_long guarantees a long handle")
        } else {
            &state.short
        }
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
        let report_id = framed.first().copied().unwrap_or(0);
        let dev = Arc::clone(&self.pick_io(state, report_id).write_dev);
        let use_feature_report = self.use_feature_report;
        tokio::task::spawn_blocking(move || {
            write_one(&dev.blocking_lock(), &framed, use_feature_report)
        })
        .await
        .context("spawn_blocking panicked")?
    }

    async fn read(&self, size: usize) -> Result<Vec<u8>> {
        read_io(
            Arc::clone(&self.io.read_access().short.read_dev),
            size,
            self.timeout_ms,
        )
        .await
    }

    async fn write_then_read(&self, data: &[u8], size: usize) -> Result<Vec<u8>> {
        let framed = self.frame(data);
        let state = self.io.write_access(framed.len()).await?;
        let report_id = framed.first().copied().unwrap_or(0);
        let timeout_ms = self.timeout_ms;
        let use_feature_report = self.use_feature_report;
        let io = self.pick_io(state, report_id);
        let write_dev = Arc::clone(&io.write_dev);
        let read_dev = Arc::clone(&io.read_dev);
        tokio::task::spawn_blocking(move || {
            let wguard = write_dev.blocking_lock();
            write_one(&wguard, &framed, use_feature_report)?;
            drop(wguard);
            let rguard = read_dev.blocking_lock();
            let mut buf = vec![0u8; size];
            let n = rguard
                .read_timeout(&mut buf, timeout_ms)
                .map_err(|e| anyhow::anyhow!("HID read error: {}", e))?;
            buf.truncate(n);
            Ok(buf)
        })
        .await
        .context("spawn_blocking panicked")?
    }

    async fn write_many(&self, packets: &[Vec<u8>]) -> Result<()> {
        let total_len: usize = packets.iter().map(Vec::len).sum();
        let state = self.io.write_access(total_len).await?;

        // Partition framed packets by destination handle, preserving per-handle order.
        let mut short_pkts: Vec<Vec<u8>> = Vec::new();
        let mut long_pkts: Vec<Vec<u8>> = Vec::new();
        for p in packets {
            let framed = self.frame(p);
            let report_id = framed.first().copied().unwrap_or(0);
            if self.routes_long(state, report_id) {
                long_pkts.push(framed);
            } else {
                short_pkts.push(framed);
            }
        }
        if !short_pkts.is_empty() {
            write_batch(
                Arc::clone(&state.short.write_dev),
                short_pkts,
                self.use_feature_report,
            )
            .await?;
        }
        if !long_pkts.is_empty() {
            if let Some(long) = &state.long {
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
        let dev = Arc::clone(&state.short.write_dev);
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
        read_io(Arc::clone(&self.io.read_access().short.read_dev), size, 0).await
    }

    /// Read one packet from the long-report handle.
    ///
    /// Returns an empty vec when no long handle is open (single-handle / Linux);
    /// callers must guard with `has_long_handle` to avoid a tight spin loop.
    async fn read_long(&self, size: usize) -> Result<Vec<u8>> {
        match &self.io.read_access().long {
            Some(long) => read_io(Arc::clone(&long.read_dev), size, self.timeout_ms).await,
            None => Ok(Vec::new()),
        }
    }

    fn has_long_handle(&self) -> bool {
        self.io.read_access().long.is_some()
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

    /// Logitech HID++ long-report report ID — the value a device would pass to
    /// `with_routing`. Defined here only to exercise the routing hook.
    const HIDPP_LONG_REPORT_ID: u8 = 0x11;
    fn route_hidpp_long(report_id: u8) -> bool {
        report_id == HIDPP_LONG_REPORT_ID
    }

    #[test]
    fn frame_raw_passthrough() {
        assert_eq!(build_frame(&[0x10, 0x02], None), vec![0x10, 0x02]);
    }

    #[test]
    fn default_routing_is_short_only() {
        // The identity default never routes to long, regardless of report ID.
        assert!(!route_short_only(0x11));
        assert!(!route_short_only(0x10));
        assert!(!route_short_only(0x00));
    }

    #[test]
    fn installed_routing_hook_selects_long_reports() {
        // A protocol-supplied predicate routes its long report ID and nothing else.
        assert!(route_hidpp_long(0x11));
        assert!(!route_hidpp_long(0x10));
        assert!(!route_hidpp_long(0x00));
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
