//! Fan curve engine
//!
//! The engine runs a control loop every `TICK_RATE_SECS`. On each tick it
//! reads per-device fan curve state from device traits, then for every
//! assigned fan calls `process_fan`, which:
//!
//!   1. Looks up the temperature sensor value via `AppState::find_sensor_by_id`.
//!   2. Linearly interpolates the target duty% from the curve's control points.
//!   3. Calls `FanCapability::set_duty` only when the delta vs. the current duty
//!      exceeds 1 pp, avoiding unnecessary writes on stable temperatures.
//!
//! The resulting `FanCurveStatus` for each fan is written into
//! `AppState::fan_curve_statuses`, which the serializer picks up and broadcasts
//! to connected UI clients on every state push.
//!
//! # Failsafe
//!
//! Any condition that prevents safe closed-loop control drives the fan to
//! `FAILSAFE_DUTY` (75%) and sets the appropriate status:
//!
//! - `NoSensor` — `sensor_id` is `None` in config (curve created but not yet
//!   configured by the user), or the sensor device is no longer present.
//!
//! # Quirks
//!
//! - **Stale detection uses per-fan history**, not per-sensor, so two fans
//!   sharing the same sensor each maintain independent staleness timers. This
//!   means one fan switching to malfunction does not immediately affect the other.
//! - **Auto-seed on first tick**: a default Balanced curve with `sensor_id: None`
//!   is written into the device's `FanEngineSlot` on the first tick for any fan
//!   that has no existing curve. This makes the fan visible in the UI immediately,
//!   but it stays in failsafe until the user assigns a sensor.
//! - **Profile changes take effect on the next tick** — the engine re-reads
//!   device-level fan curves at the start of each tick.
//! - **Curve statuses are garbage-collected** each tick: any fan whose curve was
//!   removed is dropped from `fan_curve_statuses`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::FanCurveRecord;
use crate::state::{AppState, EngineRunConfig};
use halod_protocol::types::FanCurveStatus;
use tokio::sync::watch;

pub struct PresetCurve {
    pub id: &'static str,
    pub name: &'static str,
    pub points: &'static [(f32, f32)],
}

