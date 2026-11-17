// SPDX-License-Identifier: GPL-3.0-or-later
use std::time::Duration;
use tokio::sync::broadcast;

use crate::application::bus::data_bus::DataBus;
use halod_shared::bus::{BusTransaction, BusValue};

/// An engine's effective configuration, read from its retained bus record.
pub trait EngineConfig: Clone + Default + Send + 'static {
    const TOPIC: &'static str;
    fn enabled(&self) -> bool;
    fn tick_ms(&self) -> u64;
    fn from_bus(value: &BusValue) -> Option<Self>;
}

/// A daemon-side typed subscription to an effective configuration record.
/// Lag recovery reads a fresh retained snapshot, exactly like IPC clients.
pub struct EngineConfigReceiver<C> {
    bus: std::sync::Arc<DataBus>,
    transactions: broadcast::Receiver<BusTransaction>,
    current: C,
}

impl<C: EngineConfig> EngineConfigReceiver<C> {
    pub fn new(bus: std::sync::Arc<DataBus>) -> Self {
        let transactions = bus.subscribe_transactions();
        let current = Self::read_snapshot(&bus).unwrap_or_default();
        Self {
            bus,
            transactions,
            current,
        }
    }

    pub fn current(&self) -> C {
        self.current.clone()
    }

    pub async fn changed(&mut self) -> bool {
        loop {
            match self.transactions.recv().await {
                Ok(transaction) => {
                    if let Some(config) = transaction
                        .upserts
                        .iter()
                        .find_map(|record| C::from_bus(&record.value))
                    {
                        self.current = config;
                        return true;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if let Some(config) = Self::read_snapshot(&self.bus) {
                        self.current = config;
                        return true;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    }

    fn read_snapshot(bus: &DataBus) -> Option<C> {
        bus.state_snapshot(&[C::TOPIC.into()])
            .records
            .iter()
            .find_map(|record| C::from_bus(&record.value))
    }
}

/// Interval for an fps setting, bounded to 1..=240 fps.
pub fn fps_tick_ms(fps: u32) -> u64 {
    1000 / fps.clamp(1, 240) as u64
}
use std::future::Future;
use tokio::time::MissedTickBehavior;

/// An engine driven by [`engine_run_loop_idle`]: while enabled but `has_work`
/// returns false it parks on `wait_for_work` (and config changes) instead of
/// ticking, so there is zero work when there is nothing to do.
pub trait IdleEngine: Send + 'static {
    type Config: EngineConfig;
    fn tick(&mut self, cfg: Self::Config) -> impl Future<Output = ()> + Send;
    fn wait_for_work(&mut self) -> impl Future<Output = ()> + Send;
    fn has_work(&mut self) -> impl Future<Output = bool> + Send;
    fn on_disabled(&mut self) -> impl Future<Output = ()> + Send;
}

pub async fn engine_run_loop_idle<E: IdleEngine>(
    engine_name: &'static str,
    mut cfg_rx: EngineConfigReceiver<E::Config>,
    missed_tick: MissedTickBehavior,
    mut engine: E,
) {
    log::info!("Starting {engine_name} engine");
    loop {
        let cfg = cfg_rx.current();
        if !cfg.enabled() {
            log::info!("{engine_name} engine disabled, waiting for re-enable");
            engine.on_disabled().await;
            if !cfg_rx.changed().await {
                break;
            }
            continue;
        }
        // Enabled but nothing to do: park until woken or reconfigured.
        if !engine.has_work().await {
            tokio::select! {
                () = engine.wait_for_work() => {}
                r = cfg_rx.changed() => if !r { break; },
            }
            continue;
        }
        let mut interval = tokio::time::interval(Duration::from_millis(cfg.tick_ms()));
        interval.set_missed_tick_behavior(missed_tick);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let cfg = cfg_rx.current();
                    engine.tick(cfg).await;
                    if !engine.has_work().await { break; }
                }
                r = cfg_rx.changed() => {
                    if !r { return; }
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CoolingConfig, LcdConfig};
    use halod_shared::bus::topic;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::Notify;

    /// Poll `f` until it returns true, or panic after `timeout` — avoids the
    /// flakiness of sleep-then-assert when the scheduler is under load.
    async fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) {
        tokio::time::timeout(timeout, async {
            while !f() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("condition not met within timeout");
    }

    struct Probe {
        ticks: Arc<AtomicUsize>,
        has_work: Arc<AtomicBool>,
        wake: Arc<Notify>,
    }

    impl IdleEngine for Probe {
        type Config = LcdConfig;

        async fn tick(&mut self, _cfg: LcdConfig) {
            self.ticks.fetch_add(1, Ordering::SeqCst);
        }

        async fn wait_for_work(&mut self) {
            self.wake.notified().await;
        }

        async fn has_work(&mut self) -> bool {
            self.has_work.load(Ordering::SeqCst)
        }

        async fn on_disabled(&mut self) {}
    }

    struct Pacer {
        frames: Arc<parking_lot::Mutex<Vec<tokio::time::Instant>>>,
    }

    impl IdleEngine for Pacer {
        type Config = LcdConfig;

        async fn tick(&mut self, _cfg: LcdConfig) {
            self.frames.lock().push(tokio::time::Instant::now());
        }

        async fn wait_for_work(&mut self) {
            std::future::pending::<()>().await;
        }

        async fn has_work(&mut self) -> bool {
            true
        }

        async fn on_disabled(&mut self) {}
    }

    #[tokio::test]
    async fn idle_gate_parks_until_woken_then_stops_when_work_gone() {
        let bus = std::sync::Arc::new(DataBus::default());
        bus.commit_state(
            "host.state",
            vec![(
                topic::LCD.into(),
                BusValue::Lcd(halod_shared::types::LcdState {
                    config: crate::config::LcdConfig {
                        enabled: true,
                        fps: 200,
                    },
                    ..Default::default()
                }),
            )],
            Vec::new(),
        )
        .unwrap();
        let rx = EngineConfigReceiver::<LcdConfig>::new(bus);
        let ticks = Arc::new(AtomicUsize::new(0));
        let has_work = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());

        let probe = Probe {
            ticks: ticks.clone(),
            has_work: has_work.clone(),
            wake: wake.clone(),
        };
        let handle = tokio::spawn(async move {
            engine_run_loop_idle("test", rx, MissedTickBehavior::Skip, probe).await;
        });

        // Enabled but no work → parked, no ticks.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(ticks.load(Ordering::SeqCst), 0, "must not tick while idle");

        // Work appears + wake → ticking starts.
        has_work.store(true, Ordering::SeqCst);
        wake.notify_one();
        wait_until(Duration::from_secs(5), || ticks.load(Ordering::SeqCst) > 0).await;

        // Work removed → ticking stops (allow one trailing tick, then it parks).
        has_work.store(false, Ordering::SeqCst);
        // Wait for ticks to stop advancing: two reads equal across a gap wider
        // than tick_ms, so a still-active loop would have ticked in between.
        let gap = Duration::from_millis(fps_tick_ms(200) * 4);
        let settled = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let before = ticks.load(Ordering::SeqCst);
                tokio::time::sleep(gap).await;
                if ticks.load(Ordering::SeqCst) == before {
                    return before;
                }
            }
        })
        .await
        .expect("ticking never settled");
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            settled,
            "must re-park when work is gone"
        );

        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn frame_intervals_stay_within_one_tick_of_the_period_over_1000_frames() {
        let bus = std::sync::Arc::new(DataBus::default());
        let fps = 60;
        bus.commit_state(
            "host.state",
            vec![(
                topic::LCD.into(),
                BusValue::Lcd(halod_shared::types::LcdState {
                    config: crate::config::LcdConfig { enabled: true, fps },
                    ..Default::default()
                }),
            )],
            Vec::new(),
        )
        .unwrap();
        let rx = EngineConfigReceiver::<LcdConfig>::new(bus);
        let frames = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&frames);
        let engine = tokio::spawn(engine_run_loop_idle(
            "pacing",
            rx,
            MissedTickBehavior::Skip,
            Pacer { frames: recorded },
        ));
        while frames.lock().len() < 1001 {
            tokio::time::sleep(Duration::from_millis(fps_tick_ms(fps))).await;
        }
        engine.abort();
        let period = Duration::from_millis(fps_tick_ms(fps));
        let tick = Duration::from_millis(1);
        let frames = frames.lock();
        for pair in frames[..1001].windows(2) {
            let interval = pair[1] - pair[0];
            assert!(
                interval + tick >= period && interval <= period + tick,
                "{interval:?} outside {period:?} ± {tick:?}"
            );
        }
    }

