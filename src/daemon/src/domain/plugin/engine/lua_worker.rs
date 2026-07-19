// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared plumbing for the plugins module's Lua worker threads. A `LuaWorker`
//! owns a dedicated OS thread hosting a `!Send` [`mlua::Lua`] VM, talks to it
//! over an `mpsc` channel, and answers each request on a `oneshot`. The command
//! type `Cmd` and the per-thread context `Ctx` are chosen by each caller, so the
//! device worker (boxed-closure jobs) and the effect worker (typed enum) share
//! the thread/channel/reply wiring while keeping their own dispatch style.
//!
//! Known bound: a [`request`](LuaWorker::request) timeout *abandons* the
//! worker (transitions it to [`WorkerState::Wedged`] so later requests fail fast)
//! but cannot *terminate* its OS thread — mlua exposes no safe preemptive kill, so
//! a `pcall`-catching pure-compute runaway keeps one CPU-burning zombie thread
//! alive per malicious plugin. There is also no ceiling on concurrent worker
//! threads. Bounding this honestly is architectural, not in-VM: run plugins in a
//! separate process that can be `SIGKILL`'d (the existing broker/hwaccess
//! privilege split is the natural seam), and/or cap concurrent spawns. Tracked as
//! a deliberate follow-up.

use std::ops::ControlFlow;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, oneshot};

/// Why a worker is [`WorkerState::Wedged`] — presumed alive but unresponsive
/// (or never alive at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WedgeReason {
    /// A [`request`](LuaWorker::request) exceeded `call_timeout`; the single
    /// worker thread is presumed stuck running that job forever (mlua has no
    /// preemptive kill — see the module doc).
    Timeout,
    /// The worker thread could not be spawned at all (thread ceiling or OS
    /// spawn failure); [`LuaWorker::dead`] fabricates a worker already in this
    /// state so the plugin is disabled instead of the daemon crashing.
    SpawnFailed,
}

impl WedgeReason {
    fn describe(self) -> &'static str {
        match self {
            WedgeReason::Timeout => "killed after a timeout",
            WedgeReason::SpawnFailed => "failed to start",
        }
    }
}

/// Lifecycle of a [`LuaWorker`]'s OS thread: `Starting -> Healthy | Closed`,
/// `Healthy -> Wedged`, `Healthy -> Closing -> Closed`. `Wedged`/`Closed` are
/// terminal — never revived; a fresh worker is spawned instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WorkerState {
    Starting,
    Healthy,
    Wedged(WedgeReason),
    /// A command panicked on the worker thread.  Panics must not be reported
    /// as an unexplained dropped oneshot reply to the caller.
    Panicked(String),
    Closing,
    Closed,
}

/// Process-wide ceiling on concurrent Lua worker threads, so a flood of plugins
/// (or one plugin repeatedly respawned) can't exhaust OS threads.
const MAX_WORKER_THREADS: usize = 64;
static WORKER_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Bounded per-worker command queue: caps in-flight jobs so slow transport/Lua
/// callbacks can't accumulate unbounded memory.
const WORKER_QUEUE_CAP: usize = 64;

/// A panicking job drops its reply sender before the worker
/// thread can publish [`WorkerState::Panicked`], so a caller woken by that drop
/// can momentarily observe `Healthy` state. Poll briefly for the panic
/// to surface before falling back to a bare dropped-reply error, so a panic is
/// never misreported as a silent drop.
const DROPPED_REPLY_GRACE_POLLS: u32 = 40;
const DROPPED_REPLY_GRACE_INTERVAL: Duration = Duration::from_millis(5);

