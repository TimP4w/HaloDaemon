// SPDX-License-Identifier: GPL-3.0-or-later
#[cfg(feature = "dev-plugin-repo")]
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{watch, Mutex, RwLock};

use crate::application::ipc::ClientHandle;
use crate::config::Config;
use crate::domain::registry::observers::discovery::{DiscoveryScope, PendingRediscovery};
use halod_shared::types::DiscoveryStatus;

mod config_repository;
mod device_registry;
mod persistence;
mod workers;

#[cfg(test)]
use crate::application::run_loop::EngineRunConfig;
pub use crate::domain::cooling::CoolingEngineState;
pub use crate::domain::input::{ButtonEvent, InputState};
pub use crate::domain::lcd::LcdEngineState;
pub use crate::domain::lighting::LightingState;
pub use crate::domain::profiles::FocusState;
pub use crate::domain::registry::{HidTracking, HidTrackingEntry};
pub use config_repository::ConfigRepository;
pub use device_registry::DeviceRegistry;
pub use persistence::Persistence;
pub use workers::{shutdown, start_config_save_worker, start_persist_worker};

pub struct AppState {
    // --- Cross-cutting spine (used by nearly every domain) ---
    pub config: ConfigRepository,
    /// Device registry. `RwLock` because every engine tick, topic-production pass, and
    /// `find_device_by_id` is a reader; only discovery/removal takes the write lock.
    pub device_registry: DeviceRegistry,

    // --- Domains ---
    pub discovery: Mutex<DiscoveryStatus>,
    pub hid: HidTracking,
    pub lighting: LightingState,
    pub cooling: CoolingEngineState,
    pub lcd: LcdEngineState,
    pub focus: FocusState,
    pub input: InputState,
    pub input_events: crate::domain::input::InputEventBus,

    // --- Runtime / lifecycle infra ---
    /// Per-device command serialization. Commands targeting the same device
    /// acquire the same lock (run in order, never overlap); commands to
    /// different devices run concurrently, so one slow/timing-out device can't
    /// stall the whole IPC command stream.
    /// Serializes atomic topic production. Concurrent changes can otherwise
    /// snapshot the same plugin devices at once and fill their single-threaded
    /// worker queues ahead of interactive writes.
    pub effective_state: crate::application::bus::coordinator::EffectiveStatePublisher,
    pub clients: Mutex<Vec<ClientHandle>>,
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
    /// Latest plugin update/on-disk status, retained in the plugins bus record.
    pub plugin_update_status: Mutex<Vec<halod_shared::types::PluginUpdateStatus>>,
    /// Latest repository update status, retained in the plugins bus record.
    pub plugin_repo_update_status: Mutex<Vec<halod_shared::types::RepoUpdateStatus>>,
    /// Latest signature result observed for a fetched repository commit, keyed
    /// by slug. The SHA distinguishes a rejected remote tip from the active revision.
    pub repo_signature_status: Mutex<
        std::collections::HashMap<String, (String, halod_shared::types::RepoSignatureStatus)>,
    >,
    /// Compatibility result for the latest fetched tip of each repository.
    pub repo_compatibility_status:
        Mutex<std::collections::HashMap<String, halod_shared::types::RepoCompatibilityStatus>>,
    /// The device-plugin registry (loaded manifests, consent/config, notice
    /// dedup, load warnings).
    pub registry: crate::domain::plugin::Registry,
    pub data_bus: Arc<crate::application::bus::data_bus::DataBus>,
    #[cfg(feature = "dev-plugin-repo")]
    /// Process-local development repository selected with `--dev-plugin-repo`.
    /// Registry rebuilds must retain this priority source rather than falling
    /// back to the managed official checkout.
    pub development_plugin_repo: RwLock<Option<PathBuf>>,
    /// Backing store for plugin-declared secret config values.
    pub secret_store: Arc<dyn crate::infrastructure::secrets::SecretStore>,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        Self {
            config: ConfigRepository::new(cfg),
            device_registry: DeviceRegistry::default(),
            discovery: Mutex::new(DiscoveryStatus::default()),
            hid: HidTracking::new(),
            lighting: LightingState::default(),
            cooling: CoolingEngineState::new(),
            lcd: LcdEngineState::new(),
            focus: FocusState::new(),
            input: InputState::new(),
            input_events: crate::domain::input::InputEventBus::new(),
            effective_state: crate::application::bus::coordinator::EffectiveStatePublisher::default(
            ),
            clients: Mutex::new(Vec::new()),
            engines_ready: watch::channel(false).0,
            shutdown: tokio::sync::Notify::new(),
            discovery_scope: RwLock::new(DiscoveryScope::Clean),
            pending_rediscovery: Mutex::new(PendingRediscovery::Clean),
            rediscovery_runner: Mutex::new(()),
            plugin_update_status: Mutex::new(Vec::new()),
            plugin_repo_update_status: Mutex::new(Vec::new()),
            repo_signature_status: Mutex::new(std::collections::HashMap::new()),
            repo_compatibility_status: Mutex::new(std::collections::HashMap::new()),
            registry: crate::domain::plugin::Registry::default(),
            data_bus: Arc::new(crate::application::bus::data_bus::DataBus::default()),
            #[cfg(feature = "dev-plugin-repo")]
            development_plugin_repo: RwLock::new(None),
            secret_store: Arc::new(crate::infrastructure::secrets::FileKeyStore::new()),
        }
    }

    /// The serialization lock for one device id, created on first use. Held for
    /// the duration of a device-scoped command so concurrent commands to the
    /// same device don't interleave; different devices get different locks.
    pub async fn device_lock(&self, id: &str) -> Arc<Mutex<()>> {
        self.device_registry.command_lock(id).await
    }

    /// Signal that the config is dirty. The save worker debounces and snapshots
    /// `self.config` once when the window fires, so callers need not clone.
    pub fn request_config_save(&self) {
        self.config.request_save();
    }

    /// Override the secret store (e.g. with `crate::infrastructure::secrets::open_secret_store()`
    /// to prefer the OS keyring). Builder-style, like `with_secret_store`.
    pub fn with_secret_store(
        mut self,
        store: Arc<dyn crate::infrastructure::secrets::SecretStore>,
    ) -> Self {
        self.secret_store = store;
        self
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
        handle: &crate::domain::registry::observers::discovery::DiscoveryHandle<'_>,
    ) -> bool {
        match &*self.discovery_scope.read().await {
            DiscoveryScope::PluginSet { filter, .. } => filter.matches(handle),
            DiscoveryScope::Clean | DiscoveryScope::Full => true,
        }
    }
}