    #[tokio::test]
    async fn engine_config_receiver_tracks_effective_bus_records() {
        let bus = std::sync::Arc::new(DataBus::default());
        let initial = crate::config::CoolingConfig {
            fan_curve_enabled: false,
            fan_curve_tick_ms: 900,
            fan_failsafe_duty: 70,
            ..Default::default()
        };
        bus.commit_state(
            "host.state",
            vec![(
                topic::COOLING.into(),
                BusValue::Cooling(halod_shared::types::CoolingState {
                    config: initial,
                    ..Default::default()
                }),
            )],
            Vec::new(),
        )
        .unwrap();
        let mut receiver = EngineConfigReceiver::<CoolingConfig>::new(bus.clone());
        assert!(!receiver.current().fan_curve_enabled);
        assert_eq!(receiver.current().fan_failsafe_duty, 70);

        let updated = crate::config::CoolingConfig {
            fan_curve_enabled: true,
            fan_curve_tick_ms: 1200,
            fan_failsafe_duty: 85,
            ..Default::default()
        };
        bus.commit_state(
            "host.state",
            vec![(
                topic::COOLING.into(),
                BusValue::Cooling(halod_shared::types::CoolingState {
                    config: updated,
                    ..Default::default()
                }),
            )],
            Vec::new(),
        )
        .unwrap();

        assert!(receiver.changed().await);
        assert!(receiver.current().fan_curve_enabled);
        assert_eq!(receiver.current().fan_curve_tick_ms, 1200);
        assert_eq!(receiver.current().fan_failsafe_duty, 85);
    }
}
