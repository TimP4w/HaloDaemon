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
    /// Max duty rise in %/s; 0 = instant.
    pub fan_ramp_up_pct_per_s: f32,
    /// Max duty fall in %/s; 0 = instant.
    pub fan_ramp_down_pct_per_s: f32,
    /// A falling temperature is ignored until it drops this far; 0 = off.
    pub fan_hysteresis_c: f32,
    /// Moving-average window over the sensor; 0 = off.
    pub fan_smoothing_secs: f32,
    /// A sensor unchanged for this long is treated as broken; 0 = off.
    pub fan_sensor_stale_secs: f32,
}

impl Default for CoolingConfig {
    fn default() -> Self {
        Self {
            fan_curve_enabled: true,
            fan_curve_tick_ms: 2000,
            fan_failsafe_duty: 75,
            fan_ramp_up_pct_per_s: 0.0,
            fan_ramp_down_pct_per_s: 5.0,
            fan_hysteresis_c: 3.0,
            fan_smoothing_secs: 0.0,
            fan_sensor_stale_secs: 0.0,
        }
    }
}

impl CoolingConfig {
    pub const MAX_RAMP_PCT_PER_S: f32 = 100.0;
    pub const MAX_HYSTERESIS_C: f32 = 20.0;
    pub const MAX_SMOOTHING_SECS: f32 = 60.0;
    pub const MAX_SENSOR_STALE_SECS: f32 = 600.0;

    pub fn sanitize(&mut self) {
        fn clamp(v: f32, max: f32, fallback: f32) -> f32 {
            if v.is_finite() {
                v.clamp(0.0, max)
            } else {
                fallback
            }
        }
        let default = Self::default();
        self.fan_curve_tick_ms = self.fan_curve_tick_ms.clamp(500, 60_000);
        self.fan_failsafe_duty = self.fan_failsafe_duty.min(100);
        self.fan_ramp_up_pct_per_s = clamp(
            self.fan_ramp_up_pct_per_s,
            Self::MAX_RAMP_PCT_PER_S,
            default.fan_ramp_up_pct_per_s,
        );
        self.fan_ramp_down_pct_per_s = clamp(
            self.fan_ramp_down_pct_per_s,
            Self::MAX_RAMP_PCT_PER_S,
            default.fan_ramp_down_pct_per_s,
        );
        self.fan_hysteresis_c = clamp(
            self.fan_hysteresis_c,
            Self::MAX_HYSTERESIS_C,
            default.fan_hysteresis_c,
        );
        self.fan_smoothing_secs = clamp(
            self.fan_smoothing_secs,
            Self::MAX_SMOOTHING_SECS,
            default.fan_smoothing_secs,
        );
        self.fan_sensor_stale_secs = clamp(
            self.fan_sensor_stale_secs,
            Self::MAX_SENSOR_STALE_SECS,
            default.fan_sensor_stale_secs,
        );
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

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
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
    pub device_id: crate::ids::DeviceId,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_without_tuning_keys_gets_defaults() {
        let cfg: CoolingConfig = serde_json::from_str(r#"{"fan_failsafe_duty": 60}"#).unwrap();
        assert_eq!(cfg.fan_failsafe_duty, 60);
        assert_eq!(
            cfg.fan_hysteresis_c,
            CoolingConfig::default().fan_hysteresis_c
        );
    }

    proptest::proptest! {
        #[test]
        fn sanitize_bounds_every_field(
            tick in proptest::num::u64::ANY,
            failsafe in proptest::num::u8::ANY,
            up in proptest::num::f32::ANY,
            down in proptest::num::f32::ANY,
            hysteresis in proptest::num::f32::ANY,
            smoothing in proptest::num::f32::ANY,
            stale in proptest::num::f32::ANY,
        ) {
            let mut cfg = CoolingConfig {
                fan_curve_enabled: true,
                fan_curve_tick_ms: tick,
                fan_failsafe_duty: failsafe,
                fan_ramp_up_pct_per_s: up,
                fan_ramp_down_pct_per_s: down,
                fan_hysteresis_c: hysteresis,
                fan_smoothing_secs: smoothing,
                fan_sensor_stale_secs: stale,
            };
            cfg.sanitize();
            proptest::prop_assert!((500..=60_000).contains(&cfg.fan_curve_tick_ms));
            proptest::prop_assert!(cfg.fan_failsafe_duty <= 100);
            for (v, max) in [
                (cfg.fan_ramp_up_pct_per_s, CoolingConfig::MAX_RAMP_PCT_PER_S),
                (cfg.fan_ramp_down_pct_per_s, CoolingConfig::MAX_RAMP_PCT_PER_S),
                (cfg.fan_hysteresis_c, CoolingConfig::MAX_HYSTERESIS_C),
                (cfg.fan_smoothing_secs, CoolingConfig::MAX_SMOOTHING_SECS),
                (cfg.fan_sensor_stale_secs, CoolingConfig::MAX_SENSOR_STALE_SECS),
            ] {
                proptest::prop_assert!((0.0..=max).contains(&v), "{v} outside 0..={max}");
            }
        }
    }
}
