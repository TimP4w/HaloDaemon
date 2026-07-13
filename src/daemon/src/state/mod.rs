// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{watch, Mutex, RwLock};

use crate::config::Config;
use crate::ipc::ClientHandle;
use crate::registry::discovery::{DiscoveryScope, PendingRediscovery};
use halod_shared::types::{DiscoveryStatus, LogEntry};

mod persistence;
mod workers;

pub use crate::cooling::CoolingEngineState;
pub use crate::input::{ButtonEvent, InputState};
pub use crate::lcd::LcdEngineState;
pub use crate::lighting::LightingState;
pub use crate::profiles::FocusState;
pub use crate::registry::{HidTracking, HidTrackingEntry};
pub use crate::run_loop::EngineRunConfig;
pub use persistence::Persistence;
pub use workers::{shutdown, start_config_save_worker, start_persist_worker};

pub struct AppState {
    // --- Cross-cutting spine (used by nearly every domain) ---
    pub config: RwLock<Config>,
    /// Device registry. `RwLock` because every engine tick, serializer pass, and
    /// `find_device_by_id` is a reader; only discovery/removal takes the write lock.
    pub devices: RwLock<Vec<Arc<dyn crate::drivers::Device>>>,
    /// Device ids currently going through asynchronous initialization. Several
    /// transport scanners run concurrently, so checking only `devices` leaves
    /// a window where the same physical device can be initialized twice.
    pub device_registrations: Mutex<std::collections::HashSet<String>>,

    // --- Domains ---
    pub discovery: Mutex<DiscoveryStatus>,
    pub hid: HidTracking,
    pub lighting: LightingState,
    pub cooling: CoolingEngineState,
    pub lcd: LcdEngineState,
    pub focus: FocusState,
    pub input: InputState,
    pub persistence: Persistence,

