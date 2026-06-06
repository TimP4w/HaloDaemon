use std::cell::{Cell, RefCell};
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::io::Write;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use halod_protocol::debug_info::DebugInfo;
use halod_protocol::types::{DiscoveryPhase, Notification, NotificationSeverity, RunningApp};

use crate::ipc::client::DaemonMsg;
use crate::ipc::IpcSender;
use crate::state::AppState;
use crate::commands::Command;

#[derive(Clone)]
pub enum NavTarget {
    Device(String),
    Home,
    Canvas,
    Cooling,
    Lighting,
    AppRules,
    Settings,
}


struct Subscription {
    selector: Box<dyn Fn(&AppState) -> u64>,
    callback: Box<dyn Fn(&AppState)>,
    last_hash: Cell<u64>,
}

struct StoreInner {
    state:           RefCell<AppState>,
    ipc:             IpcSender,
    connected:       Cell<bool>,
    ever_discovered: Cell<bool>,
    subscriptions:   RefCell<Vec<Subscription>>,
    conn_subs:       RefCell<Vec<Box<dyn Fn(bool)>>>,
    debug_subs:         RefCell<Vec<Box<dyn Fn(&DebugInfo)>>>,
    running_apps_cbs:   RefCell<Vec<Box<dyn Fn(Vec<RunningApp>)>>>,
    nav_fn:          RefCell<Option<Box<dyn Fn(NavTarget)>>>,
    toast_fn:        RefCell<Option<Box<dyn Fn(&Notification)>>>,
    lcd_images_fn:   RefCell<Option<Box<dyn Fn(&[serde_json::Value])>>>,
    img_uploaded_fn: RefCell<Option<Box<dyn Fn(&str)>>>,
    upload_error_fn: RefCell<Option<Box<dyn Fn()>>>,
}

/// Cheap-clone handle to shared app state + IPC dispatch.
/// Replaces `AppContext` entirely.
#[derive(Clone)]
pub struct Store(Rc<StoreInner>);

impl Store {
    pub fn new(ipc: IpcSender) -> Self {
        Store(Rc::new(StoreInner {
            state:           RefCell::new(AppState::default()),
            ipc,
            connected:       Cell::new(false),
            ever_discovered: Cell::new(false),
            subscriptions:   RefCell::new(Vec::new()),
            conn_subs:       RefCell::new(Vec::new()),
            debug_subs:         RefCell::new(Vec::new()),
            running_apps_cbs:   RefCell::new(Vec::new()),
            nav_fn:          RefCell::new(None),
            toast_fn:        RefCell::new(None),
            lcd_images_fn:   RefCell::new(None),
            img_uploaded_fn: RefCell::new(None),
            upload_error_fn: RefCell::new(None),
        }))
    }

