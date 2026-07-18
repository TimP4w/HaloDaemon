// SPDX-License-Identifier: GPL-3.0-or-later
//! Single-instance guard: only one GUI process owns the window + tray.
//!
//! At startup [`acquire`] binds a per-user local socket (Unix domain socket /
//! Windows named pipe) on a dedicated background thread. The first launch
//! becomes the [`Primary`] and keeps the listener; a later launch finds the
//! socket already served, pings it so the running instance surfaces its window,
//! and exits without opening a second window or a duplicate tray icon.
//!
//! The tray already owns the whole process on both backends (close-to-tray
//! keeps `halod-gui` resident with no window), so without this a re-launch — the
//! usual way a user "opens" a resident app — stacked a second tray icon and a
//! second window instead of raising the first.

use std::sync::mpsc;

use halod_shared::socket::gui_socket_path;
use tokio::sync::oneshot;

/// One byte a later launch writes to ask the primary to surface its window.
const SHOW_PING: u8 = b'S';

/// The "surface the window" action, installed once the egui context exists and
/// shared across every ping (`Arc` so the Windows path can hold it per pipe).
type ShowFn = std::sync::Arc<dyn Fn() + Send + Sync>;

/// Message the guard thread reports back to `main`: `Some` sender ⇒ we are the
/// primary and this is the channel to hand it the show action; `None` ⇒ another
/// instance is live and has been pinged, so the caller must exit.
type Report = Option<oneshot::Sender<ShowFn>>;

/// Outcome of trying to become the sole GUI instance.
pub enum Instance {
    /// This process owns the window + tray. Call [`Primary::serve`] once the
    /// egui context exists so later launches can raise the window.
    Primary(Primary),
    /// Another instance is already running and has been asked to show itself;
    /// the caller must exit without creating a window or tray.
    Secondary,
}

/// Handle held by the sole instance; hand it the show action via [`serve`].
pub struct Primary {
    serve: Option<oneshot::Sender<ShowFn>>,
}

impl Primary {
    /// Start answering "show" pings by running `on_show` on each one. A no-op
    /// when the guard couldn't be established (the app still runs, just
    /// unguarded).
    pub fn serve(mut self, on_show: impl Fn() + Send + Sync + 'static) {
        if let Some(tx) = self.serve.take() {
            let _ = tx.send(std::sync::Arc::new(on_show));
        }
    }
}

/// Try to become the sole GUI instance. Blocks only until the guard thread has
/// bound (or failed to bind) the socket — the accept loop runs on that thread.
pub fn acquire() -> Instance {
    let (res_tx, res_rx) = mpsc::channel::<Report>();
    let spawned = std::thread::Builder::new()
        .name("halod-gui-single-instance".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log::warn!("single-instance: runtime build failed, running unguarded: {e}");
                    report_unguarded(&res_tx);
                    return;
                }
            };
            rt.block_on(run_guard(gui_socket_path(), res_tx));
        });
    if spawned.is_err() {
        // Can't even spawn the guard thread; run unguarded rather than refuse.
        return Instance::Primary(Primary { serve: None });
    }
    match res_rx.recv() {
        Ok(Some(serve)) => Instance::Primary(Primary { serve: Some(serve) }),
        Ok(None) => Instance::Secondary,
        // Guard thread died before reporting; run unguarded.
        Err(_) => Instance::Primary(Primary { serve: None }),
    }
}

/// Report "we own the socket": hand `main` the show-action channel, then await
/// the action and return it. `None` if `main` dropped the handle first.
async fn await_show(res_tx: &mpsc::Sender<Report>) -> Option<ShowFn> {
    let (serve_tx, serve_rx) = oneshot::channel();
    res_tx.send(Some(serve_tx)).ok()?;
    serve_rx.await.ok()
}

/// Report "another instance is live": `main` must exit.
fn report_secondary(res_tx: &mpsc::Sender<Report>) {
    let _ = res_tx.send(None);
}

/// Report "primary, but the guard couldn't be set up": `main` runs unguarded.
/// Hands over a live sender whose receiver is already gone, so `serve` no-ops.
fn report_unguarded(res_tx: &mpsc::Sender<Report>) {
    let (serve_tx, _drop_rx) = oneshot::channel();
    let _ = res_tx.send(Some(serve_tx));
}