impl PresetCurve {
    pub fn serialize(&self) -> halod_protocol::types::WirePresetCurve {
        halod_protocol::types::WirePresetCurve {
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

pub struct FanCurveEngine {
    app_state: Arc<AppState>,
    /// Per-fan count of consecutive ticks the device has been missing. Used to
    /// log the "device not found" warning once per disappearance episode
    /// instead of on every tick. Cleared when the device reappears.
    missing_device_ticks: std::sync::Mutex<HashMap<String, u32>>,
    /// Per-fan stall tracking: set when RPM is 0 while curve target is >20%,
    /// cleared when the fan starts spinning again or duty drops to ≤20%.
    stall_state: std::sync::Mutex<HashMap<String, StallState>>,
}

/// Whether the "device not found" warning should be logged, given how many
/// consecutive ticks the fan's device has been missing. Logs only on the first
/// miss of an episode — devices reconnect transiently, so per-tick logging
/// would flood the log for a device that is merely unplugged.
fn should_log_missing_device(consecutive_misses: u32) -> bool {
    consecutive_misses == 1
}

impl FanCurveEngine {
    pub fn new(app_state: Arc<AppState>) -> Arc<Self> {
        Arc::new(Self {
            app_state,
            missing_device_ticks: std::sync::Mutex::new(HashMap::new()),
            stall_state: std::sync::Mutex::new(HashMap::new()),
        })
    }

    pub fn start(
        self: Arc<Self>,
        cfg_rx: watch::Receiver<EngineRunConfig>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            crate::engines::engine_run_loop(
                "FanCurve",
                cfg_rx,
                tokio::time::MissedTickBehavior::Skip,
                |cfg| {
                    let this = Arc::clone(&self);
                    async move { this.tick(cfg.failsafe_duty).await }
                },
            )
            .await;
        })
    }

    async fn tick(&self, failsafe_duty: u8) {
        let curves: HashMap<String, crate::config::FanCurveRecord> = {
            // Only Visible devices participate. Hidden/Disabled devices stay in
            // app.devices but must not be driven: writing a "default" duty could
            // be unsafe for cooling hardware (e.g. pumps), and Disabled devices
            // have been close()d so writes would fail anyway. Without this gate
            // the auto-seed branch below would also reinstate a default curve
            // every tick — defeating the clear_fan_curve() that visibility.rs
            // performs on hide/disable.
            let fan_devices: Vec<_> = self
                .app_state
                .get_active_devices()
                .await
                .into_iter()
                .filter(|d| d.as_fan().is_some())
                .collect();

            let mut map = HashMap::new();
            for device in &fan_devices {
                if let Some(fan) = device.as_fan() {
                    let curve = if let Some(c) = fan.fan_curve() {
                        c
                    } else {
                        let default = default_curve();
                        fan.set_fan_curve(default.clone());
                        crate::usecases::persist_device_state(&self.app_state, device.as_ref())
                            .await;
                        default
                    };
                    map.insert(device.id(), curve);
                }
            }
            map
        };

        for (fan_id, record) in &curves {
            let status = self.process_fan(fan_id, record, failsafe_duty).await;
            self.app_state
                .fan_curve_statuses
                .lock()
                .await
                .insert(fan_id.clone(), status);
        }

        // Remove statuses for fans that no longer have a curve.
        let fan_ids: std::collections::HashSet<String> = curves.keys().cloned().collect();
        self.app_state
            .fan_curve_statuses
            .lock()
            .await
            .retain(|k, _| fan_ids.contains(k));
    }

    async fn process_fan(
        &self,
        fan_id: &str,
        record: &FanCurveRecord,
        failsafe_duty: u8,
    ) -> FanCurveStatus {
        let sensor_id = match &record.sensor_id {
            Some(s) => s,
            None => {
                self.apply_failsafe(fan_id, failsafe_duty).await;
                return FanCurveStatus::NoSensor;
            }
        };

        let sensor = match self.app_state.find_sensor_by_id(sensor_id).await {
            Some(s) => s,
            None => {
                log::debug!("[FanCurve] Sensor not found: {sensor_id}");
                self.apply_failsafe(fan_id, failsafe_duty).await;
                return FanCurveStatus::NoSensor;
            }
        };
        let temp = sensor.value;

        // Normal control path.
        let target = interpolate(&record.points, temp as f32);
        let fan_device = self.app_state.find_device_by_id(fan_id).await;
        if let Some(device) = &fan_device {
            self.missing_device_ticks.lock().unwrap().remove(fan_id);
            let current = current_duty(device).await;
            if (current - target).abs() > 1.0 {
                let duty = target.round().clamp(0.0, 100.0) as u8;
                if let Err(e) = apply_duty(device, duty).await {
                    log::warn!("[FanCurve] Failed to set duty for {fan_id}: {e}");
                    return FanCurveStatus::WriteError(e.to_string());
                }
            }
            return self.check_stall(fan_id, device, target).await;
        } else {
            self.note_missing_device(fan_id);
        }

        FanCurveStatus::Ok
    }

    /// Record one tick where `fan_id`'s device was absent, logging the warning
    /// only on the first miss of the episode (see `should_log_missing_device`).
    fn note_missing_device(&self, fan_id: &str) {
        let mut misses = self.missing_device_ticks.lock().unwrap();
        let count = misses.entry(fan_id.to_string()).or_insert(0);
        *count += 1;
        if should_log_missing_device(*count) {
            log::debug!("[FanCurve] Fan/pump device not found or not controllable: {fan_id}");
        }
    }

    /// Check whether a fan device is stalled (RPM == 0 while target duty >20%)
    /// for more than 10 s. Fires a one-shot warning notification and returns
    /// `FanStalled`. Clears tracking when the fan starts spinning again or the
    /// duty drops to ≤20%. Pumps and non-fan devices are not checked (returns `Ok`).
    async fn check_stall(
        &self,
        fan_id: &str,
        device: &Arc<dyn crate::drivers::Device>,
        target: f32,
    ) -> FanCurveStatus {
        const STALL_SECS: u64 = 10;
        const DUTY_THRESHOLD: f32 = 20.0;

        let rpm = match current_rpm(device).await {
            Some(r) => r,
            None => return FanCurveStatus::Ok, // pumps: skip check
        };

        let stalled = rpm == 0 && target > DUTY_THRESHOLD;

        if stalled {
            let should_notify = {
                let mut map = self.stall_state.lock().unwrap();
                let entry = map.entry(fan_id.to_string()).or_insert_with(|| StallState {
                    since: std::time::Instant::now(),
                    notified: false,
                });
                if entry.since.elapsed().as_secs() >= STALL_SECS && !entry.notified {
                    entry.notified = true;
                    true
                } else {
                    false
                }
            };

            if should_notify {
                let title = format!("Fan stalled — {fan_id}");
                let msg = format!(
                    "Fan is not spinning (0 RPM) while curve duty is {target:.0}%. \
                     Check fan connections or replace the fan."
                );
                crate::notify::warn(&self.app_state, title, msg).await;
            }

            let elapsed = self
                .stall_state
                .lock()
                .unwrap()
                .get(fan_id)
                .map(|e| e.since.elapsed().as_secs())
                .unwrap_or(0);

            if elapsed >= STALL_SECS {
                FanCurveStatus::FanStalled
            } else {
                FanCurveStatus::Ok
            }
        } else {
            self.stall_state.lock().unwrap().remove(fan_id);
            FanCurveStatus::Ok
        }
    }

    #[cfg(test)]
    fn seed_stall(&self, fan_id: &str, since: std::time::Instant, notified: bool) {
        self.stall_state
            .lock()
            .unwrap()
            .insert(fan_id.to_string(), StallState { since, notified });
    }

    async fn apply_failsafe(&self, fan_id: &str, duty: u8) {
        let fan_device = self.app_state.find_device_by_id(fan_id).await;
        if let Some(device) = &fan_device {
            if let Err(e) = apply_duty(device, duty).await {
                log::warn!("[FanCurve] Failsafe set_duty failed for {fan_id}: {e}");
            }
        }
    }
}

async fn current_duty(device: &Arc<dyn crate::drivers::Device>) -> f32 {
    if let Some(fan) = device.as_fan() {
        fan.get_duty().await.unwrap_or(0) as f32
    } else {
        0.0
    }
}

async fn current_rpm(device: &Arc<dyn crate::drivers::Device>) -> Option<u32> {
    let fan = device.as_fan()?;
    fan.get_rpm().await
}

async fn apply_duty(device: &Arc<dyn crate::drivers::Device>, duty: u8) -> anyhow::Result<()> {
    if let Some(fan) = device.as_fan() {
        fan.set_duty(duty).await
    } else {
        Err(anyhow::anyhow!("device has no duty capability"))
    }
}

fn interpolate(points: &[(f32, f32)], temp: f32) -> f32 {
    debug_assert!(
        points.windows(2).all(|w| w[0].0 <= w[1].0),
        "interpolate requires ascending temperatures, got {points:?}"
    );
    if points.is_empty() {
        return 0.0;
    }
    if points.len() == 1 {
        return points[0].1;
    }
    if temp <= points[0].0 {
        return points[0].1;
    }
    let last = points.last().unwrap();
    if temp >= last.0 {
        return last.1;
    }
    for i in 0..points.len() - 1 {
        let (x1, y1) = points[i];
        let (x2, y2) = points[i + 1];
        if temp >= x1 && temp <= x2 {
            return y1 + (y2 - y1) * (temp - x1) / (x2 - x1);
        }
    }
    last.1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FanCurveRecord};
    use crate::drivers::{CapabilityRef, Device, FanCapability, FanStateSlot, SensorCapability};
    use async_trait::async_trait;
    use halod_protocol::types::{Sensor, SensorUnit};
    use std::sync::Mutex as StdMutex;

