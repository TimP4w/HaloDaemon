// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared plumbing for the plugins module's Lua worker threads. A `LuaWorker`
//! owns a dedicated OS thread hosting a `!Send` [`mlua::Lua`] VM, talks to it
//! over an `mpsc` channel, and answers each request on a `oneshot`. The command
//! type `Cmd` and the per-thread context `Ctx` are chosen by each caller, so the
//! device worker (boxed-closure jobs) and the effect worker (typed enum) share
//! the thread/channel/reply wiring while keeping their own dispatch style.
//!
//! Known bound (ARCH-R1): a [`request`](LuaWorker::request) timeout *abandons* the
//! worker (poisons the handle so later requests fail fast) but cannot *terminate*
//! its OS thread — mlua exposes no safe preemptive kill, so a `pcall`-catching
//! pure-compute runaway keeps one CPU-burning zombie thread alive per malicious
//! plugin. There is also no ceiling on concurrent worker threads. Bounding this
//! honestly is architectural, not in-VM: run plugins in a separate process that
//! can be `SIGKILL`'d (the existing broker/hwaccess privilege split is the natural
//! seam), and/or cap concurrent spawns. Tracked as a deliberate follow-up.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, oneshot};

/// Handle over a dedicated Lua-VM worker thread. `UnboundedSender` is
/// `Send + Sync`, so the handle is too. Dropping the last clone closes the
/// channel, which ends the worker loop.
pub(super) struct LuaWorker<Cmd> {
    tx: mpsc::UnboundedSender<Cmd>,
    /// "plugin" / "effect" — keeps each worker's error text distinct.
    label: &'static str,
    /// How long a single [`request`](Self::request) waits before giving up and
    /// declaring the worker dead. The Lua instruction budget kills an *uncaught*
    /// runaway, but a `pcall`-catching loop stays on the (single) worker thread
    /// forever; without this bound every later request would queue behind it and
    /// hang too. On timeout the worker is poisoned so those later requests fail
    /// fast instead of piling up on the wedged thread.
    call_timeout: Duration,
    /// Set once a request times out: the worker thread is presumed wedged, so
    /// every subsequent request short-circuits rather than enqueueing.
    dead: Arc<AtomicBool>,
}

impl<Cmd> Clone for LuaWorker<Cmd> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            label: self.label,
            call_timeout: self.call_timeout,
            dead: self.dead.clone(),
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
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || match build() {
                Ok(ctx) => {
                    let mut rx = rx;
                    while let Some(cmd) = rx.blocking_recv() {
                        if handle(cmd, &ctx).is_break() {
                            break;
                        }
                    }
                }
                Err(e) => log::error!("{label} worker stopped: {e:#}"),
            })
            .expect("spawn lua worker thread");
        Self {
            tx,
            label,
            call_timeout,
            dead: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Send a command carrying a `oneshot` reply sender and await the answer,
    /// giving up after `call_timeout`. A timeout marks the worker dead so no
    /// later request queues behind the (presumed wedged) job.
    pub(super) async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> Cmd,
    ) -> Result<T> {
        if self.dead.load(Ordering::Relaxed) {
            return Err(anyhow!(
                "{} worker is wedged (killed after a timeout)",
                self.label
            ));
        }
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .map_err(|_| anyhow!("{} worker is gone", self.label))?;
        match tokio::time::timeout(self.call_timeout, rx).await {
            Ok(res) => res.map_err(|_| anyhow!("{} worker dropped the reply", self.label)),
            Err(_) => {
                self.dead.store(true, Ordering::Relaxed);
                Err(anyhow!(
                    "{} worker exceeded its {:?} call deadline",
                    self.label,
                    self.call_timeout
                ))
            }
        }
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
    async fn request_fails_when_the_channel_is_closed() {
        // Hand-build a worker whose receiver is already dropped, so the send
        // fails deterministically (no thread timing involved).
        let (tx, rx) = mpsc::unbounded_channel::<Job>();
        drop(rx);
        let worker = LuaWorker {
            tx,
            label: "test",
            call_timeout: Duration::from_secs(5),
            dead: Arc::new(AtomicBool::new(false)),
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
