// SPDX-License-Identifier: GPL-3.0-or-later
//! Fan curve engine: a control loop that, per assigned fan, interpolates a
//! target duty from the curve against its sensor and writes it via
//! `CoolingCapability::set_cooling_duty`. Each channel's `FanCurveStatus` is published to
//! the cooling engine's status cache for retained bus topic production. Any condition
//! preventing safe closed-loop control drives the fan to the failsafe duty.

use std::collections::HashMap;
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::cooling::model::FanCurveRecord;
use crate::domain::cooling::state::curve_key;
use halod_shared::types::FanCurveStatus;

pub struct PresetCurve {
    pub id: &'static str,
    pub name: &'static str,
    pub points: &'static [(f32, f32)],
}

impl PresetCurve {
    pub fn serialize(&self) -> halod_shared::types::WirePresetCurve {
        halod_shared::types::WirePresetCurve {
            id: self.id.to_string(),
            name: self.name.to_string(),
            points: self.points.iter().map(|&(t, d)| [t, d]).collect(),
        }
    }
}

const PRESETS: &[PresetCurve] = &[
    PresetCurve {
        id: "balanced",
        name: "Balanced",
        points: &[
            (20.0, 25.0),
            (40.0, 30.0),
            (55.0, 50.0),
            (70.0, 80.0),
            (80.0, 100.0),
        ],
    },
    PresetCurve {
        id: "silent",
        name: "Silent",
        points: &[
            (20.0, 20.0),
            (45.0, 25.0),
            (60.0, 40.0),
            (75.0, 70.0),
            (85.0, 100.0),
        ],
    },
    PresetCurve {
        id: "performance",
        name: "Performance",
        points: &[
            (20.0, 40.0),
            (40.0, 55.0),
            (55.0, 75.0),
            (65.0, 90.0),
            (75.0, 100.0),
        ],
    },
    PresetCurve {
        id: "full_speed",
        name: "Full Speed",
        points: &[(0.0, 100.0), (100.0, 100.0)],
    },
    PresetCurve {
        id: "fifty_percent",
        name: "50%",
        points: &[(0.0, 50.0), (100.0, 50.0)],
    },
];

pub fn preset_curves() -> &'static [PresetCurve] {
    PRESETS
}

const HYSTERESIS_C: f32 = 3.0;

/// Hysteresis filter: rising tracks immediately, falling requires >3°C drop.
fn hysteresis_temp(last: Option<f32>, current: f32) -> f32 {
    match last {
        Some(prev) if current < prev && current > prev - HYSTERESIS_C => prev,
        _ => current,
    }
}

fn default_curve() -> FanCurveRecord {
    let points = PRESETS
        .iter()
        .find(|p| p.id == "balanced")
        .map(|p| p.points.to_vec())
        .unwrap_or_default();
    FanCurveRecord {
        sensor_id: None,
        points,
    }
}

struct StallState {
    since: std::time::Instant,
    notified: bool,
}

#[derive(Default)]
struct PerFanState {
    missing_device_ticks: u32,
    stall: Option<StallState>,
    control_temp: Option<f32>,
    failsafe_error_logged: bool,
}

pub struct FanCurveEngine {
    app_state: Arc<AppState>,
    per_fan: std::sync::Mutex<HashMap<String, PerFanState>>,
}

#[derive(Clone)]
struct CurveTarget {
    device_id: String,
    channel_id: String,
}

fn suppress_initial_no_sensor(
    previous: &HashMap<String, FanCurveStatus>,
    current: &mut HashMap<String, FanCurveStatus>,
) {
    for (key, status) in current {
        if matches!(status, FanCurveStatus::NoSensor) && !previous.contains_key(key) {
            *status = FanCurveStatus::Ok;
        }
    }
}

fn log_status_changes(
    previous: &HashMap<String, FanCurveStatus>,
    current: &HashMap<String, FanCurveStatus>,
    curves: &HashMap<String, (CurveTarget, FanCurveRecord)>,
) {
    for (key, status) in current {
        let old = previous.get(key);
        if old == Some(status) {
            continue;
        }
        let Some((target, record)) = curves.get(key) else {
            continue;
        };
        let sensor = record.sensor_id.as_deref().unwrap_or("none");
        if matches!(status, FanCurveStatus::Ok) {
            if let Some(old) = old {
                log::info!(
                    "[FanCurve] Cooling recovered for {}:{} from {old:?} (sensor: {sensor})",
                    target.device_id,
                    target.channel_id
                );
            }
        } else {
            log::warn!(
                "[FanCurve] Cooling warning for {}:{}: {status:?} (previous: {old:?}, sensor: {sensor})",
                target.device_id,
                target.channel_id
            );
        }
    }
}

// Non-async lock helpers — MutexGuard must not cross .await.
impl FanCurveEngine {
    fn lock_mutex<T>(mu: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mu.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn update_control_temp(&self, fan_id: &str, sensor_value: f32) -> f32 {
        let mut map = Self::lock_mutex(&self.per_fan);
        let state = map.entry(fan_id.to_string()).or_default();
        let eff = hysteresis_temp(state.control_temp, sensor_value);
        state.control_temp = Some(eff);
        eff
    }

    fn clear_missing_device(&self, fan_id: &str) {
        Self::lock_mutex(&self.per_fan)
            .entry(fan_id.to_string())
            .or_default()
            .missing_device_ticks = 0;
    }

    fn record_missing_device(&self, fan_id: &str) {
        let mut map = Self::lock_mutex(&self.per_fan);
        let state = map.entry(fan_id.to_string()).or_default();
        state.missing_device_ticks += 1;
        if should_log_missing_device(state.missing_device_ticks) {
            log::debug!("[FanCurve] Fan/pump device not found or not controllable: {fan_id}");
        }
    }

    /// Record a stall tick; returns `(should_notify, elapsed_secs)`.
    fn record_stall(&self, fan_id: &str) -> (bool, u64) {
        const STALL_SECS: u64 = 10;
        let mut map = Self::lock_mutex(&self.per_fan);
        let entry = map
            .entry(fan_id.to_string())
            .or_default()
            .stall
            .get_or_insert_with(|| StallState {
                since: std::time::Instant::now(),
                notified: false,
            });
        let elapsed = entry.since.elapsed().as_secs();
        let should_notify = elapsed >= STALL_SECS && !entry.notified;
        if should_notify {
            entry.notified = true;
        }
        (should_notify, elapsed)
    }

    fn clear_stall(&self, fan_id: &str) {
        if let Some(state) = Self::lock_mutex(&self.per_fan).get_mut(fan_id) {
            state.stall = None;
        }
    }

    fn clear_failsafe_error(&self, fan_id: &str) {
        if let Some(state) = Self::lock_mutex(&self.per_fan).get_mut(fan_id) {
            state.failsafe_error_logged = false;
        }
    }

    /// Returns `true` if this is the first error of the current episode
    /// (caller should log the warning).
    fn mark_failsafe_error(&self, fan_id: &str) -> bool {
        let mut map = Self::lock_mutex(&self.per_fan);
        let state = map.entry(fan_id.to_string()).or_default();
        let first = !state.failsafe_error_logged;
        state.failsafe_error_logged = true;
        first
    }

    #[cfg(test)]
    fn seed_stall(&self, fan_id: &str, since: std::time::Instant, notified: bool) {
        Self::lock_mutex(&self.per_fan)
            .entry(fan_id.to_string())
            .or_default()
            .stall = Some(StallState { since, notified });
    }

    #[cfg(test)]
    fn missing_ticks(&self, fan_id: &str) -> u32 {
        Self::lock_mutex(&self.per_fan)
            .get(fan_id)
            .map_or(0, |state| state.missing_device_ticks)
    }
}

/// Log the "device not found" warning only on the first miss of an episode,
/// since devices reconnect transiently and per-tick logging would flood.
fn should_log_missing_device(consecutive_misses: u32) -> bool {
    consecutive_misses == 1
}

impl FanCurveEngine {
    pub fn new(app_state: Arc<AppState>) -> Arc<Self> {
        Arc::new(Self {
            app_state,
            per_fan: std::sync::Mutex::new(HashMap::new()),
        })
    }

