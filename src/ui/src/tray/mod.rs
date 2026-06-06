// SPDX-License-Identifier: MIT
//! Platform-agnostic tray entry point.

#[cfg(unix)]
pub mod linux;

#[cfg(windows)]
pub mod windows;

use libadwaita as adw;

use crate::ipc::IpcSender;
use crate::store::Store;

/// Initialise the system tray. Platform-dispatches to the correct backend.
///
/// On Linux the function is non-blocking: tray registration happens on a
/// background thread and the GTK main loop polls for the result. If no SNI
/// watcher is detected within ~2 s the main window is shown automatically.
pub fn init(app: &adw::Application, window: &adw::ApplicationWindow, store: Store, ipc: IpcSender) {
    #[cfg(unix)]
    init_linux(app, window, store, ipc);

    #[cfg(windows)]
    windows::init(app, window, store, ipc);

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (app, window, store, ipc);
    }
}

// ─── Linux implementation ────────────────────────────────────────────────────

#[cfg(unix)]
fn init_linux(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    store: Store,
    ipc: IpcSender,
) {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use gtk4::glib;
    use libadwaita::prelude::*;

    use crate::tray::linux::HalodTray;
    use crate::tray::linux::TrayAction;
    use crate::service;

    // Channel: tray → GTK main thread actions.
    let (action_tx, action_rx) = async_channel::unbounded::<TrayAction>();

    // Channel: background thread → GTK main thread, carries the ksni handle.
    let (handle_tx, handle_rx) =
        mpsc::sync_channel::<Option<ksni::Handle<HalodTray>>>(1);

    // Spawn a background thread with its own tokio runtime.
    // ksni::TrayService::spawn() creates yet another OS thread internally;
    // the tokio runtime is only needed for the zbus watcher check.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tray: tokio runtime");

        rt.block_on(async move {
            // Check whether an SNI watcher is present BEFORE spawning ksni so
            // that no ksni thread, D-Bus connection, or action_tx clone leaks
            // when no watcher is available.
            if !sni_watcher_present().await {
                let _ = handle_tx.send(None);
                return;
            }

            // Watcher confirmed — now it is safe to spawn the ksni service.
            let tray = HalodTray::new(action_tx);
            let service = ksni::TrayService::new(tray);
            let handle = service.handle();
            service.spawn(); // spawns its own OS thread, consumes service
            let _ = handle_tx.send(Some(handle));
        });
    });

    // ── GTK: poll handle_rx until we receive the ksni handle ─────────────────
    let win_for_timer = window.clone();
    let store_for_timer = store.clone();
    let start = Instant::now();

    glib::timeout_add_local(Duration::from_millis(50), move || {
        use std::sync::mpsc::TryRecvError;

        // Timed-out: fall back to showing the window.
        if start.elapsed() > Duration::from_millis(2000) {
            win_for_timer.present();
            return glib::ControlFlow::Break;
        }

        match handle_rx.try_recv() {
            Ok(Some(handle)) => {
                // SNI watcher found — subscribe to store state changes so the
                // tray menu reflects live battery data.
                let handle_for_subscribe = handle.clone();
                store_for_timer.subscribe(
                    |st| crate::store::sel_hash(&linux::battery_lines(st)),
                    move |st| {
                        handle_for_subscribe
                            .update(|tray: &mut HalodTray| tray.apply_state(st));
                    },
                );
                glib::ControlFlow::Break
            }
            Ok(None) => {
                // No watcher: show the window immediately.
                win_for_timer.present();
                glib::ControlFlow::Break
            }
            Err(TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(TryRecvError::Disconnected) => {
                // Background thread panicked — fall back to showing the window.
                win_for_timer.present();
                glib::ControlFlow::Break
            }
        }
    });

    // ── GTK: handle actions sent by the tray menu ─────────────────────────────
    let app_for_actions = app.clone();
    let win_for_actions = window.clone();
    let ipc_for_actions = ipc;

    glib::MainContext::default().spawn_local(async move {
        while let Ok(action) = action_rx.recv().await {
            match action {
                TrayAction::Open => win_for_actions.present(),
                TrayAction::Quit => {
                    service::stop_service(&ipc_for_actions);
                    app_for_actions.quit();
                }
            }
        }
    });
}

// ─── zbus watcher check ──────────────────────────────────────────────────────

#[cfg(unix)]
async fn sni_watcher_present() -> bool {
    let Ok(conn) = zbus::Connection::session().await else {
        return false;
    };
    let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else {
        return false;
    };
    dbus.name_has_owner("org.kde.StatusNotifierWatcher".try_into().unwrap())
        .await
        .unwrap_or(false)
}
