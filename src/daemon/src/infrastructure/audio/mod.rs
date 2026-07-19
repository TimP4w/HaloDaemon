// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, OnceLock, RwLock,
};
use std::time::Instant;

pub mod dsp;
pub mod sink;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

pub const BANDS: usize = 64;

/// Minimum interval between capture-backend spawn attempts and between
/// in-thread session retries. Bounds (re)connect churn against the audio
/// server no matter how often effects are rebuilt.
const SPAWN_INTERVAL_MS: u64 = 5_000;
pub(crate) const SESSION_RETRY_MS: u64 = 5_000;
/// The capture backend exits after this long without a `latest()` reader.
const IDLE_EXIT_MS: u64 = 30_000;

/// Milliseconds since the module's first use, as a monotonic timestamp.
/// Distinct from `crate::util::time::now_ms` (wall-clock, Unix epoch) — this clock is
/// only meaningful for measuring elapsed intervals, not absolute time.
pub(crate) fn monotonic_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

#[derive(Clone, Copy)]
pub struct SpectrumFrame {
    pub bands: [f32; BANDS],
    pub level: f32,
    pub flux: f32,
    pub beat: bool,
    pub seq: u64,
}

impl Default for SpectrumFrame {
    fn default() -> Self {
        Self {
            bands: [0.0; BANDS],
            level: 0.0,
            flux: 0.0,
            beat: false,
            seq: 0,
        }
    }
}

pub struct AudioHandle {
    latest: RwLock<SpectrumFrame>,
    running: AtomicBool,
    next_spawn_ms: AtomicU64,
    last_read_ms: AtomicU64,
}

impl AudioHandle {
    fn new() -> Self {
        Self {
            latest: RwLock::new(SpectrumFrame::default()),
            running: AtomicBool::new(false),
            next_spawn_ms: AtomicU64::new(0),
            last_read_ms: AtomicU64::new(0),
        }
    }

    pub fn latest(&self) -> SpectrumFrame {
        self.last_read_ms.store(monotonic_ms(), Ordering::Relaxed);
        *self.latest.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Called by the platform capture thread.
    fn publish(&self, frame: SpectrumFrame) {
        *self.latest.write().unwrap_or_else(|e| e.into_inner()) = frame;
    }

    /// True once no consumer has polled `latest()` for [`IDLE_EXIT_MS`] —
    /// the platform backend's signal to disconnect and exit.
    pub(crate) fn idle_expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_read_ms.load(Ordering::Relaxed)) > IDLE_EXIT_MS
    }

    /// Marks the backend stopped; a later `shared()` call past the rate
    /// limit may spawn a new one.
    pub(crate) fn capture_stopped(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Claims the right to spawn the capture backend: false while one is
    /// running or within [`SPAWN_INTERVAL_MS`] of the previous spawn.
    fn claim_spawn(&self, now_ms: u64) -> bool {
        if now_ms < self.next_spawn_ms.load(Ordering::Relaxed) {
            return false;
        }
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.next_spawn_ms
            .store(now_ms + SPAWN_INTERVAL_MS, Ordering::Relaxed);
        // Fresh idle window so the new backend isn't reaped before the first
        // consumer read.
        self.last_read_ms.store(now_ms, Ordering::Relaxed);
        true
    }
}

/// Marks the backend stopped when the platform thread exits, on every path.
pub(crate) struct StopGuard(pub(crate) Arc<AudioHandle>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.capture_stopped();
        log::info!("Audio capture: backend thread exiting");
    }
}

/// Process-wide handle. The capture backend runs only while consumers poll
/// `latest()`: it exits after [`IDLE_EXIT_MS`] without a reader, retries
/// failed sessions in-thread while readers remain, and spawn attempts are
/// rate-limited to one per [`SPAWN_INTERVAL_MS`] so effect-rebuild churn
/// cannot reconnect-storm the audio server.
pub fn shared() -> Arc<AudioHandle> {
    static HANDLE: OnceLock<Arc<AudioHandle>> = OnceLock::new();
    let handle = HANDLE.get_or_init(|| Arc::new(AudioHandle::new()));
    if handle.claim_spawn(monotonic_ms()) {
        start_platform(Arc::clone(handle));
    }
    Arc::clone(handle)
}

/// Dispatches to the platform capture backend. Must return immediately
/// (spawn a `std::thread`) and call `capture_stopped` on every exit path
/// (see [`StopGuard`]).
fn start_platform(handle: Arc<AudioHandle>) {
    #[cfg(target_os = "linux")]
    linux::start(handle);

    #[cfg(target_os = "windows")]
    windows::start(handle);

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        handle.capture_stopped();
        log::error!("audio capture unsupported on this platform");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn default_spectrum_frame_is_silent() {
        let f = SpectrumFrame::default();
        assert!(f.bands.iter().all(|&b| b == 0.0));
        assert_eq!(f.level, 0.0);
        assert_eq!(f.flux, 0.0);
        assert!(!f.beat);
        assert_eq!(f.seq, 0);
    }

    #[test]
    fn publish_then_latest_round_trips() {
        let handle = AudioHandle::new();
        let frame = SpectrumFrame {
            level: 0.75,
            seq: 42,
            beat: true,
            ..SpectrumFrame::default()
        };
        handle.publish(frame);

        let got = handle.latest();
        assert_eq!(got.level, 0.75);
        assert_eq!(got.seq, 42);
        assert!(got.beat);
    }

    #[test]
    fn shared_returns_the_same_arc() {
        let a = shared();
        let b = shared();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn claim_spawn_blocks_while_running_and_within_rate_limit() {
        let h = AudioHandle::new();
        assert!(h.claim_spawn(0));
        assert!(!h.claim_spawn(SPAWN_INTERVAL_MS + 1), "still running");

        h.capture_stopped();
        assert!(!h.claim_spawn(SPAWN_INTERVAL_MS - 1), "rate-limited");
        assert!(h.claim_spawn(SPAWN_INTERVAL_MS));
    }

    #[test]
    fn latest_read_defers_idle_expiry() {
        let h = AudioHandle::new();
        let now = monotonic_ms();
        h.latest();
        assert!(!h.idle_expired(now + IDLE_EXIT_MS));
        assert!(h.idle_expired(now + 2 * IDLE_EXIT_MS + 1));
    }

    proptest! {
        // Whatever interleaving of backend stops and spawn attempts occurs,
        // successful spawns are never closer than SPAWN_INTERVAL_MS apart
        // and never overlap a running backend.
        #[test]
        fn spawns_are_rate_limited(
            ops in prop::collection::vec((0u64..2_000, any::<bool>()), 1..100)
        ) {
            let h = AudioHandle::new();
            let mut now = 0u64;
            let mut last_spawn: Option<u64> = None;
            let mut running = false;
            for (dt, stop) in ops {
                now += dt;
                if stop {
                    h.capture_stopped();
                    running = false;
                }
                if h.claim_spawn(now) {
                    prop_assert!(!running, "spawned while a backend was running");
                    if let Some(prev) = last_spawn {
                        prop_assert!(now - prev >= SPAWN_INTERVAL_MS);
                    }
                    last_spawn = Some(now);
                    running = true;
                }
            }
        }
    }
}
