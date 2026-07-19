// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;
use tokio::sync::mpsc;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use super::FocusEvent;

struct AppData {
    tx: mpsc::Sender<FocusEvent>,
    active_app_id: Option<String>,
    toplevels: HashMap<wayland_client::backend::ObjectId, ToplevelState>,
    /// Set when the receiver is gone — signals the dispatch loop to exit.
    disconnected: bool,
}

#[derive(Default)]
struct ToplevelState {
    /// App IDs are persistent protocol state. Compositors generally send this
    /// once when the handle is announced, not again on every activation.
    app_id: Option<String>,
    activated: bool,
}

impl AppData {
    /// Emits a focus change if `app_id` differs from the last reported app.
    /// Records `disconnected` when the receiver has been dropped so the
    /// dispatch loop can exit instead of leaking the thread.
    fn report_focus(&mut self, app_id: &str) {
        let normalized = super::normalize_name(app_id);
        if self.active_app_id.as_deref() == Some(&normalized) {
            return;
        }
        self.active_app_id = Some(normalized.clone());
        if self
            .tx
            .blocking_send(FocusEvent::AppFocused {
                process_name: normalized,
            })
            .is_err()
        {
            self.disconnected = true;
        }
    }

    fn report_no_app(&mut self) {
        if self.active_app_id.take().is_some() && self.tx.blocking_send(FocusEvent::NoApp).is_err()
        {
            self.disconnected = true;
        }
    }

    fn report_current_focus(&mut self) {
        let focused = self
            .toplevels
            .values()
            .find(|toplevel| toplevel.activated)
            .and_then(|toplevel| toplevel.app_id.clone());
        if let Some(app_id) = focused {
            self.report_focus(&app_id);
        } else {
            self.report_no_app();
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for AppData {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.entry(toplevel.id()).or_default();
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for AppData {
    fn event(
        state: &mut Self,
        proxy: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let id = proxy.id();
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                state.toplevels.entry(id).or_default().app_id = Some(app_id);
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: raw_state } => {
                use zwlr_foreign_toplevel_handle_v1::State as TState;
                let is_activated = raw_state.chunks_exact(4).any(|b| {
                    u32::from_ne_bytes(b.try_into().expect("chunks_exact(4) guarantees 4 bytes"))
                        == TState::Activated as u32
                });
                state.toplevels.entry(id).or_default().activated = is_activated;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                state.report_current_focus();
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.remove(&id);
                state.report_current_focus();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for AppData {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

pub async fn spawn() -> anyhow::Result<mpsc::Receiver<FocusEvent>> {
    // Probe synchronously to fail fast before spawning a thread.
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init::<AppData>(&conn)?;

    globals.bind::<ZwlrForeignToplevelManagerV1, _, _>(&event_queue.handle(), 1..=3, ())?;
    drop(event_queue);

    let (tx, rx) = mpsc::channel::<FocusEvent>(32);

    std::thread::spawn(move || {
        let conn = match Connection::connect_to_env() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("[FocusWatcher/Wayland] connect failed: {e}");
                return;
            }
        };
        let (globals, mut event_queue) = match registry_queue_init::<AppData>(&conn) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("[FocusWatcher/Wayland] registry_queue_init: {e}");
                return;
            }
        };
        let qh = event_queue.handle();
        let _manager = match globals.bind::<ZwlrForeignToplevelManagerV1, _, _>(&qh, 1..=3, ()) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("[FocusWatcher/Wayland] bind failed in watcher thread: {e}");
                return;
            }
        };

        let mut data = AppData {
            tx,
            active_app_id: None,
            toplevels: HashMap::new(),
            disconnected: false,
        };

        loop {
            if event_queue.blocking_dispatch(&mut data).is_err() || data.disconnected {
                break;
            }
        }
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_data(tx: mpsc::Sender<FocusEvent>) -> AppData {
        AppData {
            tx,
            active_app_id: None,
            toplevels: HashMap::new(),
            disconnected: false,
        }
    }

    #[test]
    fn report_focus_marks_disconnected_when_receiver_dropped() {
        let (tx, rx) = mpsc::channel::<FocusEvent>(1);
        drop(rx);
        let mut data = app_data(tx);
        data.report_focus("firefox");
        // A dropped receiver makes blocking_send fail; the loop must learn it should exit.
        assert!(data.disconnected, "send failure must set disconnected");
    }

    #[test]
    fn report_focus_stays_connected_while_receiver_lives() {
        let (tx, mut rx) = mpsc::channel::<FocusEvent>(1);
        let mut data = app_data(tx);
        data.report_focus("firefox");
        assert!(!data.disconnected, "live receiver must not disconnect");
        match rx.try_recv() {
            Ok(FocusEvent::AppFocused { process_name }) => {
                assert_eq!(process_name, super::super::normalize_name("firefox"));
            }
            other => panic!("expected AppFocused, got {other:?}"),
        }
    }

    #[test]
    fn report_focus_suppresses_duplicate_app() {
        let (tx, mut rx) = mpsc::channel::<FocusEvent>(4);
        let mut data = app_data(tx);
        data.report_focus("firefox");
        data.report_focus("firefox");
        assert!(rx.try_recv().is_ok(), "first focus emits");
        assert!(
            rx.try_recv().is_err(),
            "repeat of same app must not re-emit"
        );
    }
}