    // --- Runtime / lifecycle infra ---
    /// Per-device command serialization. Commands targeting the same device
    /// acquire the same lock (run in order, never overlap); commands to
    /// different devices run concurrently, so one slow/timing-out device can't
    /// stall the whole IPC command stream.
    pub device_locks: Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>,
    pub clients: Mutex<Vec<ClientHandle>>,
    /// Ring-buffer of recent log entries written by the custom logger.
    pub log_buffer: Arc<std::sync::Mutex<VecDeque<LogEntry>>>,
    /// Flipped to `true` once every domain's engine has been set
    pub engines_ready: watch::Sender<bool>,
    /// Notified to request a graceful daemon shutdown (e.g. an IPC `shutdown`
    /// command from the tray on a dev/plain run).
    pub shutdown: tokio::sync::Notify,
    /// Discovery gate consulted by scanners. See [`DiscoveryScope`].
    pub discovery_scope: RwLock<DiscoveryScope>,
    /// Atomically merged pending rediscovery work; see [`PendingRediscovery`].
    pub pending_rediscovery: Mutex<PendingRediscovery>,
    /// Elects one drain loop. Callers may merge work while another drain runs,
    /// then wait on this gate until their request has been consumed.
    pub rediscovery_runner: Mutex<()>,
    /// Latest plugin update/on-disk status, replayed to each client on connect.
    pub plugin_update_status: Mutex<Vec<halod_shared::types::PluginUpdateStatus>>,
    /// The device-plugin registry (loaded manifests, consent/config, notice
    /// dedup, load warnings).
    pub registry: crate::drivers::plugins::Registry,
    /// Backing store for plugin-declared secret config values.
    pub secret_store: Arc<dyn crate::secrets::SecretStore>,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        Self {
            config: RwLock::new(cfg),
            devices: RwLock::new(Vec::new()),
            device_registrations: Mutex::new(std::collections::HashSet::new()),
            discovery: Mutex::new(DiscoveryStatus::default()),
            hid: HidTracking::new(),
            lighting: LightingState::default(),
            cooling: CoolingEngineState::new(),
            lcd: LcdEngineState::new(),
            focus: FocusState::new(),
            input: InputState::new(),
            persistence: Persistence::new(),
            device_locks: Mutex::new(std::collections::HashMap::new()),
            clients: Mutex::new(Vec::new()),
            log_buffer: Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(
                crate::logger::BUFFER_CAP,
            ))),
            engines_ready: watch::channel(false).0,
            shutdown: tokio::sync::Notify::new(),
            discovery_scope: RwLock::new(DiscoveryScope::Clean),
            pending_rediscovery: Mutex::new(PendingRediscovery::Clean),
            rediscovery_runner: Mutex::new(()),
            plugin_update_status: Mutex::new(Vec::new()),
            registry: crate::drivers::plugins::Registry::default(),
            secret_store: Arc::new(crate::secrets::FileKeyStore::new()),
        }
    }

    /// The serialization lock for one device id, created on first use. Held for
    /// the duration of a device-scoped command so concurrent commands to the
    /// same device don't interleave; different devices get different locks.
    pub async fn device_lock(&self, id: &str) -> Arc<Mutex<()>> {
        Arc::clone(
            self.device_locks
                .lock()
                .await
                .entry(id.to_string())
                .or_default(),
        )
    }

    /// Signal that the config is dirty. The save worker debounces and snapshots
    /// `self.config` once when the window fires, so callers need not clone.
    pub fn request_config_save(&self) {
        self.persistence.save_tx.send_modify(|version| {
            *version = version.wrapping_add(1);
        });
    }

    /// Override the secret store (e.g. with `crate::secrets::open_secret_store()`
    /// to prefer the OS keyring). Builder-style, like `with_secret_store`.
    pub fn with_secret_store(mut self, store: Arc<dyn crate::secrets::SecretStore>) -> Self {
        self.secret_store = store;
        self
    }

    /// Push a full state snapshot to all connected GUI clients.
    ///
    /// Background tasks inside driver modules (e.g. notification watchers, DPI
    /// pollers) call this instead of importing `crate::ipc` directly, keeping the
    /// IPC layer out of the driver layer.
    pub async fn broadcast_state(self: &Arc<Self>) {
        crate::ipc::broadcast_state(self).await;
    }

    /// Last `n` log entries from the ring buffer (empty if the lock is poisoned).
    pub fn recent_logs(&self, n: usize) -> Vec<LogEntry> {
        self.log_buffer
            .lock()
            .map(|buf| {
                let skip = buf.len().saturating_sub(n);
                buf.iter().skip(skip).cloned().collect()
            })
            .unwrap_or_default()
    }

    pub async fn set_discovery_scope(&self, scope: DiscoveryScope) {
        *self.discovery_scope.write().await = scope;
    }

    pub async fn merge_rediscovery(&self, request: PendingRediscovery) {
        self.pending_rediscovery.lock().await.merge(request);
    }

    pub async fn take_rediscovery(&self) -> PendingRediscovery {
        self.pending_rediscovery.lock().await.take()
    }

    /// True when `handle` passes the current scope (`PluginSet`'s filter, or
    /// unconditionally under `Clean`/`Full`).
    pub async fn handle_in_scope(
        &self,
        handle: &crate::registry::discovery::DiscoveryHandle<'_>,
    ) -> bool {
        match &*self.discovery_scope.read().await {
            DiscoveryScope::PluginSet { filter, .. } => filter.matches(handle),
            DiscoveryScope::Clean | DiscoveryScope::Full => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn make_test_app() -> AppState {
        AppState::new(Config::default())
    }

    #[tokio::test]
    async fn snapshot_sensors_collects_sensors_from_all_devices() {
        let app = make_test_app();
        let dev1 = std::sync::Arc::new(
            crate::test_support::MockDevice::new("sensor_dev_1").with_sensor(vec![
                halod_shared::types::Sensor {
                    id: "temp1".into(),
                    name: "CPU Temp".into(),
                    value: 45.5,
                    unit: halod_shared::types::SensorUnit::Celsius,
                    sensor_type: halod_shared::types::SensorType::Temperature,
                    visibility: halod_shared::types::VisibilityState::Visible,
                },
            ]),
        );
        let dev2 = std::sync::Arc::new(
            crate::test_support::MockDevice::new("sensor_dev_2").with_sensor(vec![
                halod_shared::types::Sensor {
                    id: "temp2".into(),
                    name: "GPU Temp".into(),
                    value: 62.0,
                    unit: halod_shared::types::SensorUnit::Celsius,
                    sensor_type: halod_shared::types::SensorType::Temperature,
                    visibility: halod_shared::types::VisibilityState::Visible,
                },
            ]),
        );
        app.devices
            .write()
            .await
            .push(dev1 as std::sync::Arc<dyn crate::drivers::Device>);
        app.devices
            .write()
            .await
            .push(dev2 as std::sync::Arc<dyn crate::drivers::Device>);

        let snapshot = app.snapshot_sensors().await;
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot.get("temp1").unwrap().value, 45.5);
        assert_eq!(snapshot.get("temp2").unwrap().value, 62.0);
    }

    #[tokio::test]
    async fn snapshot_sensors_excludes_disabled_devices() {
        let mut cfg = Config::default();
        cfg.known_devices.insert(
            "disabled_dev".into(),
            crate::registry::config::DeviceRecord {
                name: String::new(),
                vendor: String::new(),
                model: String::new(),
                active_state: halod_shared::types::VisibilityState::Disabled,
            },
        );
        let app = AppState::new(cfg);
        let dev = std::sync::Arc::new(
            crate::test_support::MockDevice::new("disabled_dev")
                .with_fan_rpm(900)
                .with_sensor(vec![halod_shared::types::Sensor {
                    id: "temp1".into(),
                    name: "CPU Temp".into(),
                    value: 45.5,
                    unit: halod_shared::types::SensorUnit::Celsius,
                    sensor_type: halod_shared::types::SensorType::Temperature,
                    visibility: halod_shared::types::VisibilityState::Visible,
                }]),
        );
        app.devices
            .write()
            .await
            .push(dev as std::sync::Arc<dyn crate::drivers::Device>);

        let snapshot = app.snapshot_sensors().await;
        assert!(snapshot.is_empty());
    }

    #[tokio::test]
    async fn snapshot_sensors_skips_devices_without_sensor_capability() {
        let app = make_test_app();
        let dev = std::sync::Arc::new(crate::test_support::MockDevice::new("no_sensor").with_rgb());
        app.devices
            .write()
            .await
            .push(dev as std::sync::Arc<dyn crate::drivers::Device>);

        let snapshot = app.snapshot_sensors().await;
        assert!(snapshot.is_empty());
    }

    #[tokio::test]
    async fn snapshot_sensors_includes_synthesized_fan_readings() {
        let app = make_test_app();
        let dev =
            std::sync::Arc::new(crate::test_support::MockDevice::new("fan0").with_fan_rpm(900));
        app.devices
            .write()
            .await
            .push(dev as std::sync::Arc<dyn crate::drivers::Device>);

        let snapshot = app.snapshot_sensors().await;
        assert_eq!(snapshot.get("fan_fan0_rpm").map(|s| s.value), Some(900.0));
        assert!(snapshot.contains_key("fan_fan0_duty"));
    }

    #[tokio::test]
    async fn get_active_devices_filters_by_visibility_state() {
        let app = make_test_app();

        let visible_dev = std::sync::Arc::new(crate::test_support::MockDevice::new("visible_dev"));
        let hidden_dev = std::sync::Arc::new(crate::test_support::MockDevice::new("hidden_dev"));
        let unknown_dev = std::sync::Arc::new(crate::test_support::MockDevice::new("unknown_dev"));

        app.devices
            .write()
            .await
            .push(visible_dev.clone() as std::sync::Arc<dyn crate::drivers::Device>);
        app.devices
            .write()
            .await
            .push(hidden_dev.clone() as std::sync::Arc<dyn crate::drivers::Device>);
        app.devices
            .write()
            .await
            .push(unknown_dev.clone() as std::sync::Arc<dyn crate::drivers::Device>);

        // Set up known_devices: visible_dev is Visible, hidden_dev is Hidden,
        // unknown_dev is absent (falls through to unwrap_or(true)).
        {
            let mut cfg = app.config.write().await;
            cfg.known_devices.insert(
                "visible_dev".into(),
                crate::registry::config::DeviceRecord {
                    active_state: halod_shared::types::VisibilityState::Visible,
                    ..Default::default()
                },
            );
            cfg.known_devices.insert(
                "hidden_dev".into(),
                crate::registry::config::DeviceRecord {
                    active_state: halod_shared::types::VisibilityState::Hidden,
                    ..Default::default()
                },
            );
        }

        let active = app.get_active_devices().await;
        let ids: Vec<String> = active.iter().map(|d| d.id().to_owned()).collect();
        assert!(
            ids.contains(&"visible_dev".to_string()),
            "visible device must be active"
        );
        assert!(
            ids.contains(&"unknown_dev".to_string()),
            "unknown device defaults to active"
        );
        assert!(
            !ids.contains(&"hidden_dev".to_string()),
            "hidden device must not be active"
        );
    }

    #[test]
    fn recent_logs_truncates_to_n_most_recent() {
        let app = make_test_app();
        {
            let mut buf = app.log_buffer.lock().unwrap();
            for i in 0..5 {
                buf.push_back(LogEntry {
                    level: "INFO".to_string(),
                    target: String::new(),
                    message: format!("entry {i}"),
                    timestamp_ms: 0,
                });
            }
        }

        let logs = app.recent_logs(2);
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].message, "entry 3");
        assert_eq!(logs[1].message, "entry 4");
    }

    #[test]
    fn canvas_and_lcd_fps_are_clamped_to_1_240() {
        use crate::config::{LcdConfig, RgbConfig};

        // Below the floor: clamps to 1 fps → 1000 ms tick.
        let rgb_floor = RgbConfig {
            canvas_fps: 0,
            ..Default::default()
        };
        let lcd_floor = LcdConfig {
            fps: 0,
            ..Default::default()
        };
        assert_eq!(EngineRunConfig::canvas(&rgb_floor).tick_ms, 1000);
        assert_eq!(EngineRunConfig::lcd(&lcd_floor).tick_ms, 1000);

        // Above the ceiling: clamps to 240 fps → 4 ms tick.
        let rgb_ceiling = RgbConfig {
            canvas_fps: 10_000,
            ..Default::default()
        };
        let lcd_ceiling = LcdConfig {
            fps: 10_000,
            ..Default::default()
        };
        assert_eq!(EngineRunConfig::canvas(&rgb_ceiling).tick_ms, 1000 / 240);
        assert_eq!(EngineRunConfig::lcd(&lcd_ceiling).tick_ms, 1000 / 240);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn save_worker_eventually_writes_config_to_disk() {
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };

        let app = Arc::new(AppState::new(Config::default()));
        let _worker = start_config_save_worker(Arc::clone(&app));
        app.request_config_save();

        // Poll up to 5 s: debounce (250 ms) + spawn_blocking overhead.
        // A plain fixed sleep fails under test-suite load when blocking threads are busy.
        let yaml_path = dir.path().join("config.yaml");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !yaml_path.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            yaml_path.exists(),
            "config.yaml must be written after a save signal"
        );

        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn save_worker_debounces_rapid_signals_into_single_save() {
        let _guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };

        let app = Arc::new(AppState::new(Config::default()));
        let _worker = start_config_save_worker(Arc::clone(&app));

        // Fire 5 signals in rapid succession, all within the debounce window.
        for _ in 0..5 {
            app.request_config_save();
        }
        // File must not exist yet — debounce window has not elapsed.
        assert!(
            !dir.path().join("config.yaml").exists(),
            "config.yaml must not be written before the debounce window"
        );

        // Poll up to 5 s: debounce (250 ms) + spawn_blocking overhead.
        let yaml_path = dir.path().join("config.yaml");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !yaml_path.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            yaml_path.exists(),
            "config.yaml must be written after the debounce window"
        );
        assert!(
            std::fs::metadata(&yaml_path).unwrap().len() > 0,
            "config.yaml must be non-empty"
        );

        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn handle_in_scope_defaults_open_and_honours_the_filter() {
        use crate::registry::discovery::{DiscoveryFilter, DiscoveryHandle};
        let app = make_test_app();
        let handle = DiscoveryHandle::Hid {
            vid: 1,
            pid: 2,
            path: "",
            serial: None,
            idx: 0,
            usage_page: 0,
            usage: 0,
            interface_number: None,
        };

        // Clean — every handle passes.
        assert!(app.handle_in_scope(&handle).await);

        let spec = serde_json::from_value(serde_json::json!({
            "vendor": "x", "model": "y", "transport": "hid", "vid": 9, "pid": 9,
        }))
        .unwrap();
        app.set_discovery_scope(DiscoveryScope::PluginSet {
            plugin_ids: ["p".to_string()].into_iter().collect(),
            filter: Arc::new(DiscoveryFilter { specs: vec![spec] }),
        })
        .await;
        assert!(!app.handle_in_scope(&handle).await, "out-of-scope handle");

        app.set_discovery_scope(DiscoveryScope::Full).await;
        assert!(app.handle_in_scope(&handle).await, "Full is unrestricted");

        app.set_discovery_scope(DiscoveryScope::Clean).await;
        assert!(app.handle_in_scope(&handle).await);
    }
}
