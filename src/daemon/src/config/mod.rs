use anyhow::Result;
use halod_protocol::types::{AppRule, VisibilityState, DEFAULT_PROFILE_NAME};
use halod_protocol::zone_transform::ZoneContentTransform;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

pub fn load() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        return Ok(Config::default());
    }
    let raw = std::fs::read_to_string(&path)?;
    let cfg: Config = serde_yaml::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("Failed to parse config at {}: {e}", path.display()))?;
    Ok(cfg)
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("yaml.tmp");
    let yaml = serde_yaml::to_string(cfg)?;
    std::fs::write(&tmp, yaml)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("HALOD_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(appdata).join("halod")
    }
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config").join("halod")
    }
}

fn config_path() -> PathBuf {
    config_dir().join("config.yaml")
}

/// Directory where uploaded LCD images are stored persistently.
/// All uploaded images accumulate here; profiles reference them by filename only.
pub fn lcd_images_dir() -> PathBuf {
    config_dir().join("lcd_images")
}

/// A single fan curve assignment: links a fan device to a temperature sensor and curve points.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanCurveRecord {
    /// Sensor device ID to read temperature from. None = defined but not yet assigned.
    pub sensor_id: Option<String>,
    /// (temp_celsius, duty_percent) control points, must be in ascending temp order.
    pub points: Vec<(f32, f32)>,
}

impl FanCurveRecord {
    /// Lowest temperature a control point may specify, in °C. Sub-ambient is
    /// allowed (chilled loops) but absurd values are clamped.
    pub const MIN_TEMP_C: f32 = -50.0;
    /// Highest temperature a control point may specify, in °C.
    pub const MAX_TEMP_C: f32 = 150.0;

    /// Defensive normalization for the safety-critical fan/pump path.
    ///
    /// `engines::fan_curve::interpolate` assumes control points are sorted by
    /// ascending temperature and within hardware-sane ranges. Points can reach
    /// the engine from sources that bypass the API's `validate_points` check —
    /// most notably a hand-edited or corrupted `config.yaml` restored via
    /// `restore_state`. Out-of-order points there would silently produce wrong
    /// duty values. Calling this at the ingestion boundary
    /// (`FanStateSlot::set_fan_curve`) guarantees the engine only ever sees
    /// well-formed curves:
    ///   - clamp temperature to `MIN_TEMP_C..=MAX_TEMP_C` and duty to `0..=100`
    ///     (NaN is treated as the lower bound),
    ///   - sort points by ascending temperature,
    ///   - drop duplicate temperatures, keeping the first occurrence.
    pub fn sanitize(&mut self) {
        fn clamp_or_low(v: f32, lo: f32, hi: f32) -> f32 {
            if v.is_nan() {
                lo
            } else {
                v.clamp(lo, hi)
            }
        }
        for (temp, duty) in &mut self.points {
            *temp = clamp_or_low(*temp, Self::MIN_TEMP_C, Self::MAX_TEMP_C);
            *duty = clamp_or_low(*duty, 0.0, 100.0);
        }
        // total_cmp gives a total order over the now-finite temps; stable sort
        // keeps the original order among equal temperatures so dedup is predictable.
        self.points.sort_by(|a, b| a.0.total_cmp(&b.0));
        self.points.dedup_by(|a, b| a.0 == b.0);
    }

    pub fn serialize(
        &self,
        fan_id: String,
        status: halod_protocol::types::FanCurveStatus,
    ) -> halod_protocol::types::WireFanCurve {
        halod_protocol::types::WireFanCurve {
            fan_id,
            sensor_id: self.sensor_id.clone(),
            points: self.points.iter().map(|&(t, d)| [t, d]).collect(),
            status,
        }
    }
}