    // --- missing-device logging ---

    #[test]
    fn missing_device_logs_only_on_first_miss() {
        assert!(should_log_missing_device(1), "first miss logs");
        assert!(!should_log_missing_device(2), "second miss is silent");
        assert!(!should_log_missing_device(100), "later misses stay silent");
    }

    // --- interpolate ---

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

    // A curve restored from a hand-edited / corrupted config bypasses the API's
    // validate_points. The slot setter must normalize it so the engine never
    // interpolates over unsorted points.
    #[test]
    fn set_fan_curve_normalizes_unsorted_points() {
        let slot = FanStateSlot::default();
        slot.set_fan_curve(FanCurveRecord {
            sensor_id: None,
            points: vec![(80.0, 100.0), (30.0, 20.0), (55.0, 50.0)],
        });
        let points = slot.fan_curve().unwrap().points;
        assert!(
            points.windows(2).all(|w| w[0].0 <= w[1].0),
            "stored points must be ascending, got {points:?}"
        );
        // interpolate now agrees with the intended sorted curve:
        // 45 °C lies in (30,20)→(55,50): 20 + 30*(15/25) = 38%.
        assert!((interpolate(&points, 45.0) - 38.0).abs() < 0.01);
    }

    // --- mock helpers ---

    struct MockFan {
        id: &'static str,
        duty: StdMutex<u8>,
        rpm: StdMutex<u32>,
        fan: FanStateSlot,
    }

