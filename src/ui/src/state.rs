pub use halod_protocol::types::{
    AppRule, DiscoveryStatus, GlobalConfig, LogEntry, Sensor, SensorType,
    WireCanvasState, WireDevice, WireFanCurve, WireLcdEngineState, WirePresetCurve,
};

#[derive(Default)]
pub struct AppState {
    pub discovery: DiscoveryStatus,
    pub devices: Vec<WireDevice>,
    pub active_profile: String,
    pub profiles: Vec<String>,
    pub fan_curves: Vec<WireFanCurve>,
    pub preset_curves: Vec<WirePresetCurve>,
    pub canvas: WireCanvasState,
    pub lcd_engine: WireLcdEngineState,
    pub global_config: GlobalConfig,
    pub log_entries: Vec<LogEntry>,
    pub config_dir: String,
    pub app_rules: Vec<AppRule>,
    pub focus_watcher_supported: bool,
    /// Monotonically increasing counter incremented on every successful state
    /// update. Used as a cheap "fire on every broadcast" selector for Store
    /// subscribers that do not have a narrower selector.
    pub version: u64,
}

impl AppState {
    pub fn apply_json(&mut self, data: &serde_json::Value) {
        match serde_json::from_value::<halod_protocol::types::AppState>(data.clone()) {
            Ok(s) => {
                self.discovery = s.discovery;
                self.devices = s.devices;
                self.active_profile = s.active_profile;
                self.profiles = s.profiles;
                self.fan_curves = s.fan_curves;
                self.preset_curves = s.preset_curves;
                self.canvas = s.canvas;
                self.lcd_engine = s.lcd_engine;
                self.global_config = s.global_config;
                self.log_entries = s.log_entries;
                self.config_dir = s.config_dir;
                self.app_rules = s.app_rules;
                self.focus_watcher_supported = s.focus_watcher_supported;
                self.version = self.version.wrapping_add(1);
            }
            Err(e) => log::warn!("State parse failed: {e}"),
        }
    }

    /// Returns `(device_name, Sensor)` pairs for every sensor on every device.
    pub fn all_sensors(&self) -> Vec<(String, Sensor)> {
        self.devices.iter().flat_map(|dev| {
            dev.sensors()
                .map(|v| v.as_slice())
                .unwrap_or(&[])
                .iter()
                .filter(|s| s.sensor_type == SensorType::Temperature)
                .map(|s| (dev.name.clone(), s.clone()))
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_protocol::types::{DeviceCapability, DeviceType, Sensor, SensorType, SensorUnit};

    fn temp_sensor(id: &str) -> Sensor {
        Sensor {
            id: id.into(),
            name: id.into(),
            value: 42.0,
            unit: SensorUnit::Celsius,
            sensor_type: SensorType::Temperature,
            visibility: Default::default(),
        }
    }

    fn device_with_sensors(name: &str, sensors: Vec<Sensor>) -> WireDevice {
        WireDevice {
            id: name.into(),
            name: name.into(),
            device_type: DeviceType::Other,
            capabilities: vec![DeviceCapability::Sensors(sensors)],
            ..Default::default()
        }
    }

    // ── apply_json ────────────────────────────────────────────────────────────

    #[test]
    fn apply_json_applies_device_list() {
        let mut state = AppState::default();
        let json = serde_json::json!({
            "devices": [{"id": "d1", "name": "Mouse", "vendor": "", "model": "",
                         "device_type": "mouse", "connected": true, "capabilities": []}]
        });
        state.apply_json(&json);
        assert_eq!(state.devices.len(), 1);
        assert_eq!(state.devices[0].name, "Mouse");
    }

    #[test]
    fn apply_json_applies_active_profile() {
        let mut state = AppState::default();
        state.apply_json(&serde_json::json!({"active_profile": "Gaming"}));
        assert_eq!(state.active_profile, "Gaming");
    }

    #[test]
    fn apply_json_silently_ignores_invalid_json() {
        let mut state = AppState::default();
        state.devices = vec![WireDevice { id: "d1".into(), name: "kept".into(), ..Default::default() }];
        state.apply_json(&serde_json::json!("not an object"));
        // State must be unchanged — no panic, no wipe.
        assert_eq!(state.devices.len(), 1);
        assert_eq!(state.devices[0].name, "kept");
    }

    #[test]
    fn apply_json_empty_object_resets_to_defaults() {
        let mut state = AppState::default();
        state.active_profile = "Old".into();
        state.apply_json(&serde_json::json!({}));
        assert_eq!(state.active_profile, "");
        assert!(state.devices.is_empty());
    }

    // ── all_sensors ───────────────────────────────────────────────────────────

    #[test]
    fn all_sensors_empty_when_no_devices() {
        let state = AppState::default();
        assert!(state.all_sensors().is_empty());
    }

    #[test]
    fn all_sensors_empty_when_device_has_no_sensor_cap() {
        let mut state = AppState::default();
        state.devices = vec![WireDevice {
            id: "d1".into(),
            name: "Keyboard".into(),
            capabilities: vec![],
            ..Default::default()
        }];
        assert!(state.all_sensors().is_empty());
    }

    #[test]
    fn all_sensors_returns_temperature_sensors_from_all_devices() {
        let mut state = AppState::default();
        state.devices = vec![
            device_with_sensors("CPU", vec![temp_sensor("cpu_t")]),
            device_with_sensors("GPU", vec![temp_sensor("gpu_t")]),
        ];
        let sensors = state.all_sensors();
        assert_eq!(sensors.len(), 2);
        assert!(sensors.iter().any(|(name, s)| name == "CPU" && s.id == "cpu_t"));
        assert!(sensors.iter().any(|(name, s)| name == "GPU" && s.id == "gpu_t"));
    }

    #[test]
    fn all_sensors_includes_sensor_name_from_its_device() {
        let mut state = AppState::default();
        state.devices = vec![device_with_sensors("NZXT Kraken", vec![temp_sensor("liquid")])];
        let sensors = state.all_sensors();
        assert_eq!(sensors[0].0, "NZXT Kraken");
        assert_eq!(sensors[0].1.id, "liquid");
    }

    #[test]
    fn apply_json_applies_app_rules() {
        let mut state = AppState::default();
        state.apply_json(&serde_json::json!({
            "app_rules": [
                {"process_names": ["firefox"], "profile": "Web", "enabled": true}
            ]
        }));
        assert_eq!(state.app_rules.len(), 1);
        assert_eq!(state.app_rules[0].profile, "Web");
    }

    #[test]
    fn apply_json_empty_object_clears_app_rules() {
        let mut state = AppState::default();
        state.app_rules.push(halod_protocol::types::AppRule {
            process_names: vec!["x".into()],
            profile: "X".into(),
            enabled: true,
        });
        state.apply_json(&serde_json::json!({}));
        assert!(state.app_rules.is_empty());
    }
}