/// Decrements [`WORKER_COUNT`] when the worker thread exits.
struct WorkerSlot;
impl Drop for WorkerSlot {
    fn drop(&mut self) {
        WORKER_COUNT.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Handle over a dedicated Lua-VM worker thread. `Sender` is `Send + Sync`, so
/// the handle is too. Dropping the last clone closes the channel, which ends the
/// worker loop.
pub(super) struct LuaWorker<Cmd> {
    tx: mpsc::Sender<Cmd>,
    /// "plugin" / "effect" — keeps each worker's error text distinct.
    label: &'static str,
    /// How long a single [`request`](Self::request) waits before giving up and
    /// declaring the worker dead. The Lua instruction budget kills an *uncaught*
    /// runaway, but a `pcall`-catching loop stays on the (single) worker thread
    /// forever; without this bound every later request would queue behind it and
    /// hang too. On timeout the worker is poisoned so those later requests fail
    /// fast instead of piling up on the wedged thread.
    call_timeout: Duration,
    /// Current lifecycle state, shared with the worker thread so it can
    /// report `Starting -> Healthy -> Closing -> Closed` as it runs.
    state: Arc<Mutex<WorkerState>>,
    /// Monotonic per-request counter (shared across clones of the same
    /// worker) so a timeout or dropped-reply error can be correlated to a
    /// specific in-flight call in logs.
    next_req_id: Arc<AtomicU64>,
}

impl<Cmd> Clone for LuaWorker<Cmd> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            label: self.label,
            call_timeout: self.call_timeout,
            state: self.state.clone(),
            next_req_id: self.next_req_id.clone(),
        }
    }
}