    pub fn state(&self) -> std::cell::Ref<'_, AppState> {
        self.0.state.borrow()
    }

    pub fn is_connected(&self) -> bool {
        self.0.connected.get()
    }

    pub fn ever_discovered(&self) -> bool {
        self.0.ever_discovered.get()
    }

    /// Access the underlying IPC sender for sending raw JSON commands to the daemon.
    /// Widgets that haven't been migrated to `dispatch()` use this directly.
    pub fn ipc(&self) -> &IpcSender {
        &self.0.ipc
    }

    /// Subscribe to a state slice identified by `selector`'s hash.
    /// `on_change` fires only when the hash changes — not on every broadcast.
    ///
    /// Constraints: `on_change` must not call `subscribe()` (would panic due
    /// to re-entrant borrow of `subscriptions`).  This is never needed in
    /// practice — all subscriptions are registered during `build()`.
    pub fn subscribe<S, F>(&self, selector: S, on_change: F)
    where
        S: Fn(&AppState) -> u64 + 'static,
        F: Fn(&AppState) + 'static,
    {
        self.0.subscriptions.borrow_mut().push(Subscription {
            selector:  Box::new(selector),
            callback:  Box::new(on_change),
            last_hash: Cell::new(u64::MAX), // MAX → first broadcast always fires
        });
    }

    pub fn on_connection<F: Fn(bool) + 'static>(&self, f: F) {
        self.0.conn_subs.borrow_mut().push(Box::new(f));
    }

    pub fn request_debug_info<F: Fn(&DebugInfo) + 'static>(&self, f: F) {
        self.0.debug_subs.borrow_mut().push(Box::new(f));
        self.0.ipc.send(serde_json::json!({"type": "get_debug_info"}));
    }

    pub fn request_running_apps<F: Fn(Vec<RunningApp>) + 'static>(&self, f: F) {
        self.0.running_apps_cbs.borrow_mut().push(Box::new(f));
        self.0.ipc.send(serde_json::json!({"type": "list_running_apps"}));
    }

    /// All user-initiated actions. Translates to JSON and sends to daemon.
    pub fn dispatch(&self, cmd: Command) {
        self.0.ipc.send(cmd.to_json());
    }

    pub fn set_nav<F: Fn(NavTarget) + 'static>(&self, f: F) {
        *self.0.nav_fn.borrow_mut() = Some(Box::new(f));
    }

    pub fn navigate(&self, target: NavTarget) {
        if let Some(f) = self.0.nav_fn.borrow().as_ref() { f(target); }
    }

    pub fn set_toast<F: Fn(&Notification) + 'static>(&self, f: F) {
        *self.0.toast_fn.borrow_mut() = Some(Box::new(f));
    }

    /// Display a message as an error-severity notification.
    pub fn show_toast(&self, msg: &str) {
        let n = Notification {
            severity:     NotificationSeverity::Error,
            title:        String::new(),
            message:      msg.to_string(),
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        };
        self.show_notification(&n);
    }

    pub fn show_notification(&self, n: &Notification) {
        if let Some(f) = self.0.toast_fn.borrow().as_ref() { f(n); }
    }

    /// Register LCD-specific callbacks. Called once by DevicePage after it
    /// creates its LcdWidget.
    pub fn set_lcd_callbacks(
        &self,
        on_images:   impl Fn(&[serde_json::Value]) + 'static,
        on_uploaded: impl Fn(&str) + 'static,
        on_error:    impl Fn() + 'static,
    ) {
        *self.0.lcd_images_fn.borrow_mut()   = Some(Box::new(on_images));
        *self.0.img_uploaded_fn.borrow_mut() = Some(Box::new(on_uploaded));
        *self.0.upload_error_fn.borrow_mut() = Some(Box::new(on_error));
    }

    /// Two-phase update called from the GTK main loop:
    /// 1. Mutate AppState atomically.
    /// 2. Notify only subscribers whose selector hash changed.
    ///
    /// No subscriber ever observes a partially-updated state, so a profile
    /// switch that changes active_profile + fan_curves + canvas simultaneously
    /// is always seen as a consistent snapshot.
    pub fn apply_msg(&self, msg: DaemonMsg) {
        use DaemonMsg::*;
        match msg {
            State(data) => {
                {
                    let mut s = self.0.state.borrow_mut();
                    s.apply_json(&data);
                    if matches!(s.discovery.phase, DiscoveryPhase::Complete) {
                        self.0.ever_discovered.set(true);
                    }
                }
                let state = self.0.state.borrow();
                for sub in self.0.subscriptions.borrow().iter() {
                    let h = (sub.selector)(&state);
                    if h != sub.last_hash.get() {
                        sub.last_hash.set(h);
                        (sub.callback)(&state);
                    }
                }
            }
            Connected => {
                self.0.ipc.send(serde_json::json!({"type": "canvas_subscribe"}));
                self.0.ipc.send(serde_json::json!({"type": "lcd_engine_subscribe"}));
                self.0.connected.set(true);
                for sub in self.0.conn_subs.borrow().iter() { sub(true); }
            }
            Disconnected => {
                self.0.connected.set(false);
                for sub in self.0.conn_subs.borrow().iter() { sub(false); }
            }
            LcdImages(files) => {
                if let Some(f) = self.0.lcd_images_fn.borrow().as_ref() { f(&files); }
            }
            ImageUploaded { request_id } => {
                if let Some(f) = self.0.img_uploaded_fn.borrow().as_ref() { f(&request_id); }
            }
            DebugInfo(info) => {
                let subs: Vec<_> = std::mem::take(&mut self.0.debug_subs.borrow_mut());
                for sub in &subs { sub(&info); }
            }
            Error(msg) => {
                self.show_toast(&msg);
                if let Some(f) = self.0.upload_error_fn.borrow().as_ref() { f(); }
            }
            Notification(n) => self.show_notification(&n),
            RunningApps(apps) => {
                let cbs: Vec<_> = std::mem::take(&mut *self.0.running_apps_cbs.borrow_mut());
                for cb in &cbs { cb(apps.clone()); }
            }
        }
    }
}

/// Hash the JSON serialization of any `Serialize` value.
/// Used to build selector closures without needing `Hash` on protocol types.
///
/// ```rust
/// store.subscribe(
///     move |st| sel_hash(&st.fan_curves.iter().find(|f| f.fan_id == fid)),
///     move |st| widget.update_live(st),
/// );
/// ```
pub fn sel_hash(v: &impl serde::Serialize) -> u64 {
    struct HashWriter(DefaultHasher);
    impl Write for HashWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.write(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut hw = HashWriter(DefaultHasher::new());
    if let Err(e) = serde_json::to_writer(&mut hw, v) {
        log::warn!("sel_hash serialize failed: {e}");
        return 0;
    }
    hw.0.finish()
}
