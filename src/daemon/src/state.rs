use std::collections::{HashMap, VecDeque};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, OnceLock};
use tokio::sync::{broadcast, mpsc, watch, Mutex, RwLock};

use crate::config::Config;
use crate::drivers::Device;
use crate::engines::{canvas::CanvasEngine, fan_curve::FanCurveEngine, lcd::LcdEngine};
use crate::engines::focus_watcher::{ControlMsg, FocusWatcherEngine};
use crate::ipc::ClientHandle;
use halod_protocol::types::{DiscoveryStatus, FanCurveStatus, LogEntry, Sensor};

/// A button press or release event from a diverted HID++ button.
#[derive(Debug, Clone)]
pub struct ButtonEvent {
    /// ID of the device that emitted the event.
    pub device_id: String,
    /// CIDs that transitioned to pressed since the last event.
    pub pressed: Vec<u16>,
    /// CIDs that transitioned to released since the last event.
    pub released: Vec<u16>,
}

/// Value stored in `hid_device_tracking`.
pub enum HidTrackingEntry {
    /// Normal path: device(s) created for this HID key.
    /// When the key disappears, close and remove all of them.
    Primary(Vec<Arc<dyn Device>>),
    /// Wired override: an existing device adopted this HID key's transport.
    /// When the key disappears, revert the device's transport instead of removing it.
    WiredOverride(Arc<dyn Device>),
}

/// Runtime configuration sent to each engine via a watch channel.
#[derive(Debug, Clone)]
pub struct EngineRunConfig {
    pub enabled: bool,
    /// Interval in milliseconds (engines convert fps → ms themselves).
    pub tick_ms: u64,
    /// Duty applied when a fan's sensor is absent. Only used by the fan-curve engine.
    pub failsafe_duty: u8,
}

pub struct Engines {
    pub canvas: Arc<CanvasEngine>,
    pub fan_curve: Arc<FanCurveEngine>,
    pub lcd: Arc<LcdEngine>,
    pub focus_watcher: Arc<FocusWatcherEngine>,
    pub fan_curve_cfg_tx: watch::Sender<EngineRunConfig>,
    pub canvas_cfg_tx: watch::Sender<EngineRunConfig>,
    pub lcd_cfg_tx: watch::Sender<EngineRunConfig>,
    pub focus_watcher_cfg_tx: watch::Sender<EngineRunConfig>,
}