fn default_zone_size() -> f32 {
    0.15
}
fn default_rotation() -> f32 {
    0.0
}
fn default_sample_radius() -> f32 {
    3.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlacedZone {
    pub device_id: String,
    pub zone_id: String,
    pub x: f32,
    pub y: f32,
    #[serde(default = "default_zone_size")]
    pub w: f32,
    #[serde(default = "default_zone_size")]
    pub h: f32,
    #[serde(default = "default_rotation")]
    pub rotation: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanvasState {
    pub active_effect: Option<(
        String,
        HashMap<String, halod_protocol::types::EffectParamValue>,
    )>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placed_zones: Vec<PlacedZone>,
    #[serde(default = "default_sample_radius")]
    pub sample_radius: f32,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            active_effect: None,
            placed_zones: Vec::new(),
            sample_radius: 3.0,
        }
    }
}

/// Metadata recorded the first time a device's state is saved.
/// Persists across disconnects so profiles can reference offline devices.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub name: String,
    pub vendor: String,
    pub model: String,
    #[serde(default)]
    pub active_state: halod_protocol::types::VisibilityState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LcdEngineState {
    /// Maps device_id → template_id for devices currently in LCD engine mode.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub device_templates: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub device_states: HashMap<String, Value>,

    #[serde(default)]
    pub canvas_state: CanvasState,

    #[serde(default)]
    pub lcd_engine_state: LcdEngineState,
}