    pub fn start(
        self: Arc<Self>,
        cfg_rx: crate::application::run_loop::EngineConfigReceiver,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            crate::application::run_loop::engine_run_loop_idle(
                "FanCurve",
                cfg_rx,
                tokio::time::MissedTickBehavior::Skip,
                |_cfg| {
                    let this = Arc::clone(&self);
                    let duty = _cfg.failsafe_duty.unwrap_or(100);
                    async move { this.tick(duty).await }
                },
                std::future::pending::<()>,
                || std::future::ready(true),
            )
            .await;
        })
    }

    async fn tick(&self, failsafe_duty: u8) {
        let curves: HashMap<String, (CurveTarget, FanCurveRecord)> = {
            let devices = self.app_state.get_active_devices().await;
            let cfg = self.app_state.config.read().await;
            let mut map = HashMap::new();
            for device in devices {
                if let Some(cooling) = device.as_cooling() {
                    for channel in cooling.cooling_channels() {
                        if !channel.controllable
                            || !cfg.channel_enabled(
                                device.id(),
                                halod_shared::types::ChannelKind::Cooling,
                                &channel.id,
                            )
                        {
                            continue;
                        }
                        let curve = cooling.curve(&channel.id).unwrap_or_else(|| {
                            let default = default_curve();
                            cooling.set_curve(channel.id.clone(), default.clone());
                            self.app_state.config.persistence().notify.notify_one();
                            default
                        });
                        let target = CurveTarget {
                            device_id: device.id().to_owned(),
                            channel_id: channel.id,
                        };
                        map.insert(
                            curve_key(&target.device_id, &target.channel_id),
                            (target, curve),
                        );
                    }
                }
            }
            map
        };

        let sensors = self.app_state.data_bus.sensors();
        let mut new_statuses = HashMap::with_capacity(curves.len());
        for (key, (target, record)) in &curves {
            let status = self
                .process_curve(key, target, record, &sensors, failsafe_duty)
                .await;
            new_statuses.insert(key.clone(), status);
        }

        let changed = {
            let mut statuses = self.app_state.cooling.statuses.lock().await;
            suppress_initial_no_sensor(&statuses, &mut new_statuses);
            if *statuses == new_statuses {
                false
            } else {
                log_status_changes(&statuses, &new_statuses, &curves);
                *statuses = new_statuses;
                true
            }
        };
        if changed {
            crate::application::usecases::cooling::runtime::status_changed(&self.app_state).await;
        }
    }

    async fn process_curve(
        &self,
        key: &str,
        curve_target: &CurveTarget,
        record: &FanCurveRecord,
        sensors: &HashMap<String, halod_shared::types::Sensor>,
        failsafe_duty: u8,
    ) -> FanCurveStatus {
        if let Err(e) = record.validate() {
            log::warn!(
                "[FanCurve] Invalid curve for {}:{}: {e:#}",
                curve_target.device_id,
                curve_target.channel_id
            );
            self.apply_failsafe(key, curve_target, failsafe_duty).await;
            return FanCurveStatus::WriteError(format!("invalid fan curve configuration: {e}"));
        }
        let constant_duty = record.points.first().map(|point| point.1).filter(|first| {
            record
                .points
                .iter()
                .all(|point| (point.1 - *first).abs() <= f32::EPSILON)
        });
        let target = if let Some(duty) = constant_duty {
            duty
        } else {
            let Some(sensor_id) = &record.sensor_id else {
                self.apply_failsafe(key, curve_target, failsafe_duty).await;
                return FanCurveStatus::NoSensor;
            };
            let Some(sensor) = sensors.get(sensor_id) else {
                log::debug!("[FanCurve] Sensor not found: {sensor_id}");
                self.apply_failsafe(key, curve_target, failsafe_duty).await;
                return FanCurveStatus::NoSensor;
            };
            if sensor.sensor_type != halod_shared::types::SensorType::Temperature {
                log::warn!("[FanCurve] Sensor {sensor_id} is not a temperature sensor");
                self.apply_failsafe(key, curve_target, failsafe_duty).await;
                return FanCurveStatus::NoSensor;
            }
            if !sensor.value.is_finite() {
                log::warn!("[FanCurve] Sensor {sensor_id} returned a non-finite temperature");
                self.apply_failsafe(key, curve_target, failsafe_duty).await;
                return FanCurveStatus::NoSensor;
            }
            let temp = self.update_control_temp(key, sensor.value as f32);
            interpolate(&record.points, temp)
        };
        let fan_device = self
            .app_state
            .find_device_by_id(&curve_target.device_id)
            .await;
        if let Some(device) = &fan_device {
            // Offline but present — treat as missing so we stop writing to a dead socket.
            if !device.is_live() {
                self.record_missing_device(key);
                return FanCurveStatus::NoDevice;
            }
            self.clear_missing_device(key);
            let current = current_duty(device, &curve_target.channel_id).await;
            if (current - target).abs() > 1.0 {
                let duty = target.round().clamp(0.0, 100.0) as u8;
                if let Err(e) = apply_duty(device, &curve_target.channel_id, duty).await {
                    log::warn!(
                        "[FanCurve] Failed to set duty for {}:{}: {e}",
                        curve_target.device_id,
                        curve_target.channel_id
                    );
                    return FanCurveStatus::WriteError(e.to_string());
                }
                crate::application::usecases::cooling::runtime::duty_applied(
                    &self.app_state,
                    &curve_target.device_id,
                    &curve_target.channel_id,
                    duty,
                )
                .await;
            }
            return self
                .check_stall_channel(key, device, &curve_target.channel_id, target)
                .await;
        } else {
            self.record_missing_device(key);
        }

        FanCurveStatus::NoDevice
    }

    // Single-output shim over `process_curve`, used only by tests.
    #[cfg(test)]
    async fn process_fan(
        &self,
        fan_id: &str,
        record: &FanCurveRecord,
        sensors: &HashMap<String, halod_shared::types::Sensor>,
        failsafe_duty: u8,
    ) -> FanCurveStatus {
        self.process_curve(
            &curve_key(fan_id, "default"),
            &CurveTarget {
                device_id: fan_id.to_string(),
                channel_id: "default".to_string(),
            },
            record,
            sensors,
            failsafe_duty,
        )
        .await
    }

    /// Stalled (0 RPM at >20% target duty) for more than 10s fires a one-shot
    /// warning and returns `FanStalled`; pumps (no RPM) always return `Ok`.
    async fn check_stall_channel(
        &self,
        key: &str,
        device: &Arc<dyn crate::domain::device::Device>,
        channel_id: &str,
        target: f32,
    ) -> FanCurveStatus {
        const STALL_SECS: u64 = 10;
        const DUTY_THRESHOLD: f32 = 20.0;

        let rpm = match current_rpm(device, channel_id).await {
            Some(r) => r,
            None => return FanCurveStatus::Ok,
        };

        let stalled = rpm == 0 && target > DUTY_THRESHOLD;

        if stalled {
            let (should_notify, elapsed) = self.record_stall(key);

            if should_notify {
                let fan_name = device
                    .as_cooling()
                    .and_then(|cooling| {
                        cooling
                            .cooling_channels()
                            .into_iter()
                            .find(|channel| channel.id == channel_id)
                            .map(|channel| channel.name)
                    })
                    .unwrap_or_else(|| device.name().to_owned());
                crate::application::notifications::send(
                    &self.app_state,
                    halod_shared::types::NotificationCode::FanStalled { fan: fan_name },
                )
                .await;
            }

            if elapsed >= STALL_SECS {
                FanCurveStatus::FanStalled
            } else {
                FanCurveStatus::Ok
            }
        } else {
            self.clear_stall(key);
            FanCurveStatus::Ok
        }
    }

    // Single-output shim over `check_stall_channel`, used only by tests.
    #[cfg(test)]
    async fn check_stall(
        &self,
        fan_id: &str,
        device: &Arc<dyn crate::domain::device::Device>,
        target: f32,
    ) -> FanCurveStatus {
        self.check_stall_channel(fan_id, device, "default", target)
            .await
    }

    async fn apply_failsafe(&self, key: &str, target: &CurveTarget, duty: u8) {
        let fan_device = self.app_state.find_device_by_id(&target.device_id).await;
        if let Some(device) = &fan_device {
            if !device.is_live() {
                self.record_missing_device(key);
                return;
            }
            match apply_duty(device, &target.channel_id, duty).await {
                Ok(()) => {
                    self.clear_failsafe_error(key);
                    crate::application::usecases::cooling::runtime::duty_applied(
                        &self.app_state,
                        &target.device_id,
                        &target.channel_id,
                        duty,
                    )
                    .await;
                }
                Err(e) => {
                    if self.mark_failsafe_error(key) {
                        log::warn!(
                            "[FanCurve] Failsafe set_duty failed for {}:{}: {e}",
                            target.device_id,
                            target.channel_id
                        );
                    }
                }
            }
        }
    }
}

