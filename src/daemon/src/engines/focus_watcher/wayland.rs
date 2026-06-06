use tokio::sync::mpsc;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use super::FocusEvent;

struct AppData {
    tx: std::sync::mpsc::SyncSender<FocusEvent>,
    active_app_id: Option<String>,
    pending_app_id: Option<String>,
    pending_activated: bool,
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for AppData {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrForeignToplevelManagerV1,
        _event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for AppData {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                state.pending_app_id = Some(app_id);
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: raw_state } => {
                use zwlr_foreign_toplevel_handle_v1::State as TState;
                let is_activated = raw_state
                    .chunks_exact(4)
                    .any(|b| u32::from_ne_bytes(b.try_into().unwrap()) == TState::Activated as u32);
                state.pending_activated = is_activated;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                if state.pending_activated {
                    if let Some(app_id) = state.pending_app_id.take() {
                        let normalized = super::normalize_name(&app_id);
                        if state.active_app_id.as_deref() != Some(&normalized) {
                            state.active_app_id = Some(normalized.clone());
                            let _ = state.tx.send(FocusEvent::AppFocused {
                                process_name: normalized,
                            });
                        }
                    }
                }
                state.pending_app_id = None;
                state.pending_activated = false;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {}
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
    ) {}
}

pub async fn spawn() -> anyhow::Result<mpsc::Receiver<FocusEvent>> {
    // Probe the connection synchronously to fail fast before spawning a thread.
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init::<AppData>(&conn)?;

    // Check the protocol is available.
    globals.bind::<ZwlrForeignToplevelManagerV1, _, _>(&event_queue.handle(), 1..=3, ())?;
    drop(event_queue);

    let (bridge_tx, bridge_rx) = std::sync::mpsc::sync_channel::<FocusEvent>(32);
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
        let _manager = globals
            .bind::<ZwlrForeignToplevelManagerV1, _, _>(&qh, 1..=3, ())
            .unwrap();

        let mut data = AppData {
            tx: bridge_tx,
            active_app_id: None,
            pending_app_id: None,
            pending_activated: false,
        };

        loop {
            if event_queue.blocking_dispatch(&mut data).is_err() {
                break;
            }
        }
    });

    // Bridge: forward from std::sync::mpsc to tokio mpsc
    tokio::task::spawn_blocking(move || {
        while let Ok(event) = bridge_rx.recv() {
            if tx.blocking_send(event).is_err() {
                break;
            }
        }
    });

    Ok(rx)
}