// TODO: maybe make these consts instead of fn?
fn default_fan_curve_tick_ms() -> u64 {
    2000
}
fn default_canvas_fps() -> u32 {
    20
}
fn default_lcd_fps() -> u32 {
    20
}
fn default_fan_failsafe_duty() -> u8 {
    75
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_close_to_tray() -> bool {
    true
}
fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default = "default_enabled")]
    pub engine_fan_curve_enabled: bool,
    #[serde(default = "default_fan_curve_tick_ms")]
    pub engine_fan_curve_tick_ms: u64,
    #[serde(default = "default_enabled")]
    pub engine_canvas_enabled: bool,
    #[serde(default = "default_canvas_fps")]
    pub engine_canvas_fps: u32,
    #[serde(default = "default_enabled")]
    pub engine_lcd_enabled: bool,
    #[serde(default = "default_lcd_fps")]
    pub engine_lcd_fps: u32,
    #[serde(default = "default_fan_failsafe_duty")]
    pub fan_failsafe_duty: u8,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_close_to_tray")]
    pub close_to_tray: bool,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            engine_fan_curve_enabled: true,
            engine_fan_curve_tick_ms: 2000,
            engine_canvas_enabled: true,
            engine_canvas_fps: 20,
            engine_lcd_enabled: true,
            engine_lcd_fps: 20,
            fan_failsafe_duty: 75,
            log_level: "info".to_string(),
            close_to_tray: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_profile_name")]
    pub active_profile: String,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
    #[serde(default)]
    pub known_devices: HashMap<String, DeviceRecord>,
    #[serde(default)]
    pub global: GlobalConfig,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub device_layouts: HashMap<String, DeviceLayout>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub sensor_visibility: HashMap<String, VisibilityState>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub device_transforms: HashMap<String, HashMap<String, ZoneContentTransform>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub app_rules: Vec<AppRule>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceLayout {
    #[serde(default)]
    pub channels: HashMap<String, ChannelLayoutRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelLayoutRecord {
    /// Only user-added links. Hardware-detected accessories are re-probed each
    /// boot and never persisted here.
    #[serde(default)]
    pub chain_links: Vec<ChainLinkRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainLinkRecord {
    /// Stable across restarts — canvas placements, transforms, and saved RGB
    /// state are keyed by this child id.
    pub id: String,
    pub kind: String,
    pub name: String,
    pub topology: halod_protocol::types::ZoneTopology,
    pub led_count: u32,
}

fn default_profile_name() -> String {
    DEFAULT_PROFILE_NAME.to_string()
}

impl Default for Config {
    fn default() -> Self {
        let mut profiles = HashMap::new();
        profiles.insert(DEFAULT_PROFILE_NAME.to_string(), Profile::default());
        Self {
            active_profile: DEFAULT_PROFILE_NAME.to_string(),
            profiles,
            known_devices: HashMap::new(),
            global: GlobalConfig::default(),
            device_layouts: HashMap::new(),
            sensor_visibility: HashMap::new(),
            device_transforms: HashMap::new(),
            app_rules: Vec::new(),
        }
    }
}

impl Config {
    pub fn active_profile_data(&self) -> &Profile {
        self.profiles
            .get(&self.active_profile)
            .or_else(|| self.profiles.get(DEFAULT_PROFILE_NAME))
            .unwrap_or_else(|| panic!("no default profile"))
    }

    pub fn active_profile_data_mut(&mut self) -> &mut Profile {
        let key = self.active_profile.clone();
        self.profiles.entry(key).or_insert_with(Profile::default)
    }

    pub fn profile_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.profiles.keys().cloned().collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fan_curve_record_serde_round_trip() {
        let record = FanCurveRecord {
            sensor_id: Some("hwmon_pci_temp1".to_string()),
            points: vec![(30.0, 20.0), (60.0, 60.0), (85.0, 100.0)],
        };
        let yaml = serde_yaml::to_string(&record).unwrap();
        let decoded: FanCurveRecord = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(decoded.sensor_id, record.sensor_id);
        assert_eq!(decoded.points.len(), 3);
        assert!((decoded.points[0].0 - 30.0).abs() < 0.001);
        assert!((decoded.points[2].1 - 100.0).abs() < 0.001);
    }

    #[test]
    fn sanitize_sorts_points_by_ascending_temperature() {
        let mut record = FanCurveRecord {
            sensor_id: None,
            points: vec![(80.0, 100.0), (30.0, 20.0), (55.0, 50.0)],
        };
        record.sanitize();
        assert_eq!(record.points, vec![(30.0, 20.0), (55.0, 50.0), (80.0, 100.0)]);
    }

    #[test]
    fn sanitize_clamps_duty_and_temperature_to_sane_ranges() {
        let mut record = FanCurveRecord {
            sensor_id: None,
            points: vec![(-999.0, -10.0), (999.0, 250.0)],
        };
        record.sanitize();
        assert_eq!(
            record.points,
            vec![
                (FanCurveRecord::MIN_TEMP_C, 0.0),
                (FanCurveRecord::MAX_TEMP_C, 100.0),
            ]
        );
    }

    #[test]
    fn sanitize_drops_duplicate_temperatures_keeping_first() {
        let mut record = FanCurveRecord {
            sensor_id: None,
            points: vec![(50.0, 30.0), (50.0, 90.0), (70.0, 80.0)],
        };
        record.sanitize();
        assert_eq!(record.points, vec![(50.0, 30.0), (70.0, 80.0)]);
    }

    #[test]
    fn sanitize_replaces_nan_with_lower_bound() {
        let mut record = FanCurveRecord {
            sensor_id: None,
            points: vec![(f32::NAN, f32::NAN), (40.0, 50.0)],
        };
        record.sanitize();
        assert_eq!(
            record.points,
            vec![(FanCurveRecord::MIN_TEMP_C, 0.0), (40.0, 50.0)]
        );
    }

    #[test]
    fn global_config_close_to_tray_defaults_to_true_when_field_absent() {
        let yaml = "log_level: info";
        let cfg: GlobalConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.close_to_tray);
    }

    #[test]
    fn load_handles_missing_valid_and_malformed_config() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HALOD_CONFIG_DIR", dir.path());
        let path = dir.path().join("config.yaml");

        // Missing file -> defaults.
        let cfg = load().unwrap();
        assert_eq!(cfg.active_profile, DEFAULT_PROFILE_NAME);

        // Valid file -> parsed values.
        std::fs::write(&path, "active_profile: gaming\n").unwrap();
        let cfg = load().unwrap();
        assert_eq!(cfg.active_profile, "gaming");

        // Malformed YAML -> error, not silent defaults.
        std::fs::write(&path, "active_profile: [unterminated\n").unwrap();
        let err = load().unwrap_err();
        assert!(err.to_string().contains("Failed to parse config"));

        std::env::remove_var("HALOD_CONFIG_DIR");
    }
}
