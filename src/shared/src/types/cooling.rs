// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;

/// Persisted under `config.yaml`'s `cooling` key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CoolingConfig {
    pub fan_curve_enabled: bool,
    /// Milliseconds between fan-curve ticks.
    pub fan_curve_tick_ms: u64,
    /// Duty (0-100) applied when a fan's assigned sensor is absent.
    pub fan_failsafe_duty: u8,
}

impl Default for CoolingConfig {
    fn default() -> Self {
        Self {
            fan_curve_enabled: true,
            fan_curve_tick_ms: 2000,
            fan_failsafe_duty: 75,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoolingState {
    #[serde(default)]
    pub fan_curves: Vec<WireFanCurve>,
    #[serde(default)]
    pub preset_curves: Vec<WirePresetCurve>,
    #[serde(default)]
    pub config: CoolingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CoolingChannelKind {
    #[default]
    Fan,
    Pump,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CoolingChannel {
    /// Stable and device-local; keys persisted curves.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: CoolingChannelKind,
    #[serde(default)]
    pub controllable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty: Option<u8>,
    /// `Hidden` drops the channel from the UI; `Disabled` also stops the fan curve driving it.
    #[serde(default)]
    pub visibility: VisibilityState,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CoolingStatus {
    #[serde(default)]
    pub channels: Vec<CoolingChannel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FanCurveStatus {
    #[default]
    Ok,
    /// sensor_id is None, or the sensor device could not be found.
    NoSensor,
    /// Sensor value has not changed for longer than the stale threshold.
    SensorMalfunction,
    /// e.g. permission denied on the sysfs pwm file.
    WriteError(String),
    /// Fan RPM has been 0 while curve duty is >20% for more than 10 s.
    FanStalled,
    /// Saved curve but device absent; no write was attempted.
    NoDevice,
    /// Probing at full duty produced no RPM; inert until any RPM > 0.
    Disconnected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireFanCurve {
    #[serde(default)]
    pub device_id: String,
    #[serde(default = "default_cooling_channel_id")]
    pub channel_id: String,
    pub sensor_id: Option<String>,
    /// [temp_celsius, duty_percent].
    pub points: Vec<[f32; 2]>,
    pub status: FanCurveStatus,
}

fn default_cooling_channel_id() -> String {
    "default".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WirePresetCurve {
    pub id: String,
    pub name: String,
    pub points: Vec<[f32; 2]>,
}