async fn current_duty(device: &Arc<dyn crate::domain::device::Device>, channel_id: &str) -> f32 {
    if let Some(cooling) = device.as_cooling() {
        cooling
            .get_cooling_status(channel_id)
            .await
            .ok()
            .and_then(|s| s.duty)
            .unwrap_or(0) as f32
    } else {
        0.0
    }
}

async fn current_rpm(
    device: &Arc<dyn crate::domain::device::Device>,
    channel_id: &str,
) -> Option<u32> {
    if let Some(cooling) = device.as_cooling() {
        return cooling
            .get_cooling_status(channel_id)
            .await
            .ok()
            .and_then(|s| s.rpm);
    }
    None
}

async fn apply_duty(
    device: &Arc<dyn crate::domain::device::Device>,
    channel_id: &str,
    duty: u8,
) -> anyhow::Result<()> {
    if let Some(cooling) = device.as_cooling() {
        cooling.set_cooling_duty(channel_id, duty).await
    } else {
        Err(anyhow::anyhow!("device has no duty capability"))
    }
}

fn interpolate(points: &[(f32, f32)], temp: f32) -> f32 {
    debug_assert!(
        points.windows(2).all(|w| w[0].0 <= w[1].0),
        "interpolate requires ascending temperatures, got {points:?}"
    );
    halod_shared::curve::duty_at_tuples(points, temp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::domain::cooling::model::FanCurveRecord;
    use crate::domain::device::{
        CapabilityRef, CoolingCapability, CoolingStateSlot, Device, SensorCapability,
    };
    use async_trait::async_trait;
    use halod_shared::types::{CoolingChannel, CoolingChannelKind, Sensor, SensorUnit};
    use std::sync::Mutex as StdMutex;

    #[test]
    fn missing_device_logs_only_on_first_miss() {
        assert!(should_log_missing_device(1), "first miss logs");
        assert!(!should_log_missing_device(2), "second miss is silent");
        assert!(!should_log_missing_device(100), "later misses stay silent");
    }

    #[test]
    fn initial_no_sensor_is_suppressed_for_one_cycle() {
        let previous = HashMap::new();
        let mut current = HashMap::from([("pump".to_owned(), FanCurveStatus::NoSensor)]);

        suppress_initial_no_sensor(&previous, &mut current);

        assert_eq!(current["pump"], FanCurveStatus::Ok);
    }

    #[test]
    fn persistent_no_sensor_is_reported() {
        let previous = HashMap::from([("pump".to_owned(), FanCurveStatus::Ok)]);
        let mut current = HashMap::from([("pump".to_owned(), FanCurveStatus::NoSensor)]);

        suppress_initial_no_sensor(&previous, &mut current);

        assert_eq!(current["pump"], FanCurveStatus::NoSensor);
    }

    #[test]
    fn hysteresis_first_reading_uses_current_temp() {
        assert_eq!(hysteresis_temp(None, 50.0), 50.0);
    }

    #[test]
    fn hysteresis_rising_temp_tracks_immediately() {
        assert_eq!(hysteresis_temp(Some(50.0), 55.0), 55.0);
    }

    #[test]
    fn hysteresis_small_fall_holds_previous_temp() {
        assert_eq!(hysteresis_temp(Some(50.0), 48.0), 50.0);
    }

    #[test]
    fn hysteresis_large_fall_tracks_new_temp() {
        assert_eq!(hysteresis_temp(Some(50.0), 46.0), 46.0);
    }

    #[test]
    fn hysteresis_at_band_edge_tracks_new_temp() {
        // Exactly HYSTERESIS_C below is *outside* the band (`>` is strict), so 47 °C
        // is tracked, not held at 50. Pins the `>` boundary against `>=`.
        assert_eq!(hysteresis_temp(Some(50.0), 50.0 - HYSTERESIS_C), 47.0);
    }

    #[test]
    fn default_curve_matches_balanced_preset() {
        let balanced = preset_curves()
            .iter()
            .find(|p| p.id == "balanced")
            .expect("balanced preset exists");
        let expected: Vec<(f32, f32)> = balanced.points.to_vec();
        assert_eq!(
            default_curve().points,
            expected,
            "default curve must be the Balanced preset"
        );
    }

    #[test]
    fn interpolate_within_range() {
        let points = vec![(30.0, 20.0), (60.0, 60.0), (85.0, 100.0)];
        // 45 is halfway between 30..60 → duty = 20 + (60-20)*0.5 = 40
        let result = interpolate(&points, 45.0);
        assert!((result - 40.0).abs() < 0.01, "got {result}");
    }

    #[test]
    fn interpolate_below_range_clamps_to_first_point() {
        let points = vec![(30.0, 20.0), (60.0, 60.0)];
        assert!((interpolate(&points, 10.0) - 20.0).abs() < 0.01);
    }

    #[test]
    fn interpolate_above_range_clamps_to_last_point() {
        let points = vec![(30.0, 20.0), (60.0, 60.0)];
        assert!((interpolate(&points, 90.0) - 60.0).abs() < 0.01);
    }

    #[test]
    fn interpolate_single_point_always_returns_that_duty() {
        let points = vec![(50.0, 75.0)];
        for temp in [0.0f32, 50.0, 100.0] {
            assert!((interpolate(&points, temp) - 75.0).abs() < 0.01);
        }
    }

    #[test]
    fn interpolate_empty_returns_zero() {
        assert_eq!(interpolate(&[], 50.0), 0.0);
    }

    #[test]
    fn interpolate_duplicate_adjacent_temps_avoids_nan() {
        // Zero span between the two 60.0 points must not divide by zero.
        let points = vec![(30.0, 20.0), (60.0, 60.0), (60.0, 90.0), (85.0, 100.0)];
        let result = interpolate(&points, 60.0);
        assert!(result.is_finite(), "got {result}");
    }

    // A curve restored from a hand-edited / corrupted config bypasses the API's
    // validate_points. The slot setter must normalize it so the engine never
    // interpolates over unsorted points.
    #[test]
    fn set_fan_curve_normalizes_unsorted_points() {
        let slot = CoolingStateSlot::default();
        slot.set_curve(
            "default".to_string(),
            FanCurveRecord {
                sensor_id: None,
                points: vec![(80.0, 100.0), (30.0, 20.0), (55.0, 50.0)],
            },
        );
        let points = slot.curve("default").unwrap().points;
        assert!(
            points.windows(2).all(|w| w[0].0 <= w[1].0),
            "stored points must be ascending, got {points:?}"
        );
        // interpolate now agrees with the intended sorted curve:
        // 45 °C lies in (30,20)→(55,50): 20 + 30*(15/25) = 38%.
        assert!((interpolate(&points, 45.0) - 38.0).abs() < 0.01);
    }

    struct MockFan {
        id: &'static str,
        duty: StdMutex<u8>,
        rpm: StdMutex<u32>,
        cooling: CoolingStateSlot,
        live: std::sync::atomic::AtomicBool,
    }

    impl MockFan {
        fn new(id: &'static str) -> Arc<Self> {
            Arc::new(Self {
                id,
                duty: StdMutex::new(20),
                rpm: StdMutex::new(1000),
                cooling: CoolingStateSlot::default(),
                live: std::sync::atomic::AtomicBool::new(true),
            })
        }

        /// A fan whose backing device is offline (`is_live() == false`), e.g. an
        /// integration whose server dropped.
        fn new_offline_with_curve(id: &'static str, record: FanCurveRecord) -> Arc<Self> {
            let this = Self::new_with_curve(id, record);
            this.live.store(false, std::sync::atomic::Ordering::SeqCst);
            this
        }

        fn new_with_curve(id: &'static str, record: FanCurveRecord) -> Arc<Self> {
            let this = Self::new(id);
            this.cooling.set_curve("default".to_string(), record);
            this
        }

        fn new_stalled(id: &'static str, record: FanCurveRecord) -> Arc<Self> {
            let this = Self::new_with_curve(id, record);
            *this.rpm.lock().unwrap() = 0;
            this
        }

        fn last_duty(&self) -> u8 {
            *self.duty.lock().unwrap()
        }
    }

    #[async_trait]
    impl Device for MockFan {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.id
        }
        fn vendor(&self) -> &str {
            "test"
        }
        fn model(&self) -> &str {
            "test"
        }
        async fn initialize(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn is_live(&self) -> bool {
            self.live.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::Cooling(self)]
        }
    }

    #[async_trait]
    impl CoolingCapability for MockFan {
        fn cooling_channels(&self) -> Vec<CoolingChannel> {
            vec![self.channel()]
        }
        async fn get_cooling_status(&self, _channel_id: &str) -> anyhow::Result<CoolingChannel> {
            Ok(self.channel())
        }
        async fn set_cooling_duty(&self, _channel_id: &str, duty: u8) -> anyhow::Result<()> {
            *self.duty.lock().unwrap() = duty;
            Ok(())
        }
        fn cooling_state(&self) -> &CoolingStateSlot {
            &self.cooling
        }
    }

    impl MockFan {
        fn channel(&self) -> CoolingChannel {
            CoolingChannel {
                id: "default".into(),
                name: "Fan".into(),
                kind: CoolingChannelKind::Fan,
                controllable: true,
                rpm: Some(*self.rpm.lock().unwrap()),
                duty: Some(*self.duty.lock().unwrap()),
                visibility: Default::default(),
            }
        }
    }

    struct MockSensor {
        sensor_id: &'static str,
        temp: f64,
        id: String,
    }

    impl MockSensor {
        fn new(sensor_id: &'static str, temp: f64) -> Arc<Self> {
            Arc::new(Self {
                sensor_id,
                temp,
                id: format!("device_{}", sensor_id),
            })
        }
    }

    #[async_trait]
    impl Device for MockSensor {
        fn id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            self.sensor_id
        }
        fn vendor(&self) -> &str {
            "test"
        }
        fn model(&self) -> &str {
            "test"
        }
        async fn initialize(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::Sensor(self)]
        }
    }

    #[async_trait]
    impl SensorCapability for MockSensor {
        async fn get_sensors(&self) -> anyhow::Result<Vec<Sensor>> {
            Ok(vec![Sensor {
                id: self.sensor_id.to_string(),
                name: self.sensor_id.to_string(),
                value: self.temp,
                unit: SensorUnit::Celsius,
                sensor_type: halod_shared::types::SensorType::Temperature,
            }])
        }
    }

    /// Build an AppState with a fan pre-loaded with `record` in its FanEngineSlot.
    fn make_app(fan: Arc<MockFan>, sensor: Option<Arc<MockSensor>>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        let mut devices: Vec<Arc<dyn Device>> = vec![fan as Arc<dyn Device>];
        if let Some(s) = sensor {
            devices.push(s as Arc<dyn Device>);
        }
        *app.device_registry.try_write().unwrap() = devices;
        app
    }

    #[tokio::test]
    async fn sensor_bus_indexes_all_sensors_by_id() {
        let fan = MockFan::new("fan_0");
        let sensor = MockSensor::new("sensor_0", 42.0);
        let app = make_app(fan, Some(sensor));
        crate::application::usecases::device::telemetry::observe(&app).await;
        let sensors = app.data_bus.sensors();
        assert_eq!(sensors.get("sensor_0").map(|s| s.value), Some(42.0));
    }

    #[tokio::test]
    async fn tick_calls_set_duty_when_temp_triggers_change() {
        // At temp=70, interpolated between (60,60)→(90,100): 60 + 40*(10/30) ≈ 73%
        // current_duty=20, |20-73| > 1.0 → set_duty fires
        let record = FanCurveRecord {
            sensor_id: Some("sensor_0".to_string()),
            points: vec![(30.0, 20.0), (60.0, 60.0), (90.0, 100.0)],
        };
        let fan = MockFan::new_with_curve("fan_0", record);
        let sensor = MockSensor::new("sensor_0", 70.0);
        let app = make_app(fan.clone(), Some(sensor));
        crate::application::usecases::device::telemetry::observe(&app).await;
        let engine = FanCurveEngine::new(app);
        engine.tick(75).await;
        assert_ne!(fan.last_duty(), 20, "duty should have been updated from 20");
    }

    #[tokio::test]
    async fn tick_does_not_write_when_delta_is_exactly_one() {
        // Duty 20, flat curve at 21 → delta exactly 1.0. The guard is strict
        // (`abs() > 1.0`), so no write. Pins the `>` boundary and the subtraction.
        let record = FanCurveRecord {
            sensor_id: Some("sensor_0".to_string()),
            points: vec![(0.0, 21.0), (100.0, 21.0)],
        };
        let fan = MockFan::new_with_curve("fan_0", record);
        let sensor = MockSensor::new("sensor_0", 50.0);
        let app = make_app(fan.clone(), Some(sensor));
        crate::application::usecases::device::telemetry::observe(&app).await;
        let engine = FanCurveEngine::new(app);
        engine.tick(75).await;
        assert_eq!(
            fan.last_duty(),
            20,
            "delta of exactly 1.0 must not trigger a duty write"
        );
    }

    #[tokio::test]
    async fn record_missing_device_increments_per_call() {
        let app = Arc::new(AppState::new(Config::default()));
        let engine = FanCurveEngine::new(app);
        assert_eq!(engine.missing_ticks("fan_0"), 0);
        engine.record_missing_device("fan_0");
        assert_eq!(engine.missing_ticks("fan_0"), 1, "first miss counts");
        engine.record_missing_device("fan_0");
        engine.record_missing_device("fan_0");
        assert_eq!(
            engine.missing_ticks("fan_0"),
            3,
            "each miss increments by one"
        );
    }

    #[tokio::test]
    async fn tick_sets_failsafe_when_sensor_not_found() {
        let record = FanCurveRecord {
            sensor_id: Some("missing_sensor".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_with_curve("fan_0", record);
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app);
        engine.tick(75).await;
        assert_eq!(fan.last_duty(), 75);
    }

    #[tokio::test]
    async fn tick_sets_failsafe_for_non_finite_temperature() {
        let record = FanCurveRecord {
            sensor_id: Some("sensor_0".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_with_curve("fan_0", record);
        let sensor = MockSensor::new("sensor_0", f64::NAN);
        let app = make_app(fan.clone(), Some(sensor));
        crate::application::usecases::device::telemetry::observe(&app).await;

        let engine = FanCurveEngine::new(app.clone());
        engine.tick(75).await;

        assert_eq!(fan.last_duty(), 75);
        assert_eq!(
            app.cooling.statuses.lock().await[&curve_key("fan_0", "default")],
            FanCurveStatus::Ok
        );
        engine.tick(75).await;
        assert_eq!(
            app.cooling.statuses.lock().await[&curve_key("fan_0", "default")],
            FanCurveStatus::NoSensor
        );
    }

    #[tokio::test]
    async fn tick_applies_failsafe_when_no_sensor_assigned() {
        let record = FanCurveRecord {
            sensor_id: None,
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_with_curve("fan_0", record);
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app.clone());
        engine.tick(75).await;
        assert_eq!(fan.last_duty(), 75);
        assert_eq!(
            app.cooling.statuses.lock().await[&curve_key("fan_0", "default")],
            FanCurveStatus::Ok
        );
        engine.tick(75).await;
        let statuses = app.cooling.statuses.lock().await;
        assert_eq!(
            statuses[&curve_key("fan_0", "default")],
            FanCurveStatus::NoSensor
        );
    }

    #[tokio::test]
    async fn constant_duty_curve_does_not_need_a_sensor() {
        let record = FanCurveRecord {
            sensor_id: Some("disabled_hwmon_sensor".to_string()),
            points: vec![(0.0, 100.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_with_curve("fan_0", record);
        let app = make_app(fan.clone(), None);

        FanCurveEngine::new(app).tick(75).await;

        assert_eq!(fan.last_duty(), 100);
    }

    #[tokio::test]
    async fn failsafe_does_not_write_to_offline_fan() {
        let record = FanCurveRecord {
            sensor_id: None,
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_offline_with_curve("fan_0", record);
        let initial_duty = fan.last_duty();
        let app = make_app(fan.clone(), None);

        FanCurveEngine::new(app).tick(75).await;

        assert_eq!(fan.last_duty(), initial_duty);
    }

    #[tokio::test]
    async fn tick_seeds_default_curve_for_unconfigured_fan() {
        // Fan has no curve pre-loaded — tick should auto-seed one into its slot.
        let fan = MockFan::new("fan_0");
        assert!(
            fan.cooling.curve("default").is_none(),
            "starts with no curve"
        );

        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app.clone());
        engine.tick(75).await;

        assert!(
            fan.cooling.curve("default").is_some(),
            "fan_0 should have been seeded on the first tick"
        );
    }

    #[tokio::test]
    async fn tick_seeded_curve_has_no_sensor_and_applies_failsafe() {
        let fan = MockFan::new("fan_0");
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app.clone());
        engine.tick(75).await;

        assert_eq!(fan.last_duty(), 75);
        assert_eq!(
            app.cooling.statuses.lock().await[&curve_key("fan_0", "default")],
            FanCurveStatus::Ok
        );
        engine.tick(75).await;
        let statuses = app.cooling.statuses.lock().await;
        assert_eq!(
            statuses[&curve_key("fan_0", "default")],
            FanCurveStatus::NoSensor
        );
    }

    #[tokio::test]
    async fn tick_does_not_reseed_existing_curve() {
        let custom_record = FanCurveRecord {
            sensor_id: None,
            points: vec![(0.0, 42.0), (100.0, 42.0)],
        };
        let fan = MockFan::new_with_curve("fan_0", custom_record);
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app.clone());
        engine.tick(75).await;

        // The original custom points should be preserved, not replaced by balanced defaults
        let record = fan
            .cooling
            .curve("default")
            .expect("curve should still be set");
        assert_eq!(
            record.points[0].1, 42.0,
            "existing curve should not be overwritten"
        );
    }

    async fn mark_active_state(
        app: &Arc<AppState>,
        device_id: &str,
        state: halod_shared::types::VisibilityState,
    ) {
        use crate::domain::registry::model::DeviceRecord;
        let mut cfg = app.config.write().await;
        cfg.known_devices.insert(
            device_id.to_string(),
            DeviceRecord {
                name: device_id.to_string(),
                vendor: "test".into(),
                model: "test".into(),
                active_state: state,
            },
        );
    }

    #[tokio::test]
    async fn tick_skips_disabled_fan_and_does_not_seed_curve() {
        let fan = MockFan::new("fan_0");
        let app = make_app(fan.clone(), None);
        mark_active_state(
            &app,
            "fan_0",
            halod_shared::types::VisibilityState::Disabled,
        )
        .await;
        let initial_duty = fan.last_duty();

        let engine = FanCurveEngine::new(app.clone());
        engine.tick(75).await;

        assert!(
            fan.cooling.curve("default").is_none(),
            "disabled fan must not get a default curve seeded"
        );
        assert_eq!(
            fan.last_duty(),
            initial_duty,
            "disabled fan must not receive a duty write"
        );
        let statuses = app.cooling.statuses.lock().await;
        assert!(
            !statuses.contains_key("fan_0"),
            "disabled fan must not appear in fan_curve_statuses"
        );
    }

    #[tokio::test]
    async fn tick_skips_disabled_channel_and_does_not_seed_curve() {
        let fan = MockFan::new("fan_0");
        let app = make_app(fan.clone(), None);
        {
            let mut cfg = app.config.write().await;
            cfg.channel_visibility
                .entry("fan_0".into())
                .or_default()
                .insert(
                    halod_shared::types::ChannelKind::Cooling.key("default"),
                    halod_shared::types::VisibilityState::Disabled,
                );
        }
        let initial_duty = fan.last_duty();

        FanCurveEngine::new(app.clone()).tick(75).await;

        assert!(fan.cooling.curve("default").is_none());
        assert_eq!(fan.last_duty(), initial_duty);
        assert!(app.cooling.statuses.lock().await.is_empty());
    }

    #[tokio::test]
    async fn tick_drives_a_hidden_channel() {
        let record = FanCurveRecord {
            sensor_id: Some("sensor_0".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_with_curve("fan_0", record);
        let app = make_app(fan.clone(), Some(MockSensor::new("sensor_0", 80.0)));
        {
            let mut cfg = app.config.write().await;
            cfg.channel_visibility
                .entry("fan_0".into())
                .or_default()
                .insert(
                    halod_shared::types::ChannelKind::Cooling.key("default"),
                    halod_shared::types::VisibilityState::Hidden,
                );
        }
        crate::application::usecases::device::telemetry::observe(&app).await;

        FanCurveEngine::new(app.clone()).tick(75).await;

        assert_ne!(
            fan.last_duty(),
            0,
            "Hidden only hides the channel in the UI; the curve keeps driving it"
        );
    }

    #[tokio::test]
    async fn tick_skips_hidden_fan() {
        let record = FanCurveRecord {
            sensor_id: Some("sensor_0".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_with_curve("fan_0", record);
        let sensor = MockSensor::new("sensor_0", 80.0);
        let app = make_app(fan.clone(), Some(sensor));
        mark_active_state(&app, "fan_0", halod_shared::types::VisibilityState::Hidden).await;
        let initial_duty = fan.last_duty();

        let engine = FanCurveEngine::new(app);
        engine.tick(75).await;

        assert_eq!(
            fan.last_duty(),
            initial_duty,
            "hidden fan must not receive a duty write"
        );
    }

    #[tokio::test]
    async fn process_fan_reports_no_device_when_fan_device_missing() {
        // Sensor present but the fan device is gone: must not report Ok, and
        // must be NoDevice rather than WriteError (no write was attempted).
        let sensor = MockSensor::new("s", 50.0);
        let app = Arc::new(AppState::new(Config::default()));
        *app.device_registry.try_write().unwrap() = vec![sensor as Arc<dyn Device>];
        let engine = FanCurveEngine::new(app.clone());
        crate::application::usecases::device::telemetry::observe(&app).await;
        let sensors = app.data_bus.sensors();
        let record = FanCurveRecord {
            sensor_id: Some("s".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let status = engine.process_fan("ghost_fan", &record, &sensors, 75).await;
        assert_eq!(status, FanCurveStatus::NoDevice);
    }

    #[tokio::test]
    async fn process_fan_skips_an_offline_device() {
        // An offline fan (e.g. an integration whose server dropped) is present
        // but unreachable: no duty write, and reported as NoDevice so the engine
        // stops hammering the dead socket.
        let record = FanCurveRecord {
            sensor_id: Some("sensor_0".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_offline_with_curve("fan_0", record.clone());
        let sensor = MockSensor::new("sensor_0", 80.0);
        let app = make_app(fan.clone(), Some(sensor));
        let initial_duty = fan.last_duty();
        let engine = FanCurveEngine::new(app.clone());
        crate::application::usecases::device::telemetry::observe(&app).await;
        let sensors = app.data_bus.sensors();

        let status = engine.process_fan("fan_0", &record, &sensors, 75).await;

        assert_eq!(status, FanCurveStatus::NoDevice);
        assert_eq!(
            fan.last_duty(),
            initial_duty,
            "an offline fan must not receive a duty write"
        );
    }

    #[tokio::test]
    async fn check_stall_returns_ok_when_fan_is_spinning() {
        let record = FanCurveRecord {
            sensor_id: Some("s".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_with_curve("fan_0", record);
        // rpm defaults to 1000 — fan is spinning
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app);
        let device: Arc<dyn crate::domain::device::Device> = fan;
        let status = engine.check_stall("fan_0", &device, 50.0).await;
        assert_eq!(status, FanCurveStatus::Ok);
    }

    #[tokio::test]
    async fn check_stall_returns_ok_during_grace_period() {
        let record = FanCurveRecord {
            sensor_id: Some("s".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_stalled("fan_0", record);
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app);
        let device: Arc<dyn crate::domain::device::Device> = fan;
        // Stall just started — within 10 s grace period
        let status = engine.check_stall("fan_0", &device, 50.0).await;
        assert_eq!(status, FanCurveStatus::Ok);
    }

    #[tokio::test]
    async fn check_stall_returns_fan_stalled_after_grace_period() {
        let record = FanCurveRecord {
            sensor_id: Some("s".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_stalled("fan_0", record);
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app);
        engine.seed_stall(
            "fan_0",
            std::time::Instant::now() - std::time::Duration::from_secs(15),
            false,
        );
        let device: Arc<dyn crate::domain::device::Device> = fan;
        let status = engine.check_stall("fan_0", &device, 50.0).await;
        assert_eq!(status, FanCurveStatus::FanStalled);
    }

    #[tokio::test]
    async fn check_stall_returns_ok_when_duty_below_threshold() {
        let record = FanCurveRecord {
            sensor_id: Some("s".to_string()),
            points: vec![(0.0, 10.0), (100.0, 10.0)],
        };
        let fan = MockFan::new_stalled("fan_0", record);
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app);
        engine.seed_stall(
            "fan_0",
            std::time::Instant::now() - std::time::Duration::from_secs(15),
            false,
        );
        let device: Arc<dyn crate::domain::device::Device> = fan;
        // target = 10% ≤ 20% → not considered a stall
        let status = engine.check_stall("fan_0", &device, 10.0).await;
        assert_eq!(status, FanCurveStatus::Ok);
        assert!(engine.per_fan.lock().unwrap()["fan_0"].stall.is_none());
    }

    #[tokio::test]
    async fn check_stall_notifies_only_once_per_episode() {
        let record = FanCurveRecord {
            sensor_id: Some("s".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_stalled("fan_0", record);
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app);
        engine.seed_stall(
            "fan_0",
            std::time::Instant::now() - std::time::Duration::from_secs(15),
            true, // already notified
        );
        let device: Arc<dyn crate::domain::device::Device> = fan;
        let status = engine.check_stall("fan_0", &device, 50.0).await;
        assert_eq!(status, FanCurveStatus::FanStalled);
        // notified flag must stay true
        assert!(
            engine.per_fan.lock().unwrap()["fan_0"]
                .stall
                .as_ref()
                .unwrap()
                .notified
        );
    }

    #[tokio::test]
    async fn check_stall_clears_when_fan_recovers() {
        let record = FanCurveRecord {
            sensor_id: Some("s".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_with_curve("fan_0", record);
        // rpm = 1000 (spinning)
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app);
        engine.seed_stall(
            "fan_0",
            std::time::Instant::now() - std::time::Duration::from_secs(15),
            true,
        );
        let device: Arc<dyn crate::domain::device::Device> = fan;
        let status = engine.check_stall("fan_0", &device, 50.0).await;
        assert_eq!(status, FanCurveStatus::Ok);
        assert!(engine.per_fan.lock().unwrap()["fan_0"].stall.is_none());
    }

    #[tokio::test]
    async fn check_stall_target_at_threshold_is_not_a_stall() {
        // Exactly DUTY_THRESHOLD (20%) is not a stall (`> 20.0` is strict): even a
        // long-stalled fan returns Ok and clears tracking. Pins `>` against `>=`.
        let record = FanCurveRecord {
            sensor_id: Some("s".to_string()),
            points: vec![(0.0, 20.0), (100.0, 20.0)],
        };
        let fan = MockFan::new_stalled("fan_0", record);
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app);
        engine.seed_stall(
            "fan_0",
            std::time::Instant::now() - std::time::Duration::from_secs(15),
            false,
        );
        let device: Arc<dyn crate::domain::device::Device> = fan;
        let status = engine.check_stall("fan_0", &device, 20.0).await;
        assert_eq!(status, FanCurveStatus::Ok);
        assert!(engine.per_fan.lock().unwrap()["fan_0"].stall.is_none());
    }

    #[tokio::test]
    async fn check_stall_sets_notified_flag_after_grace_period() {
        // Past the grace period a not-yet-notified stall fires once and sets
        // `notified`. Pins `elapsed >= STALL_SECS && !entry.notified`.
        let record = FanCurveRecord {
            sensor_id: Some("s".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_stalled("fan_0", record);
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app);
        engine.seed_stall(
            "fan_0",
            std::time::Instant::now() - std::time::Duration::from_secs(15),
            false,
        );
        let device: Arc<dyn crate::domain::device::Device> = fan;
        let _ = engine.check_stall("fan_0", &device, 50.0).await;
        assert!(
            engine.per_fan.lock().unwrap()["fan_0"]
                .stall
                .as_ref()
                .unwrap()
                .notified,
            "crossing the grace period must fire the notification and set the flag"
        );
    }

    #[tokio::test]
    async fn check_stall_does_not_notify_within_grace_period() {
        // Within the grace period nothing fires and `notified` stays false.
        // Pins the `&&` against `||`.
        let record = FanCurveRecord {
            sensor_id: Some("s".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let fan = MockFan::new_stalled("fan_0", record); // fresh stall, elapsed ≈ 0
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app);
        let device: Arc<dyn crate::domain::device::Device> = fan;
        let status = engine.check_stall("fan_0", &device, 50.0).await;
        assert_eq!(status, FanCurveStatus::Ok);
        assert!(
            !engine.per_fan.lock().unwrap()["fan_0"]
                .stall
                .as_ref()
                .unwrap()
                .notified,
            "must not notify during the grace period"
        );
    }

    struct MockPump {
        id: &'static str,
        duty: StdMutex<u8>,
        cooling: CoolingStateSlot,
    }

    impl MockPump {
        fn new_with_curve(id: &'static str, record: FanCurveRecord) -> Arc<Self> {
            let p = Arc::new(Self {
                id,
                duty: StdMutex::new(50),
                cooling: CoolingStateSlot::default(),
            });
            p.cooling.set_curve("pump".to_string(), record);
            p
        }

        fn last_duty(&self) -> u8 {
            *self.duty.lock().unwrap()
        }
    }

    #[async_trait]
    impl Device for MockPump {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.id
        }
        fn vendor(&self) -> &str {
            "test"
        }
        fn model(&self) -> &str {
            "test"
        }
        async fn initialize(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::Cooling(self)]
        }
    }

    #[async_trait]
    impl CoolingCapability for MockPump {
        fn cooling_channels(&self) -> Vec<CoolingChannel> {
            vec![CoolingChannel {
                id: "pump".into(),
                name: "Pump".into(),
                kind: CoolingChannelKind::Pump,
                controllable: true,
                rpm: None,
                duty: Some(*self.duty.lock().unwrap()),
                visibility: Default::default(),
            }]
        }
        async fn get_cooling_status(&self, channel_id: &str) -> anyhow::Result<CoolingChannel> {
            self.cooling_channels()
                .into_iter()
                .find(|channel| channel.id == channel_id)
                .ok_or_else(|| anyhow::anyhow!("unknown cooling channel '{channel_id}'"))
        }
        async fn set_cooling_duty(&self, _channel_id: &str, duty: u8) -> anyhow::Result<()> {
            *self.duty.lock().unwrap() = duty;
            Ok(())
        }
        fn cooling_state(&self) -> &CoolingStateSlot {
            &self.cooling
        }
    }

    #[tokio::test]
    async fn tick_applies_duty_to_pump_device() {
        let record = FanCurveRecord {
            sensor_id: Some("sensor_0".to_string()),
            points: vec![(0.0, 30.0), (100.0, 100.0)],
        };
        let pump = MockPump::new_with_curve("pump_0", record);
        let app = Arc::new(AppState::new(Config::default()));
        let sensor = MockSensor::new("sensor_0", 50.0);
        *app.device_registry.try_write().unwrap() =
            vec![pump.clone() as Arc<dyn Device>, sensor as Arc<dyn Device>];
        crate::application::usecases::device::telemetry::observe(&app).await;
        let engine = FanCurveEngine::new(app);
        engine.tick(75).await;
        assert_ne!(pump.last_duty(), 50, "pump duty should have been updated");
    }

    #[tokio::test]
    async fn check_stall_skips_for_pump_without_rpm() {
        let record = FanCurveRecord {
            sensor_id: Some("s".to_string()),
            points: vec![(0.0, 50.0), (100.0, 100.0)],
        };
        let pump = MockPump::new_with_curve("pump_0", record);
        let app = Arc::new(AppState::new(Config::default()));
        *app.device_registry.try_write().unwrap() = vec![pump.clone() as Arc<dyn Device>];
        let engine = FanCurveEngine::new(app);
        let device: Arc<dyn crate::domain::device::Device> = pump;
        let status = engine.check_stall("pump_0", &device, 60.0).await;
        assert_eq!(
            status,
            FanCurveStatus::Ok,
            "pump with no RPM skips stall check"
        );
    }
}

#[cfg(test)]
mod prop_tests {
    use super::{hysteresis_temp, interpolate, HYSTERESIS_C};
    use proptest::prelude::*;

    /// Non-empty curve with strictly ascending temperatures and duties in
    /// `[0, 100]`. Accumulates positive gaps so temps strictly increase without
    /// relying on float sort stability.
    fn ascending_curve() -> impl Strategy<Value = Vec<(f32, f32)>> {
        prop::collection::vec((0.5f32..20.0, 0.0f32..100.0), 1..8).prop_map(|gaps| {
            let mut temp = -20.0f32;
            gaps.into_iter()
                .map(|(gap, duty)| {
                    temp += gap;
                    (temp, duty)
                })
                .collect()
        })
    }

    proptest! {
        /// `interpolate` never produces NaN/inf and stays within the curve's
        /// min/max duty — it only blends or clamps, never extrapolates.
        #[test]
        fn interpolate_stays_within_curve_duty_bounds(
            curve in ascending_curve(),
            temp in -100.0f32..200.0,
        ) {
            let out = interpolate(&curve, temp);
            prop_assert!(out.is_finite());
            let min = curve.iter().map(|p| p.1).fold(f32::MAX, f32::min);
            let max = curve.iter().map(|p| p.1).fold(f32::MIN, f32::max);
            prop_assert!(out >= min - 1e-3 && out <= max + 1e-3, "{out} not in [{min}, {max}]");
        }

        /// Below the first control temperature the duty is pinned to the first
        /// point; above the last it is pinned to the last point.
        #[test]
        fn interpolate_clamps_outside_range(curve in ascending_curve()) {
            let first = *curve.first().unwrap();
            let last = *curve.last().unwrap();
            prop_assert_eq!(interpolate(&curve, first.0 - 10.0), first.1);
            prop_assert_eq!(interpolate(&curve, last.0 + 10.0), last.1);
        }

        /// A non-decreasing curve yields a non-decreasing duty: hotter never
        /// produces a lower duty. The core safety property of the cooling curve.
        #[test]
        fn interpolate_is_monotonic_for_monotonic_curves(
            curve in ascending_curve(),
            t1 in -100.0f32..200.0,
            t2 in -100.0f32..200.0,
        ) {
            // Force the duties non-decreasing along the (already-ascending) temps.
            let mut mono = curve;
            let mut running = 0.0f32;
            for p in mono.iter_mut() {
                running = running.max(p.1);
                p.1 = running;
            }
            let (lo, hi) = if t1 <= t2 { (t1, t2) } else { (t2, t1) };
            prop_assert!(interpolate(&mono, lo) <= interpolate(&mono, hi) + 1e-3);
        }

        /// Downward hysteresis only holds a higher previous value within the band;
        /// rising temperatures always pass through immediately.
        #[test]
        fn hysteresis_holds_within_band_only(last in -50.0f32..150.0, current in -50.0f32..150.0) {
            let out = hysteresis_temp(Some(last), current);
            if current >= last {
                prop_assert_eq!(out, current, "rising temps pass through");
            } else if current > last - HYSTERESIS_C {
                prop_assert_eq!(out, last, "small drop holds the previous temp");
            } else {
                prop_assert_eq!(out, current, "drop beyond the band tracks immediately");
            }
        }
    }
}