#[async_trait::async_trait]
impl crate::domain::events::ChangeSink for AppState {
    async fn record_change(&self, change: crate::domain::events::Change) {
        self.effective_state.record(self, change).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn make_test_app() -> AppState {
        AppState::new(Config::default())
    }

    #[test]
    fn retained_record_commits_are_scoped_to_usecases() {
        fn visit(path: &std::path::Path, violations: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(path).expect("read source directory") {
                let path = entry.expect("read source entry").path();
                if path.is_dir() {
                    visit(&path, violations);
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs")
                    || path.ends_with("state/mod.rs")
                {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("read Rust source");
                if source.contains(".record_change(")
                    && !path
                        .components()
                        .any(|component| component.as_os_str() == "usecases")
                {
                    violations.push(path);
                }
            }
        }

        let mut violations = Vec::new();
        visit(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut violations,
        );
        assert!(
            violations.is_empty(),
            "retained records may only be committed by use cases: {violations:?}"
        );
    }

    #[tokio::test]
    async fn sensor_bus_collects_sensors_from_all_devices() {
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
        app.device_registry
            .write()
            .await
            .push(dev1 as std::sync::Arc<dyn crate::infrastructure::drivers::Device>);
        app.device_registry
            .write()
            .await
            .push(dev2 as std::sync::Arc<dyn crate::infrastructure::drivers::Device>);

        crate::application::usecases::device::telemetry::observe(&app).await;
        let snapshot = app.data_bus.sensors();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot.get("temp1").unwrap().value, 45.5);
        assert_eq!(snapshot.get("temp2").unwrap().value, 62.0);
    }

    #[tokio::test]
    async fn sensor_bus_excludes_disabled_devices() {
        let mut cfg = Config::default();
        cfg.known_devices.insert(
            "disabled_dev".into(),
            crate::domain::registry::model::DeviceRecord {
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
        app.device_registry
            .write()
            .await
            .push(dev as std::sync::Arc<dyn crate::infrastructure::drivers::Device>);

        crate::application::usecases::device::telemetry::observe(&app).await;
        let snapshot = app.data_bus.sensors();
        assert!(snapshot.is_empty());
    }

    #[tokio::test]
    async fn sensor_bus_invalidates_a_failed_producer() {
        use std::sync::atomic::Ordering;

        let app = make_test_app();
        let device = std::sync::Arc::new(
            crate::test_support::MockDevice::new("sensor_dev").with_sensor(vec![
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
        app.device_registry.write().await.push(device.clone());
        crate::application::usecases::device::telemetry::observe(&app).await;
        assert!(app.data_bus.sensors().contains_key("temp1"));

        device.live.store(false, Ordering::SeqCst);
        crate::application::usecases::device::telemetry::observe(&app).await;
        assert!(app.data_bus.sensors().is_empty());
        assert_eq!(
            app.data_bus
                .read(&crate::application::bus::data_bus::sensor_key("temp1"))
                .status,
            crate::application::bus::data_bus::SnapshotStatus::Unavailable
        );
    }

    #[tokio::test]
    async fn sensor_bus_skips_devices_without_sensor_capability() {
        let app = make_test_app();
        let dev = std::sync::Arc::new(crate::test_support::MockDevice::new("no_sensor").with_rgb());
        app.device_registry
            .write()
            .await
            .push(dev as std::sync::Arc<dyn crate::infrastructure::drivers::Device>);

        crate::application::usecases::device::telemetry::observe(&app).await;
        let snapshot = app.data_bus.sensors();
        assert!(snapshot.is_empty());
    }

    #[tokio::test]
    async fn sensor_bus_includes_synthesized_fan_readings() {
        let app = make_test_app();
        let dev =
            std::sync::Arc::new(crate::test_support::MockDevice::new("fan0").with_fan_rpm(900));
        app.device_registry
            .write()
            .await
            .push(dev as std::sync::Arc<dyn crate::infrastructure::drivers::Device>);

        crate::application::usecases::device::telemetry::observe(&app).await;
        let snapshot = app.data_bus.sensors();
        assert_eq!(
            snapshot.get("cooling_fan0_default_rpm").map(|s| s.value),
            Some(900.0)
        );
        assert!(snapshot.contains_key("cooling_fan0_default_duty"));
    }

    #[tokio::test]
    async fn get_active_devices_filters_by_visibility_state() {
        let app = make_test_app();

        let visible_dev = std::sync::Arc::new(crate::test_support::MockDevice::new("visible_dev"));
        let hidden_dev = std::sync::Arc::new(crate::test_support::MockDevice::new("hidden_dev"));
        let unknown_dev = std::sync::Arc::new(crate::test_support::MockDevice::new("unknown_dev"));

        app.device_registry
            .write()
            .await
            .push(visible_dev.clone() as std::sync::Arc<dyn crate::infrastructure::drivers::Device>);
        app.device_registry
            .write()
            .await
            .push(hidden_dev.clone() as std::sync::Arc<dyn crate::infrastructure::drivers::Device>);
        app.device_registry
            .write()
            .await
            .push(unknown_dev.clone() as std::sync::Arc<dyn crate::infrastructure::drivers::Device>);

        // Set up known_devices: visible_dev is Visible, hidden_dev is Hidden,
        // unknown_dev is absent (falls through to unwrap_or(true)).
        {
            let mut cfg = app.config.write().await;
            cfg.known_devices.insert(
                "visible_dev".into(),
                crate::domain::registry::model::DeviceRecord {
                    active_state: halod_shared::types::VisibilityState::Visible,
                    ..Default::default()
                },
            );
            cfg.known_devices.insert(
                "hidden_dev".into(),
                crate::domain::registry::model::DeviceRecord {
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
        use crate::domain::registry::observers::discovery::{DiscoveryFilter, DiscoveryHandle};
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

        let mut spec: crate::domain::plugin::DeviceSpec =
            serde_json::from_value(serde_json::json!({
            "vendor": "x", "model": "y",
            "match": { "hid": { "vid": 9, "pid": 9 } }
            }))
            .unwrap();
        spec.transport = "hid".to_owned();
        spec.vid = Some(9);
        spec.pid = Some(9);
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
