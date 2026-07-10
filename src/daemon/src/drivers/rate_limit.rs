// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-device write-rate enforcement, generic over any transport.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use halod_shared::types::{WriteRateLimit, WriteRateStatus};

/// Reject a write once more than this many seconds' worth of bytes are queued.
const SAFETY_VALVE_SECS: u64 = 2;

const STATS_WINDOW: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Acquire {
    Ready,
    WaitFor(Duration),
}

/// Token bucket: refills at `rate` bytes/sec up to `capacity`. Takes `now`
/// explicitly so it's deterministic without real time.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(now: Instant) -> Self {
        Self {
            tokens: 1.0,
            last_refill: now,
        }
    }

    fn try_acquire(&mut self, rate: f64, capacity: f64, cost: f64, now: Instant) -> Acquire {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate).min(capacity);
        self.last_refill = now;
        let threshold = cost.min(capacity);
        if self.tokens >= threshold {
            self.tokens -= threshold;
            Acquire::Ready
        } else {
            let deficit = threshold - self.tokens;
            let wait_secs = deficit / rate;
            Acquire::WaitFor(Duration::from_secs_f64(wait_secs).max(Duration::from_nanos(1)))
        }
    }
}

fn exceeds_safety_valve(queued_bytes: u64, rate: u32) -> bool {
    queued_bytes >= (rate as u64).saturating_mul(SAFETY_VALVE_SECS).max(1)
}

fn is_stale(t: Instant, now: Instant, window: Duration) -> bool {
    now.duration_since(t) > window
}

/// Result of one [`WriteRateLimiter::gate_once`] decision.
#[derive(Debug, Clone, Copy, PartialEq)]
enum GateOutcome {
    Ready,
    Wait(Duration),
    Rejected,
}

/// Per-device write-rate enforcement. `None` means unthrottled — writes are
/// recorded for stats but never delayed or rejected. When a limit is set,
/// writes are delayed (FIFO) rather than dropped; only a sustained flood
/// beyond the safety valve is rejected.
pub struct WriteRateLimiter {
    bucket: Mutex<TokenBucket>,
    limit_bytes_per_sec: AtomicU32,
    queued_bytes: AtomicU64,
    rejected_total: AtomicU64,
    recent: Mutex<VecDeque<(Instant, usize)>>,
}

impl WriteRateLimiter {
    pub fn new(limit: Option<WriteRateLimit>) -> Self {
        let now = Instant::now();
        Self {
            bucket: Mutex::new(TokenBucket::new(now)),
            limit_bytes_per_sec: AtomicU32::new(Self::encode(limit)),
            queued_bytes: AtomicU64::new(0),
            rejected_total: AtomicU64::new(0),
            recent: Mutex::new(VecDeque::new()),
        }
    }

    fn encode(limit: Option<WriteRateLimit>) -> u32 {
        limit.map(|l| l.max_bytes_per_sec.max(1)).unwrap_or(0)
    }