#[cfg(unix)]
async fn run_guard(path: String, res_tx: mpsc::Sender<Report>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};

    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if let Ok(mut stream) = UnixStream::connect(&path).await {
                let _ = stream.write_all(&[SHOW_PING]).await;
                let _ = stream.shutdown().await;
                report_secondary(&res_tx);
                return;
            }
            // A live instance would have accepted the connection; it didn't, so
            // this is a stale socket left by a crashed instance. Reclaim it.
            let _ = std::fs::remove_file(&path);
            match UnixListener::bind(&path) {
                Ok(l) => l,
                Err(e) => {
                    log::warn!("single-instance: rebind after stale socket failed: {e}");
                    report_unguarded(&res_tx);
                    return;
                }
            }
        }
        Err(e) => {
            log::warn!("single-instance: bind failed, running unguarded: {e}");
            report_unguarded(&res_tx);
            return;
        }
    };

    let Some(show) = await_show(&res_tx).await else {
        return;
    };
    loop {
        match listener.accept().await {
            Ok((mut stream, _)) => {
                let mut buf = [0u8; 1];
                if stream.read_exact(&mut buf).await.is_ok() && buf[0] == SHOW_PING {
                    show();
                }
            }
            Err(e) => {
                log::warn!("single-instance: accept failed: {e}");
                break;
            }
        }
    }
}

#[cfg(windows)]
async fn run_guard(path: String, res_tx: mpsc::Sender<Report>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

    // `first_pipe_instance(true)` fails with ERROR_ACCESS_DENIED (mapped to
    // `PermissionDenied`) when another instance already created the pipe.
    let mut server = match ServerOptions::new().first_pipe_instance(true).create(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            match ClientOptions::new().open(&path) {
                Ok(mut client) => {
                    let _ = client.write_all(&[SHOW_PING]).await;
                    let _ = client.shutdown().await;
                    report_secondary(&res_tx);
                }
                // Pipe vanished between the two calls; run unguarded.
                Err(_) => report_unguarded(&res_tx),
            }
            return;
        }
        Err(e) => {
            log::warn!("single-instance: pipe create failed, running unguarded: {e}");
            report_unguarded(&res_tx);
            return;
        }
    };

    let Some(show) = await_show(&res_tx).await else {
        return;
    };
    loop {
        if server.connect().await.is_err() {
            break;
        }
        // Hand off the connected instance and open the next one so a subsequent
        // launch never finds the pipe unavailable.
        let mut connected = match ServerOptions::new().create(&path) {
            Ok(next) => std::mem::replace(&mut server, next),
            Err(e) => {
                log::warn!("single-instance: next pipe instance failed: {e}");
                break;
            }
        };
        let mut buf = [0u8; 1];
        if connected.read_exact(&mut buf).await.is_ok() && buf[0] == SHOW_PING {
            show();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn temp_socket() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(halod_shared::app::GUI_SOCKET_FILENAME);
        let path = path.to_str().unwrap().to_string();
        (dir, path)
    }

    /// Spawn the real `run_guard` on the current runtime and return the report
    /// once it has finished binding.
    fn spawn_guard(path: String) -> mpsc::Receiver<Report> {
        let (tx, rx) = mpsc::channel::<Report>();
        tokio::spawn(run_guard(path, tx));
        rx
    }

    async fn recv_report(rx: &mpsc::Receiver<Report>) -> Report {
        for _ in 0..200 {
            if let Ok(r) = rx.try_recv() {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("guard never reported")
    }

    #[tokio::test]
    async fn a_second_launch_pings_the_first_and_reports_secondary() {
        let (_dir, path) = temp_socket();

        // Primary binds and serves; each ping bumps a counter.
        let first_rx = spawn_guard(path.clone());
        let Some(serve_tx) = recv_report(&first_rx).await else {
            panic!("first guard should be primary");
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_show = hits.clone();
        serve_tx
            .send(Arc::new(move || {
                hits_show.fetch_add(1, Ordering::SeqCst);
            }))
            .ok()
            .expect("serve action installed");

        // Second launch: bind fails, so it pings the primary and reports Secondary.
        let second_rx = spawn_guard(path.clone());
        assert!(
            recv_report(&second_rx).await.is_none(),
            "second launch reports secondary"
        );

        for _ in 0..100 {
            if hits.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "primary got exactly one ping"
        );
    }

    #[tokio::test]
    async fn a_stale_socket_is_reclaimed_as_primary() {
        let (_dir, path) = temp_socket();
        // A bound-then-dropped listener leaves the socket file with nobody
        // listening — the crashed-instance case.
        {
            let _l = tokio::net::UnixListener::bind(&path).unwrap();
        }
        assert!(std::path::Path::new(&path).exists(), "stale file remains");

        let rx = spawn_guard(path);
        assert!(
            recv_report(&rx).await.is_some(),
            "stale socket is reclaimed, not mistaken for a live instance"
        );
    }

    #[test]
    fn serve_after_an_unguarded_report_is_a_no_op() {
        // `report_unguarded` drops the receiver, so `Primary::serve` must not panic.
        let (tx, rx) = mpsc::channel::<Report>();
        report_unguarded(&tx);
        let Ok(Some(serve)) = rx.recv() else {
            panic!("unguarded still reports primary")
        };
        Primary { serve: Some(serve) }.serve(|| unreachable!("no listener to ping it"));
    }
}