impl<Cmd: Send + 'static> LuaWorker<Cmd> {
    /// Spawn the worker thread. `build` constructs the `!Send` context *on* the
    /// worker thread (so the VM never crosses threads); if it fails the worker
    /// logs `"{label} worker stopped: …"` and exits. `handle` runs each command
    /// against that context and returns `ControlFlow::Break` to end the loop.
    /// `call_timeout` bounds a single [`request`](Self::request).
    pub(super) fn spawn<Ctx>(
        name: &'static str,
        label: &'static str,
        call_timeout: Duration,
        build: impl FnOnce() -> Result<Ctx> + Send + 'static,
        handle: impl Fn(Cmd, &Ctx) -> ControlFlow<()> + Send + 'static,
    ) -> Result<Self> {
        // Reserve a thread slot; refuse rather than exhaust OS threads.
        let n = WORKER_COUNT.fetch_add(1, Ordering::Relaxed);
        if n >= MAX_WORKER_THREADS {
            WORKER_COUNT.fetch_sub(1, Ordering::Relaxed);
            return Err(anyhow!(
                "{label} worker limit reached ({MAX_WORKER_THREADS} threads)"
            ));
        }
        let (tx, rx) = mpsc::channel(WORKER_QUEUE_CAP);
        let state = Arc::new(Mutex::new(WorkerState::Starting));
        let thread_state = state.clone();
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let _slot = WorkerSlot;
                match build() {
                    Ok(ctx) => {
                        *thread_state.lock().unwrap() = WorkerState::Healthy;
                        let mut rx = rx;
                        while let Some(cmd) = rx.blocking_recv() {
                            match panic::catch_unwind(AssertUnwindSafe(|| handle(cmd, &ctx))) {
                                Ok(ControlFlow::Continue(())) => {}
                                Ok(ControlFlow::Break(())) => break,
                                Err(payload) => {
                                    let message = panic_message(payload);
                                    log::error!("{label} worker panicked while handling a request: {message}");
                                    *thread_state.lock().unwrap() = WorkerState::Panicked(message);
                                    return;
                                }
                            }
                        }
                        *thread_state.lock().unwrap() = WorkerState::Closing;
                    }
                    Err(e) => log::error!("{label} worker stopped: {e:#}"),
                }
                *thread_state.lock().unwrap() = WorkerState::Closed;
            });
        if let Err(e) = spawned {
            WORKER_COUNT.fetch_sub(1, Ordering::Relaxed);
            return Err(anyhow!("{label} worker thread spawn failed: {e}"));
        }
        Ok(Self {
            tx,
            label,
            call_timeout,
            state,
            next_req_id: Arc::new(AtomicU64::new(0)),
        })
    }

    /// A worker that owns no thread and fails every request — used when a real
    /// worker can't be spawned (thread ceiling/exhaustion) so the plugin is
    /// disabled rather than crashing the daemon.
    pub(super) fn dead(label: &'static str) -> Self {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        Self {
            tx,
            label,
            call_timeout: Duration::from_secs(1),
            state: Arc::new(Mutex::new(WorkerState::Wedged(WedgeReason::SpawnFailed))),
            next_req_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Snapshot of the current lifecycle state.
    #[cfg(test)]
    pub(super) fn state(&self) -> WorkerState {
        self.state.lock().unwrap().clone()
    }

    pub(super) fn is_usable(&self) -> bool {
        !self.tx.is_closed()
            && matches!(
                *self.state.lock().unwrap(),
                WorkerState::Starting | WorkerState::Healthy
            )
    }

    /// Send a command carrying a `oneshot` reply sender and await the answer,
    /// giving up after `call_timeout`. A timeout marks the worker dead so no
    /// later request queues behind the (presumed wedged) job.
    pub(super) async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> Cmd,
    ) -> Result<T> {
        let req_id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        match &*self.state.lock().unwrap() {
            WorkerState::Wedged(reason) => {
                return Err(anyhow!(
                    "{} worker is wedged ({})",
                    self.label,
                    reason.describe()
                ));
            }
            WorkerState::Panicked(message) => {
                return Err(anyhow!("{} worker panicked: {message}", self.label));
            }
            WorkerState::Closing | WorkerState::Closed => {
                return Err(anyhow!("{} worker is gone", self.label));
            }
            WorkerState::Starting | WorkerState::Healthy => {}
        }
        let (reply, rx) = oneshot::channel();
        self.tx.try_send(make(reply)).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => anyhow!("{} worker is busy", self.label),
            mpsc::error::TrySendError::Closed(_) => {
                *self.state.lock().unwrap() = WorkerState::Closed;
                anyhow!("{} worker is gone", self.label)
            }
        })?;
        match tokio::time::timeout(self.call_timeout, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(self.dropped_reply_error(req_id).await),
            Err(_) => {
                *self.state.lock().unwrap() = WorkerState::Wedged(WedgeReason::Timeout);
                Err(anyhow!(
                    "{} worker exceeded its {:?} call deadline (req {req_id})",
                    self.label,
                    self.call_timeout
                ))
            }
        }
    }

    /// Enqueue a shutdown-critical command without dropping it when the bounded
    /// queue is temporarily full. The same deadline still bounds both waiting
    /// for queue capacity and waiting for the worker's reply.
    pub(super) async fn request_terminal<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> Cmd,
    ) -> Result<T> {
        let req_id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        match &*self.state.lock().unwrap() {
            WorkerState::Wedged(reason) => {
                return Err(anyhow!(
                    "{} worker is wedged ({})",
                    self.label,
                    reason.describe()
                ));
            }
            WorkerState::Panicked(message) => {
                return Err(anyhow!("{} worker panicked: {message}", self.label));
            }
            WorkerState::Closing | WorkerState::Closed => {
                return Err(anyhow!("{} worker is gone", self.label));
            }
            WorkerState::Starting | WorkerState::Healthy => {}
        }
        let (reply, rx) = oneshot::channel();
        let command = make(reply);
        // One deadline bounds both waiting for queue capacity and the reply, so a
        // slow send can't hand the recv a fresh full budget.
        let deadline = tokio::time::Instant::now() + self.call_timeout;
        let wedged = |worker: &Self| {
            *worker.state.lock().unwrap() = WorkerState::Wedged(WedgeReason::Timeout);
            anyhow!(
                "{} worker exceeded its {:?} terminal deadline (req {req_id})",
                worker.label,
                worker.call_timeout
            )
        };
        match tokio::time::timeout_at(deadline, self.tx.send(command)).await {
            Err(_) => return Err(wedged(self)),
            Ok(Err(_)) => {
                *self.state.lock().unwrap() = WorkerState::Closed;
                return Err(anyhow!("{} worker is gone", self.label));
            }
            Ok(Ok(())) => {}
        }
        match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(self.dropped_reply_error(req_id).await),
            Err(_) => Err(wedged(self)),
        }
    }
}