    pub fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.limit_bytes_per_sec
            .store(Self::encode(limit), Ordering::Relaxed);
    }

    /// One gating decision for a write of `len` bytes: whether it's admitted
    /// now, must wait `Duration` before retrying, or the safety valve has
    /// already been tripped by other queued waiters. Shared by the async and
    /// blocking `acquire` variants so both sleep flavors gate identically.
    fn gate_once(&self, len: usize, rate: u32) -> GateOutcome {
        let outcome = {
            let mut bucket = self.bucket.lock().expect("rate limiter bucket poisoned");
            bucket.try_acquire(rate as f64, rate as f64, len as f64, Instant::now())
        };
        match outcome {
            Acquire::Ready => GateOutcome::Ready,
            Acquire::WaitFor(d) => {
                if exceeds_safety_valve(self.queued_bytes.load(Ordering::Relaxed), rate) {
                    self.rejected_total.fetch_add(1, Ordering::Relaxed);
                    GateOutcome::Rejected
                } else {
                    self.queued_bytes.fetch_add(len as u64, Ordering::Relaxed);
                    GateOutcome::Wait(d)
                }
            }
        }
    }

    /// Admit one write of `len` bytes, delaying until the rate allows it.
    /// With no limit, admits immediately. Errors only when the safety valve
    /// is exceeded.
    pub async fn acquire(&self, len: usize) -> Result<()> {
        let rate = self.limit_bytes_per_sec.load(Ordering::Relaxed);
        if rate == 0 {
            // Unthrottled: still record the write so bytes/sec stays visible.
            self.record(len);
            return Ok(());
        }

        loop {
            match self.gate_once(len, rate) {
                GateOutcome::Ready => break,
                GateOutcome::Rejected => bail!("write rate limit exceeded ({rate} bytes/sec)"),
                GateOutcome::Wait(d) => {
                    tokio::time::sleep(d).await;
                    self.queued_bytes.fetch_sub(len as u64, Ordering::Relaxed);
                }
            }
        }

        self.record(len);
        Ok(())
    }

    /// Blocking twin of [`Self::acquire`] for transports whose write path is
    /// synchronous (USB bulk/control transfers): sleeps the calling thread
    /// rather than awaiting. Only call this from a thread that's allowed to
    /// block — a `spawn_blocking` worker, or a transport whose contract
    /// already documents blocking I/O on the caller's thread.
    pub fn acquire_blocking(&self, len: usize) -> Result<()> {
        let rate = self.limit_bytes_per_sec.load(Ordering::Relaxed);
        if rate == 0 {
            self.record(len);
            return Ok(());
        }

        loop {
            match self.gate_once(len, rate) {
                GateOutcome::Ready => break,
                GateOutcome::Rejected => bail!("write rate limit exceeded ({rate} bytes/sec)"),
                GateOutcome::Wait(d) => {
                    std::thread::sleep(d);
                    self.queued_bytes.fetch_sub(len as u64, Ordering::Relaxed);
                }
            }
        }

        self.record(len);
        Ok(())
    }

    /// Record a write for live stats without rate-gating it.
    pub fn record(&self, len: usize) {
        let now = Instant::now();
        let mut recent = self
            .recent
            .lock()
            .expect("rate limiter recent-writes lock poisoned");
        recent.push_back((now, len));
        while let Some(&(t, _)) = recent.front() {
            if is_stale(t, now, STATS_WINDOW) {
                recent.pop_front();
            } else {
                break;
            }
        }
    }

    /// Combine two statuses for a device writing through multiple transports.
    pub fn combine_status(a: WriteRateStatus, b: WriteRateStatus) -> WriteRateStatus {
        WriteRateStatus {
            limit: a.limit.or(b.limit),
            current_writes_per_sec: a.current_writes_per_sec + b.current_writes_per_sec,
            current_bytes_per_sec: a.current_bytes_per_sec + b.current_bytes_per_sec,
            rejected_total: a.rejected_total + b.rejected_total,
        }
    }

    pub fn status(&self) -> WriteRateStatus {
        let now = Instant::now();
        let recent = self
            .recent
            .lock()
            .expect("rate limiter recent-writes lock poisoned");
        let (writes, bytes) = recent
            .iter()
            .filter(|(t, _)| !is_stale(*t, now, STATS_WINDOW))
            .fold((0u32, 0u64), |(w, b), (_, len)| (w + 1, b + *len as u64));
        let rate = self.limit_bytes_per_sec.load(Ordering::Relaxed);
        WriteRateStatus {
            limit: (rate > 0).then_some(WriteRateLimit {
                max_bytes_per_sec: rate,
            }),
            current_writes_per_sec: writes as f32,
            current_bytes_per_sec: bytes as f32,
            rejected_total: self.rejected_total.load(Ordering::Relaxed),
        }
    }
}

/// Owns raw transport I/O state `T` behind a write-rate gate: the only routes
/// to `T` are the metered write accessors, the greppable unmetered
/// `read_access`, and the tallying batch gate. A transport built on `Metered`
/// cannot write bytes without metering them, by construction rather than by
/// convention.
pub struct Metered<T> {
    inner: Arc<MeteredInner<T>>,
}

struct MeteredInner<T> {
    io: T,
    limiter: WriteRateLimiter,
}

impl<T> Clone for Metered<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> Metered<T> {
    pub fn new(io: T, limit: Option<WriteRateLimit>) -> Self {
        Self {
            inner: Arc::new(MeteredInner {
                io,
                limiter: WriteRateLimiter::new(limit),
            }),
        }
    }