    impl MockFan {
        fn new(id: &'static str) -> Arc<Self> {
            Arc::new(Self {
                id,
                duty: StdMutex::new(20),
                rpm: StdMutex::new(1000),
                fan: FanStateSlot::default(),
            })
        }

        fn new_with_curve(id: &'static str, record: FanCurveRecord) -> Arc<Self> {
            let this = Self::new(id);
            this.fan.set_fan_curve(record);
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
        fn id(&self) -> String {
            self.id.to_string()
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
            vec![CapabilityRef::Fan(self)]
        }
    }

    #[async_trait]
    impl FanCapability for MockFan {
        async fn get_duty(&self) -> anyhow::Result<u8> {
            Ok(*self.duty.lock().unwrap())
        }
        async fn set_duty(&self, duty: u8) -> anyhow::Result<()> {
            *self.duty.lock().unwrap() = duty;
            Ok(())
        }
        async fn get_rpm(&self) -> Option<u32> {
            Some(*self.rpm.lock().unwrap())
        }
        fn fan_state(&self) -> &FanStateSlot {
            &self.fan
        }
    }

    struct MockSensor {
        sensor_id: &'static str,
        temp: f64,
    }

    impl MockSensor {
        fn new(sensor_id: &'static str, temp: f64) -> Arc<Self> {
            Arc::new(Self { sensor_id, temp })
        }
    }

    #[async_trait]
    impl Device for MockSensor {
        fn id(&self) -> String {
            format!("device_{}", self.sensor_id)
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
                sensor_type: halod_protocol::types::SensorType::Temperature,
                visibility: Default::default(),
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
        *app.devices.try_lock().unwrap() = devices;
        app
    }

    // --- tick tests ---

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
        let engine = FanCurveEngine::new(app);
        engine.tick(75).await;
        assert_ne!(fan.last_duty(), 20, "duty should have been updated from 20");
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
        let statuses = app.fan_curve_statuses.lock().await;
        assert_eq!(statuses["fan_0"], FanCurveStatus::NoSensor);
    }

    #[tokio::test]
    async fn tick_seeds_default_curve_for_unconfigured_fan() {
        // Fan has no curve pre-loaded — tick should auto-seed one into its slot.
        let fan = MockFan::new("fan_0");
        assert!(fan.fan.fan_curve().is_none(), "starts with no curve");

        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app.clone());
        engine.tick(75).await;

        assert!(
            fan.fan.fan_curve().is_some(),
            "fan_0 should have been seeded on the first tick"
        );
    }

    #[tokio::test]
    async fn tick_seeded_curve_has_no_sensor_and_applies_failsafe() {
        let fan = MockFan::new("fan_0");
        let app = make_app(fan.clone(), None);
        let engine = FanCurveEngine::new(app.clone());
        engine.tick(75).await;

        // Seeded curve has sensor_id: None → NoSensor failsafe
        assert_eq!(fan.last_duty(), 75);
        let statuses = app.fan_curve_statuses.lock().await;
        assert_eq!(statuses["fan_0"], FanCurveStatus::NoSensor);
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
        let record = fan.fan.fan_curve().expect("curve should still be set");
        assert_eq!(
            record.points[0].1, 42.0,
            "existing curve should not be overwritten"
        );
    }

