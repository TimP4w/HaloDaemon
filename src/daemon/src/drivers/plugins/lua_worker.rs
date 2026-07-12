// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared plumbing for the plugins module's Lua worker threads. A `LuaWorker`
//! owns a dedicated OS thread hosting a `!Send` [`mlua::Lua`] VM, talks to it
//! over an `mpsc` channel, and answers each request on a `oneshot`. The command
//! type `Cmd` and the per-thread context `Ctx` are chosen by each caller, so the
//! device worker (boxed-closure jobs) and the effect worker (typed enum) share
//! the thread/channel/reply wiring while keeping their own dispatch style.

use std::ops::ControlFlow;

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, oneshot};

/// Handle over a dedicated Lua-VM worker thread. `UnboundedSender` is
/// `Send + Sync`, so the handle is too. Dropping the last clone closes the
/// channel, which ends the worker loop.
pub(super) struct LuaWorker<Cmd> {
    tx: mpsc::UnboundedSender<Cmd>,
    /// "plugin" / "effect" — keeps each worker's error text distinct.
    label: &'static str,
}

impl<Cmd> Clone for LuaWorker<Cmd> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            label: self.label,
        }
    }
}

impl<Cmd: Send + 'static> LuaWorker<Cmd> {
    /// Spawn the worker thread. `build` constructs the `!Send` context *on* the
    /// worker thread (so the VM never crosses threads); if it fails the worker
    /// logs `"{label} worker stopped: …"` and exits. `handle` runs each command
    /// against that context and returns `ControlFlow::Break` to end the loop.
    pub(super) fn spawn<Ctx>(
        name: &'static str,
        label: &'static str,
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
        Self { tx, label }
    }

    /// Send a command carrying a `oneshot` reply sender and await the answer.
    /// `make` builds the command from the reply sender.
    pub(super) async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> Cmd,
    ) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .map_err(|_| anyhow!("{} worker is gone", self.label))?;
        rx.await
            .map_err(|_| anyhow!("{} worker dropped the reply", self.label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closure-dispatched worker (like the device worker): the context is a
    /// plain counter, and each job reads it back over the reply channel.
    type Job = Box<dyn FnOnce(&i32) -> ControlFlow<()> + Send>;

    fn spawn_counter(seed: i32) -> LuaWorker<Job> {
        LuaWorker::spawn(
            "halod-test-worker",
            "test",
            move || Ok(seed),
            |job: Job, ctx: &i32| job(ctx),
        )
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
        let worker = LuaWorker { tx, label: "test" };
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