    /// Meter `len` bytes, delaying (or rejecting a sustained flood) as the
    /// declared limit demands, then grant access to the raw I/O for the write.
    pub async fn write_access(&self, len: usize) -> Result<&T> {
        self.inner.limiter.acquire(len).await?;
        Ok(&self.inner.io)
    }

    /// Blocking twin of [`Self::write_access`]: sleeps the calling thread
    /// instead of awaiting. Only call from a `spawn_blocking` worker, or a
    /// transport whose contract already documents blocking I/O on the
    /// caller's thread.
    pub fn write_access_blocking(&self, len: usize) -> Result<&T> {
        self.inner.limiter.acquire_blocking(len)?;
        Ok(&self.inner.io)
    }

    /// Unmetered access for reads — reads are never rate-limited. Any write
    /// issued through this instead of `write_access`/`write_access_blocking`
    /// is a bug; the name is deliberately greppable.
    pub fn read_access(&self) -> &T {
        &self.inner.io
    }

    pub fn status(&self) -> WriteRateStatus {
        self.inner.limiter.status()
    }

    pub fn set_limit(&self, limit: Option<WriteRateLimit>) {
        self.inner.limiter.set_write_rate_limit(limit);
    }

    /// Recovers the raw I/O, consuming the gate. `None` if other clones of
    /// this gate are still alive.
    pub fn into_inner(self) -> Option<T> {
        Arc::try_unwrap(self.inner).ok().map(|inner| inner.io)
    }
}