impl<Cmd> LuaWorker<Cmd> {
    async fn dropped_reply_error(&self, req_id: u64) -> anyhow::Error {
        for _ in 0..DROPPED_REPLY_GRACE_POLLS {
            if let WorkerState::Panicked(message) = &*self.state.lock().unwrap() {
                return anyhow!(
                    "{} worker panicked while handling req {req_id}: {message}",
                    self.label
                );
            }
            tokio::time::sleep(DROPPED_REPLY_GRACE_INTERVAL).await;
        }
        anyhow!("{} worker dropped the reply (req {req_id})", self.label)
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closure-dispatched worker (like the device worker): the context is a
    /// plain counter, and each job reads it back over the reply channel.
    type Job = Box<dyn FnOnce(&i32) -> ControlFlow<()> + Send>;

    fn spawn_counter(seed: i32) -> LuaWorker<Job> {
        spawn_counter_with_timeout(seed, Duration::from_secs(5))
    }

    fn spawn_counter_with_timeout(seed: i32, call_timeout: Duration) -> LuaWorker<Job> {
        LuaWorker::spawn(
            "halod-test-worker",
            "test",
            call_timeout,
            move || Ok(seed),
            |job: Job, ctx: &i32| job(ctx),
        )
        .expect("spawn test worker")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wedged_job_times_out_and_poisons_the_worker() {
        let worker = spawn_counter_with_timeout(0, Duration::from_millis(50));
        // A job that blocks the single worker thread forever (mimics a
        // `pcall`-catching runaway the instruction budget can't unwind).
        let err = worker
            .request(|reply: oneshot::Sender<()>| {
                Box::new(move |_: &i32| {
                    std::thread::sleep(Duration::from_secs(60));
                    let _ = reply.send(());
                    ControlFlow::Continue(())
                }) as Job
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("call deadline"), "{err}");
        // Once poisoned, every later request fails fast instead of queueing
        // behind the still-wedged thread.
        let err2 = worker
            .request(|reply| {
                Box::new(move |_: &i32| {
                    let _ = reply.send(());
                    ControlFlow::Continue(())
                })
            })
            .await
            .unwrap_err();
        assert!(err2.to_string().contains("wedged"), "{err2}");
    }

    #[tokio::test]
    async fn a_dead_worker_fails_every_request() {
        let worker: LuaWorker<Job> = LuaWorker::dead("test");
        let err = worker
            .request(|reply| {
                Box::new(move |_: &i32| {
                    let _ = reply.send(());
                    ControlFlow::Continue(())
                })
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("wedged"), "{err}");
    }

    #[test]
    fn dead_worker_starts_wedged_with_spawn_failed_reason() {
        let worker: LuaWorker<Job> = LuaWorker::dead("test");
        assert_eq!(
            worker.state(),
            WorkerState::Wedged(WedgeReason::SpawnFailed)
        );
    }

    #[tokio::test]
    async fn spawn_reaches_healthy_after_a_successful_request() {
        let worker = spawn_counter(7);
        let got: i32 = worker
            .request(|reply| {
                Box::new(move |ctx: &i32| {
                    let _ = reply.send(*ctx);
                    ControlFlow::Continue(())
                })
            })
            .await
            .unwrap();
        assert_eq!(got, 7);
        assert_eq!(worker.state(), WorkerState::Healthy);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timeout_transitions_state_to_wedged_timeout() {
        let worker = spawn_counter_with_timeout(0, Duration::from_millis(50));
        let _ = worker
            .request(|reply: oneshot::Sender<()>| {
                Box::new(move |_: &i32| {
                    std::thread::sleep(Duration::from_secs(60));
                    let _ = reply.send(());
                    ControlFlow::Continue(())
                }) as Job
            })
            .await;
        assert_eq!(worker.state(), WorkerState::Wedged(WedgeReason::Timeout));
    }

    #[tokio::test]
    async fn request_ids_are_monotonic_per_worker() {
        let worker = spawn_counter(0);
        // A job that never sends on `reply` fails deterministically with the
        // "dropped the reply" error, letting us read `req_id` out of it
        // without racing a real timeout.
        let err0 = worker
            .request(|_reply: oneshot::Sender<()>| {
                Box::new(|_: &i32| ControlFlow::Continue(())) as Job
            })
            .await
            .unwrap_err();
        let err1 = worker
            .request(|_reply: oneshot::Sender<()>| {
                Box::new(|_: &i32| ControlFlow::Continue(())) as Job
            })
            .await
            .unwrap_err();
        assert!(err0.to_string().contains("req 0"), "{err0}");
        assert!(err1.to_string().contains("req 1"), "{err1}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handler_break_ends_the_loop_and_state_becomes_closed() {
        let worker = spawn_counter(0);
        worker
            .request(|reply: oneshot::Sender<()>| {
                Box::new(move |_: &i32| {
                    let _ = reply.send(());
                    ControlFlow::Break(())
                }) as Job
            })
            .await
            .unwrap();
        // The thread transitions Closing -> Closed after returning from the
        // loop, just after replying; poll briefly rather than racing it.
        for _ in 0..50 {
            if worker.state() == WorkerState::Closed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(worker.state(), WorkerState::Closed);
    }

    #[tokio::test]
    async fn request_round_trips_through_the_worker() {
        let worker = spawn_counter(41);
        let got: i32 = worker
            .request(|reply| {
                Box::new(move |ctx: &i32| {
                    let _ = reply.send(*ctx + 1);
                    ControlFlow::Continue(())
                })
            })
            .await
            .unwrap();
        assert_eq!(got, 42);
    }

    #[tokio::test]
    async fn terminal_request_waits_for_queue_capacity() {
        let (tx, mut rx) = mpsc::channel::<Job>(1);
        tx.try_send(Box::new(|_: &i32| ControlFlow::Continue(())))
            .unwrap();
        let worker = LuaWorker {
            tx,
            label: "test",
            call_timeout: Duration::from_secs(1),
            state: Arc::new(Mutex::new(WorkerState::Healthy)),
            next_req_id: Arc::new(AtomicU64::new(0)),
        };
        let terminal = tokio::spawn(async move {
            worker
                .request_terminal(|reply| {
                    Box::new(move |_: &i32| {
                        let _ = reply.send(42);
                        ControlFlow::Break(())
                    }) as Job
                })
                .await
        });
        tokio::task::yield_now().await;
        assert!(
            !terminal.is_finished(),
            "terminal command must wait, not fail busy"
        );

        let _ = rx.recv().await.unwrap()(&0);
        let _ = rx.recv().await.unwrap()(&0);
        assert_eq!(terminal.await.unwrap().unwrap(), 42);
    }

    #[tokio::test]
    async fn a_job_that_drops_its_reply_surfaces_an_error() {
        let worker = spawn_counter(0);
        // The job runs but never sends on `reply`, so the sender is dropped.
        let err = worker
            .request(|_reply: oneshot::Sender<()>| {
                Box::new(|_: &i32| ControlFlow::Continue(())) as Job
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("dropped the reply"));
    }

    #[tokio::test]
    async fn a_panicking_job_reports_the_panic_instead_of_a_dropped_reply() {
        let worker = spawn_counter(0);
        let err = worker
            .request(|reply: oneshot::Sender<()>| {
                // The job owns the reply, so it is only dropped as this frame
                // unwinds — i.e. the reply-drop is *caused by* the panic, which is
                // what lets the worker report the panic rather than a bare drop.
                Box::new(move |_: &i32| -> ControlFlow<()> {
                    let _reply = reply;
                    panic!("intentional worker panic")
                }) as Job
            })
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("panicked while handling req 0"),
            "{err}"
        );
        assert!(
            err.to_string().contains("intentional worker panic"),
            "{err}"
        );
        assert!(matches!(worker.state(), WorkerState::Panicked(_)));
    }

    #[tokio::test]
    async fn request_fails_when_the_channel_is_closed() {
        // Hand-build a worker whose receiver is already dropped, so the send
        // fails deterministically (no thread timing involved).
        let (tx, rx) = mpsc::channel::<Job>(1);
        drop(rx);
        let worker = LuaWorker {
            tx,
            label: "test",
            call_timeout: Duration::from_secs(5),
            state: Arc::new(Mutex::new(WorkerState::Healthy)),
            next_req_id: Arc::new(AtomicU64::new(0)),
        };
        let err = worker
            .request(|reply| {
                Box::new(move |_: &i32| {
                    let _ = reply.send(());
                    ControlFlow::Continue(())
                })
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("worker is gone"));
    }
}