    async fn mark_active_state(
        app: &Arc<AppState>,
        device_id: &str,
        state: halod_protocol::types::VisibilityState,
    ) {
        use crate::config::DeviceRecord;
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
            halod_protocol::types::VisibilityState::Disabled,
        )
        .await;
        let initial_duty = fan.last_duty();

        let engine = FanCurveEngine::new(app.clone());
        engine.tick(75).await;

        assert!(
            fan.fan.fan_curve().is_none(),
            "disabled fan must not get a default curve seeded"
        );
        assert_eq!(
            fan.last_duty(),
            initial_duty,
            "disabled fan must not receive a duty write"
        );
        let statuses = app.fan_curve_statuses.lock().await;
        assert!(
            !statuses.contains_key("fan_0"),
            "disabled fan must not appear in fan_curve_statuses"
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
        mark_active_state(
            &app,
            "fan_0",
            halod_protocol::types::VisibilityState::Hidden,
        )
        .await;
        let initial_duty = fan.last_duty();

        let engine = FanCurveEngine::new(app);
        engine.tick(75).await;

        assert_eq!(
            fan.last_duty(),
            initial_duty,
            "hidden fan must not receive a duty write"
        );
    }

    // --- stall detection ---

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
        let device: Arc<dyn crate::drivers::Device> = fan;
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
        let device: Arc<dyn crate::drivers::Device> = fan;
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
        let device: Arc<dyn crate::drivers::Device> = fan;
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
        let device: Arc<dyn crate::drivers::Device> = fan;
        // target = 10% ≤ 20% → not considered a stall
        let status = engine.check_stall("fan_0", &device, 10.0).await;
        assert_eq!(status, FanCurveStatus::Ok);
        assert!(engine.stall_state.lock().unwrap().get("fan_0").is_none());
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
        let device: Arc<dyn crate::drivers::Device> = fan;
        let status = engine.check_stall("fan_0", &device, 50.0).await;
        assert_eq!(status, FanCurveStatus::FanStalled);
        // notified flag must stay true
        assert!(engine.stall_state.lock().unwrap()["fan_0"].notified);
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
        let device: Arc<dyn crate::drivers::Device> = fan;
        let status = engine.check_stall("fan_0", &device, 50.0).await;
        assert_eq!(status, FanCurveStatus::Ok);
        assert!(engine.stall_state.lock().unwrap().get("fan_0").is_none());
    }

    // --- pump path ---

    struct MockPump {
        id: &'static str,
        duty: StdMutex<u8>,
        fan: FanStateSlot,
    }

    impl MockPump {
        fn new_with_curve(id: &'static str, record: FanCurveRecord) -> Arc<Self> {
            let p = Arc::new(Self {
                id,
                duty: StdMutex::new(50),
                fan: FanStateSlot::default(),
            });
            p.fan.set_fan_curve(record);
            p
        }

        fn last_duty(&self) -> u8 {
            *self.duty.lock().unwrap()
        }
    }

    #[async_trait]
    impl Device for MockPump {
        fn id(&self) -> String {
            self.id.to_string()
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
            vec![CapabilityRef::Fan(self)]
        }
    }

    #[async_trait]
    impl FanCapability for MockPump {
        async fn get_duty(&self) -> anyhow::Result<u8> {
            Ok(*self.duty.lock().unwrap())
        }
        async fn set_duty(&self, duty: u8) -> anyhow::Result<()> {
            *self.duty.lock().unwrap() = duty;
            Ok(())
        }
        async fn get_rpm(&self) -> Option<u32> {
            None
        }
        fn fan_state(&self) -> &FanStateSlot {
            &self.fan
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
        *app.devices.try_lock().unwrap() =
            vec![pump.clone() as Arc<dyn Device>, sensor as Arc<dyn Device>];
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
        *app.devices.try_lock().unwrap() = vec![pump.clone() as Arc<dyn Device>];
        let engine = FanCurveEngine::new(app);
        let device: Arc<dyn crate::drivers::Device> = pump;
        let status = engine.check_stall("pump_0", &device, 60.0).await;
        assert_eq!(
            status,
            FanCurveStatus::Ok,
            "pump with no RPM skips stall check"
        );
    }
}