impl<T: Send + Sync + 'static> Metered<T> {
    /// Post-hoc gate for batch transports whose written-byte count is only
    /// known after the operations run (e.g. SMBus, where individual register
    /// ops don't know the batch total up front): runs `f` on a blocking
    /// thread with a byte tally, then meters the tallied bytes. A meter
    /// rejection takes precedence over `f`'s own result.
    pub async fn write_tallied<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&T, &AtomicUsize) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        let bytes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&bytes);
        let result = tokio::task::spawn_blocking(move || f(&inner.io, &counter))
            .await
            .map_err(|e| anyhow!("spawn_blocking panicked: {e}"))?;
        self.inner
            .limiter
            .acquire(bytes.load(Ordering::Relaxed))
            .await?;
        result
    }

    /// Inline twin of [`Self::write_tallied`]: runs `f` on the calling thread
    /// (no `spawn_blocking`, no `Send`/`'static` bound) and meters the tallied
    /// bytes through the blocking gate. Use when the batch must call back into
    /// non-`Send` state (e.g. a plugin's Lua VM) and the caller is already on a
    /// thread allowed to block. A meter rejection takes precedence over `f`'s
    /// own result.
    pub fn write_tallied_local<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&T, &AtomicUsize) -> Result<R>,
    {
        let bytes = AtomicUsize::new(0);
        let result = f(&self.inner.io, &bytes);
        self.inner
            .limiter
            .acquire_blocking(bytes.load(Ordering::Relaxed))?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn first_write_is_never_delayed() {
        let now = Instant::now();
        let mut bucket = TokenBucket::new(now);
        assert_eq!(bucket.try_acquire(30.0, 30.0, 1.0, now), Acquire::Ready);
    }

    #[test]
    fn second_immediate_write_waits_for_refill() {
        let now = Instant::now();
        let mut bucket = TokenBucket::new(now);
        assert_eq!(bucket.try_acquire(2.0, 2.0, 1.0, now), Acquire::Ready);
        match bucket.try_acquire(2.0, 2.0, 1.0, now) {
            Acquire::WaitFor(d) => assert!(d > Duration::ZERO && d <= Duration::from_secs(1)),
            Acquire::Ready => panic!("expected the second write at rate=2 to wait"),
        }
    }

    #[test]
    fn wait_for_duration_is_never_zero_even_for_a_vanishing_deficit() {
        let now = Instant::now();
        let mut bucket = TokenBucket::new(now);
        bucket.tokens = 1.0 - f64::EPSILON;
        match bucket.try_acquire(200.0, 200.0, 1.0, now) {
            Acquire::WaitFor(d) => assert!(d >= Duration::from_nanos(1)),
            Acquire::Ready => {}
        }
    }

    #[test]
    fn refill_after_enough_elapsed_time_admits_again() {
        let now = Instant::now();
        let mut bucket = TokenBucket::new(now);
        assert_eq!(bucket.try_acquire(1.0, 1.0, 1.0, now), Acquire::Ready);
        let later = now + Duration::from_secs(2);
        assert_eq!(bucket.try_acquire(1.0, 1.0, 1.0, later), Acquire::Ready);
    }

    #[test]
    fn oversized_single_write_resolves_instead_of_starving_forever() {
        let now = Instant::now();
        let mut bucket = TokenBucket::new(now);
        let (rate, capacity, cost) = (10.0, 10.0, 1_000.0);

        let mut t = now;
        let mut retries = 0;
        loop {
            match bucket.try_acquire(rate, capacity, cost, t) {
                Acquire::Ready => break,
                Acquire::WaitFor(d) => {
                    t += d;
                    retries += 1;
                    assert!(retries < 1000, "oversized write never resolved to Ready");
                }
            }
        }
    }

    #[test]
    fn safety_valve_threshold() {
        assert!(!exceeds_safety_valve(0, 30));
        assert!(!exceeds_safety_valve(59, 30));
        assert!(exceeds_safety_valve(60, 30));
        assert!(exceeds_safety_valve(120, 30));
        assert!(!exceeds_safety_valve(0, 0));
        assert!(exceeds_safety_valve(1, 0));
    }

    #[test]
    fn is_stale_boundary() {
        let t0 = Instant::now();
        assert!(!is_stale(t0, t0, STATS_WINDOW), "no time elapsed");
        assert!(
            !is_stale(t0, t0 + STATS_WINDOW, STATS_WINDOW),
            "exactly at the window edge is still fresh"
        );
        assert!(
            is_stale(
                t0,
                t0 + STATS_WINDOW + Duration::from_nanos(1),
                STATS_WINDOW
            ),
            "one nanosecond past the window is stale"
        );
    }

    proptest! {
        #[test]
        fn granted_writes_never_exceed_rate_in_any_window(
            rate in 1u32..200,
            burst_count in 1usize..300,
        ) {
            let capacity = rate as f64;
            let start = Instant::now();
            let mut bucket = TokenBucket::new(start);
            let mut t = start;
            let mut grants: Vec<Instant> = Vec::new();

            for _ in 0..burst_count {
                let mut retries = 0;
                loop {
                    match bucket.try_acquire(rate as f64, capacity, 1.0, t) {
                        Acquire::Ready => {
                            grants.push(t);
                            break;
                        }
                        Acquire::WaitFor(d) => {
                            t += d;
                            retries += 1;
                            prop_assert!(retries < 10_000, "try_acquire did not converge");
                        }
                    }
                }
            }

            for i in 0..grants.len() {
                let window_end = grants[i] + Duration::from_secs(1);
                let count = grants[i..].iter().take_while(|&&g| g <= window_end).count();
                prop_assert!(
                    count as u32 <= rate + 1,
                    "window starting at grant {} allowed {} writes for rate {}",
                    i,
                    count,
                    rate
                );
            }
        }
    }

    #[tokio::test]
    async fn unthrottled_when_no_limit_is_set() {
        let limiter = WriteRateLimiter::new(None);
        for _ in 0..1000 {
            limiter.acquire(1_000_000).await.unwrap();
        }
        let status = limiter.status();
        assert_eq!(status.limit, None);
        assert_eq!(status.rejected_total, 0);
        assert!(status.current_bytes_per_sec > 0.0, "stats still tracked");
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_delays_rather_than_rejects_under_the_safety_valve() {
        let limiter = WriteRateLimiter::new(Some(WriteRateLimit {
            max_bytes_per_sec: 2,
        }));
        limiter.acquire(1).await.unwrap();
        let before = Instant::now();
        limiter.acquire(1).await.unwrap();
        assert!(Instant::now() >= before + Duration::from_millis(400));
    }

    #[tokio::test(start_paused = true)]
    async fn sustained_flood_eventually_rejects_without_growing_forever() {
        let limiter = std::sync::Arc::new(WriteRateLimiter::new(Some(WriteRateLimit {
            max_bytes_per_sec: 1,
        })));
        let mut handles = Vec::new();
        for _ in 0..50 {
            let limiter = limiter.clone();
            handles.push(tokio::spawn(async move { limiter.acquire(1).await }));
        }

        let mut ok = 0;
        let mut err = 0;
        for h in handles {
            match h.await.unwrap() {
                Ok(()) => ok += 1,
                Err(_) => err += 1,
            }
        }

        assert!(ok > 0, "at least one write should be admitted");
        assert!(
            err > 0,
            "a sustained flood should eventually be rejected rather than queueing forever"
        );
        assert_eq!(ok + err, 50);
        assert_eq!(limiter.status().rejected_total, err as u64);
    }

    #[test]
    fn combine_status_sums_counters_and_keeps_the_declared_limit() {
        let hid = WriteRateStatus {
            limit: Some(WriteRateLimit {
                max_bytes_per_sec: 30,
            }),
            current_writes_per_sec: 2.0,
            current_bytes_per_sec: 128.0,
            rejected_total: 3,
        };
        let bulk = WriteRateStatus {
            limit: None,
            current_writes_per_sec: 24.0,
            current_bytes_per_sec: 1_228_800.0,
            rejected_total: 2,
        };
        let combined = WriteRateLimiter::combine_status(hid, bulk);
        assert_eq!(
            combined.limit,
            Some(WriteRateLimit {
                max_bytes_per_sec: 30
            })
        );
        assert_eq!(combined.current_writes_per_sec, 26.0);
        assert_eq!(combined.current_bytes_per_sec, 1_228_928.0);
        assert_eq!(combined.rejected_total, 5);

        // The limit is taken from whichever side declares one.
        let flipped = WriteRateLimiter::combine_status(
            bulk,
            WriteRateStatus {
                limit: Some(WriteRateLimit {
                    max_bytes_per_sec: 7,
                }),
                current_writes_per_sec: 0.0,
                current_bytes_per_sec: 0.0,
                rejected_total: 0,
            },
        );
        assert_eq!(
            flipped.limit,
            Some(WriteRateLimit {
                max_bytes_per_sec: 7
            })
        );
    }

    #[test]
    fn status_reports_configured_limit_and_zero_stats_when_idle() {
        let limiter = WriteRateLimiter::new(Some(WriteRateLimit {
            max_bytes_per_sec: 15,
        }));
        let status = limiter.status();
        assert_eq!(
            status.limit,
            Some(WriteRateLimit {
                max_bytes_per_sec: 15
            })
        );
        assert_eq!(status.current_writes_per_sec, 0.0);
        assert_eq!(status.current_bytes_per_sec, 0.0);
        assert_eq!(status.rejected_total, 0);
    }

    #[test]
    fn status_tallies_write_count_and_byte_count_independently() {
        let limiter = WriteRateLimiter::new(None);
        limiter.record(10);
        limiter.record(20);
        limiter.record(30);
        let status = limiter.status();
        assert_eq!(status.current_writes_per_sec, 3.0);
        assert_eq!(status.current_bytes_per_sec, 60.0);
    }

    // ── acquire_blocking ────────────────────────────────────────────────

    #[test]
    fn acquire_blocking_delays_between_writes_when_limited() {
        let limiter = WriteRateLimiter::new(Some(WriteRateLimit {
            max_bytes_per_sec: 20,
        }));
        limiter.acquire_blocking(1).unwrap(); // consumes the initial burst credit
        let before = Instant::now();
        limiter.acquire_blocking(1).unwrap();
        assert!(Instant::now() >= before + Duration::from_millis(30));
    }

    #[test]
    fn acquire_blocking_is_unthrottled_and_still_records_without_a_limit() {
        let limiter = WriteRateLimiter::new(None);
        for _ in 0..1000 {
            limiter.acquire_blocking(1_000_000).unwrap();
        }
        let status = limiter.status();
        assert_eq!(status.limit, None);
        assert_eq!(status.rejected_total, 0);
        assert!(status.current_bytes_per_sec > 0.0, "stats still tracked");
    }

    #[test]
    fn acquire_blocking_rejects_when_the_safety_valve_is_tripped() {
        let limiter = WriteRateLimiter::new(Some(WriteRateLimit {
            max_bytes_per_sec: 1,
        }));
        limiter.acquire_blocking(1).unwrap(); // consumes the initial burst credit
        limiter.queued_bytes.store(2, Ordering::Relaxed); // already at the rate=1 safety-valve threshold
        let err = limiter.acquire_blocking(1).unwrap_err();
        assert!(err.to_string().contains("write rate limit exceeded"));
        assert_eq!(limiter.status().rejected_total, 1);
    }

    // ── Metered<T> ──────────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn metered_write_access_delays_when_a_limit_is_set() {
        let gate = Metered::new(
            (),
            Some(WriteRateLimit {
                max_bytes_per_sec: 2,
            }),
        );
        gate.write_access(1).await.unwrap();
        let before = Instant::now();
        gate.write_access(1).await.unwrap();
        assert!(Instant::now() >= before + Duration::from_millis(400));
    }

    #[tokio::test]
    async fn metered_write_access_is_unthrottled_but_counted_without_a_limit() {
        let gate = Metered::new((), None);
        for _ in 0..100 {
            gate.write_access(1_000).await.unwrap();
        }
        let status = gate.status();
        assert_eq!(status.limit, None);
        assert!(status.current_bytes_per_sec > 0.0, "stats still tracked");
    }

    #[test]
    fn metered_write_access_blocking_delays_when_a_limit_is_set() {
        let gate = Metered::new(
            (),
            Some(WriteRateLimit {
                max_bytes_per_sec: 20,
            }),
        );
        gate.write_access_blocking(1).unwrap();
        let before = Instant::now();
        gate.write_access_blocking(1).unwrap();
        assert!(Instant::now() >= before + Duration::from_millis(30));
    }

    #[tokio::test(start_paused = true)]
    async fn metered_set_limit_then_clear_round_trips_through_status() {
        let gate = Metered::new((), None);
        assert_eq!(gate.status().limit, None);

        gate.set_limit(Some(WriteRateLimit {
            max_bytes_per_sec: 5,
        }));
        assert_eq!(
            gate.status().limit,
            Some(WriteRateLimit {
                max_bytes_per_sec: 5
            })
        );
        gate.write_access(5).await.unwrap();
        let before = Instant::now();
        gate.write_access(5).await.unwrap();
        assert!(
            Instant::now() >= before + Duration::from_millis(400),
            "limit is enforced once set"
        );

        gate.set_limit(None);
        assert_eq!(gate.status().limit, None);
    }

    #[tokio::test(start_paused = true)]
    async fn metered_write_tallied_delays_the_next_write_by_the_tallied_bytes() {
        let gate = Metered::new(
            (),
            Some(WriteRateLimit {
                max_bytes_per_sec: 2,
            }),
        );
        gate.write_tallied(|_io, bytes| {
            bytes.fetch_add(2, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap();

        let before = Instant::now();
        gate.write_access(2).await.unwrap();
        assert!(
            Instant::now() >= before + Duration::from_millis(400),
            "the tallied bytes from the batch feed the same limiter as write_access"
        );
    }

    #[tokio::test]
    async fn metered_write_tallied_meters_the_bytes_the_batch_counted() {
        let gate = Metered::new((), None);
        gate.write_tallied(|_io, bytes| {
            bytes.fetch_add(7, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(gate.status().current_bytes_per_sec, 7.0);
    }

    #[tokio::test]
    async fn metered_write_tallied_propagates_the_closures_error() {
        let gate = Metered::new((), None);
        let err = gate
            .write_tallied(|_io: &(), _bytes| -> Result<()> { bail!("closure failed") })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("closure failed"));
    }

    #[test]
    fn metered_read_access_neither_delays_nor_records() {
        let gate = Metered::new(
            (),
            Some(WriteRateLimit {
                max_bytes_per_sec: 1,
            }),
        );
        for _ in 0..10 {
            gate.read_access();
        }
        assert_eq!(gate.status().current_bytes_per_sec, 0.0);
    }

    #[test]
    fn metered_into_inner_returns_io_only_for_the_sole_owner() {
        let gate = Metered::new(42u32, None);
        let clone = gate.clone();
        assert_eq!(clone.into_inner(), None, "a live clone blocks recovery");
        // `gate` is now the sole owner (its clone was consumed above).
        assert_eq!(gate.into_inner(), Some(42));
    }
}