pub struct AppState {
    pub config: RwLock<Config>,
    pub clients: Mutex<Vec<ClientHandle>>,
    pub discovery: Mutex<DiscoveryStatus>,
    pub devices: Mutex<Vec<Arc<dyn Device>>>,
    /// Maps HID key ("vid:pid:serial") → tracking entry.
    pub hid_device_tracking: Mutex<HashMap<String, HidTrackingEntry>>,
    /// Per-fan curve status written by the engine, read by the serializer.
    pub fan_curve_statuses: Mutex<HashMap<String, FanCurveStatus>>,
    pub engines: OnceLock<Engines>,
    /// Ring-buffer of recent log entries written by the custom logger.
    pub log_buffer: Arc<std::sync::Mutex<VecDeque<LogEntry>>>,
    /// Button events emitted by diverted HID++ buttons; consumed by KeyRemapEngine.
    pub button_event_tx: broadcast::Sender<ButtonEvent>,
    /// True while the global Layer Shift modifier is held down.
    pub layer_shift_active: Arc<Mutex<bool>>,
    /// Notified to request a graceful daemon shutdown (e.g. an IPC `shutdown`
    /// command from the tray on a dev/plain run).
    pub shutdown: tokio::sync::Notify,
    /// True when this process is the service `--worker` (relaunched by the
    /// supervisor). Determines how an IPC `shutdown` is handled.
    pub is_service_worker: bool,
    /// Debounced config save channel. Send a config snapshot to schedule a save.
    pub config_save_tx: watch::Sender<Option<Config>>,
    /// Channel to the focus watcher engine; None until the engine is started.
    pub focus_watcher_tx: Mutex<Option<mpsc::Sender<ControlMsg>>>,
    /// Set to true once a platform focus backend starts successfully.
    pub focus_watcher_supported: AtomicBool,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        let (button_event_tx, _) = broadcast::channel(256);
        let (config_save_tx, _) = watch::channel(None);
        Self {
            config: RwLock::new(cfg),
            clients: Mutex::new(Vec::new()),
            discovery: Mutex::new(DiscoveryStatus::default()),
            devices: Mutex::new(Vec::new()),
            hid_device_tracking: Mutex::new(HashMap::new()),
            fan_curve_statuses: Mutex::new(HashMap::new()),
            engines: OnceLock::new(),
            log_buffer: Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(500))),
            button_event_tx,
            layer_shift_active: Arc::new(Mutex::new(false)),
            shutdown: tokio::sync::Notify::new(),
            is_service_worker: false,
            config_save_tx,
            focus_watcher_tx: Mutex::new(None),
            focus_watcher_supported: AtomicBool::new(false),
        }
    }

    pub fn set_focus_watcher_supported(&self, v: bool) {
        self.focus_watcher_supported.store(v, Ordering::Relaxed);
    }

    pub fn request_config_save(&self, cfg: Config) {
        let _ = self.config_save_tx.send(Some(cfg));
    }

    /// Mark this process as the service `--worker`. Builder-style so existing
    /// `AppState::new(cfg)` call sites (including tests) are unaffected.
    pub fn with_service_worker(mut self, is_worker: bool) -> Self {
        self.is_service_worker = is_worker;
        self
    }

    pub async fn find_device_by_id(&self, id: &str) -> Option<Arc<dyn Device>> {
        self.devices
            .lock()
            .await
            .iter()
            .find(|d| d.id() == id)
            .cloned()
    }

    pub async fn find_sensor_by_id(&self, sensor_id: &str) -> Option<Sensor> {
        let devices = self.devices.lock().await.clone();
        for device in &devices {
            if let Some(cap) = device.as_sensor_capability() {
                if let Ok(sensors) = cap.get_sensors().await {
                    if let Some(s) = sensors.into_iter().find(|s| s.id == sensor_id) {
                        return Some(s);
                    }
                }
            }
        }
        None
    }

    pub async fn get_active_devices(&self) -> Vec<Arc<dyn Device>> {
        let cfg = self.config.read().await;
        self.devices
            .lock()
            .await
            .iter()
            .filter(|d| {
                cfg.known_devices
                    .get(&d.id())
                    .map(|r| r.active_state == halod_protocol::types::VisibilityState::Visible)
                    .unwrap_or(true)
            })
            .cloned()
            .collect()
    }
}

pub fn start_config_save_worker(
    mut rx: watch::Receiver<Option<Config>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if rx.changed().await.is_err() {
                break;
            }
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_millis(250);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    result = rx.changed() => {
                        if result.is_err() { return; }
                    }
                }
            }
            let cfg = rx.borrow().clone();
            if let Some(cfg) = cfg {
                let result = tokio::task::spawn_blocking(move || crate::config::save(&cfg)).await;
                match result {
                    Err(e) => log::warn!("[Config] Save worker panicked: {e}"),
                    Ok(Err(e)) => {
                        log::warn!("[Config] Save failed, will retry on next change: {e}");
                    }
                    Ok(Ok(())) => {}
                }
            }
        }
    })
}

pub async fn shutdown(app: Arc<AppState>) {
    log::info!("Gracefully shutting down...");
    let devices = app.devices.lock().await.clone();
    for device in devices {
        device.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn service_worker_flag_defaults_off() {
        let app = AppState::new(Config::default());
        assert!(!app.is_service_worker);
    }

    #[test]
    fn with_service_worker_sets_the_flag() {
        let app = AppState::new(Config::default()).with_service_worker(true);
        assert!(app.is_service_worker);
    }
}
