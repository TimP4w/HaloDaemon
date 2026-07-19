// SPDX-License-Identifier: GPL-3.0-or-later
use std::time::Duration;
use tokio::sync::broadcast;

use crate::services::data_bus::DataBus;
use halod_shared::bus::{topic, BusTransaction, BusValue};

/// Runtime configuration sent to each engine via a watch channel.
#[derive(Debug, Clone)]
pub struct EngineRunConfig {
    pub enabled: bool,
    /// Interval in milliseconds (engines convert fps → ms themselves).
    pub tick_ms: u64,
    pub failsafe_duty: Option<u8>,
}

impl EngineRunConfig {
    /// Run config for the fan-curve engine from cooling settings.
    pub fn fan_curve(c: &crate::config::CoolingConfig) -> Self {
        Self {
            enabled: c.fan_curve_enabled,
            tick_ms: c.fan_curve_tick_ms,
            failsafe_duty: Some(c.fan_failsafe_duty),
        }
    }

    /// Run config for the canvas engine (fps → ms).
    pub fn canvas(c: &crate::config::RgbConfig) -> Self {
        Self {
            enabled: c.canvas_enabled,
            tick_ms: 1000 / c.canvas_fps.clamp(1, 240) as u64,
            failsafe_duty: None,
        }
    }

    /// Run config for the LCD engine (fps → ms).
    pub fn lcd(c: &crate::config::LcdConfig) -> Self {
        Self {
            enabled: c.enabled,
            tick_ms: 1000 / c.fps.clamp(1, 240) as u64,
            failsafe_duty: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum EngineConfigTopic {
    Cooling,
    Lighting,
    Lcd,
}

/// A daemon-side typed subscription to an effective configuration record.
/// Lag recovery reads a fresh retained snapshot, exactly like IPC clients.
pub struct EngineConfigReceiver {
    bus: std::sync::Arc<DataBus>,
    topic: EngineConfigTopic,
    transactions: broadcast::Receiver<BusTransaction>,
    current: EngineRunConfig,
}

impl EngineConfigReceiver {
    pub fn new(bus: std::sync::Arc<DataBus>, topic: EngineConfigTopic) -> Self {
        let transactions = bus.subscribe_transactions();
        let current = Self::read_snapshot(&bus, topic).unwrap_or_else(|| match topic {
            EngineConfigTopic::Cooling => EngineRunConfig::fan_curve(&Default::default()),
            EngineConfigTopic::Lighting => EngineRunConfig::canvas(&Default::default()),
            EngineConfigTopic::Lcd => EngineRunConfig::lcd(&Default::default()),
        });
        Self {
            bus,
            topic,
            transactions,
            current,
        }
    }

    pub fn current(&self) -> EngineRunConfig {
        self.current.clone()
    }

    pub async fn changed(&mut self) -> bool {
        loop {
            match self.transactions.recv().await {
                Ok(transaction) => {
                    if let Some(config) = transaction
                        .upserts
                        .iter()
                        .find_map(|record| Self::from_value(self.topic, &record.value))
                    {
                        self.current = config;
                        return true;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if let Some(config) = Self::read_snapshot(&self.bus, self.topic) {
                        self.current = config;
                        return true;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    }

    fn key(topic: EngineConfigTopic) -> &'static str {
        match topic {
            EngineConfigTopic::Cooling => topic::COOLING,
            EngineConfigTopic::Lighting => topic::LIGHTING,
            EngineConfigTopic::Lcd => topic::LCD,
        }
    }

    fn read_snapshot(bus: &DataBus, topic_kind: EngineConfigTopic) -> Option<EngineRunConfig> {
        bus.state_snapshot(&[Self::key(topic_kind).into()])
            .records
            .iter()
            .find_map(|record| Self::from_value(topic_kind, &record.value))
    }

    fn from_value(topic: EngineConfigTopic, value: &BusValue) -> Option<EngineRunConfig> {
        match (topic, value) {
            (EngineConfigTopic::Cooling, BusValue::Cooling(value)) => {
                Some(EngineRunConfig::fan_curve(&value.config))
            }
            (EngineConfigTopic::Lighting, BusValue::Lighting(value)) => {
                Some(EngineRunConfig::canvas(&value.config))
            }
            (EngineConfigTopic::Lcd, BusValue::Lcd(value)) => {
                Some(EngineRunConfig::lcd(&value.config))
            }
            _ => None,
        }
    }
}
use tokio::time::MissedTickBehavior;

/// Shared outer watch-loop + inner interval-tick pattern used by all engines.
/// `tick_fn` runs once per tick; the loop exits when `cfg_rx` closes.
pub async fn engine_run_loop<F, Fut>(
    engine_name: &'static str,
    cfg_rx: EngineConfigReceiver,
    missed_tick: MissedTickBehavior,
    tick_fn: F,
) where
    F: FnMut(EngineRunConfig) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    // No idle gate: always has work, so `wait_for_work` is never awaited.
    engine_run_loop_idle(
        engine_name,
        cfg_rx,
        missed_tick,
        tick_fn,
        std::future::pending::<()>,
        || std::future::ready(true),
    )
    .await
}

/// `engine_run_loop` with an extra idle gate: while enabled but `has_work`
/// returns false, the engine parks on `wait_for_work` (and config changes)
/// instead of ticking — zero work when there is nothing to do. `wait_for_work`
/// is only awaited in that idle state, so non-idling engines pass a future that
/// never resolves.
pub async fn engine_run_loop_idle<F, Fut, W, Wut, H, Hut>(
    engine_name: &'static str,
    mut cfg_rx: EngineConfigReceiver,
    missed_tick: MissedTickBehavior,
    mut tick_fn: F,
    mut wait_for_work: W,
    mut has_work: H,
) where
    F: FnMut(EngineRunConfig) -> Fut,
    Fut: std::future::Future<Output = ()>,
    W: FnMut() -> Wut,
    Wut: std::future::Future<Output = ()>,
    H: FnMut() -> Hut,
    Hut: std::future::Future<Output = bool>,
{
    log::info!("Starting {engine_name} engine");
    loop {
        let cfg = cfg_rx.current();
        if !cfg.enabled {
            log::info!("[{engine_name}] Engine disabled, waiting for re-enable");
            if !cfg_rx.changed().await {
                break;
            }
            continue;
        }
        // Enabled but nothing to do: park until woken or reconfigured.
        if !has_work().await {
            tokio::select! {
                _ = wait_for_work() => {}
                r = cfg_rx.changed() => if !r { break; },
            }
            continue;
        }
        let mut interval = tokio::time::interval(Duration::from_millis(cfg.tick_ms));
        interval.set_missed_tick_behavior(missed_tick);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let cfg = cfg_rx.current();
                    tick_fn(cfg).await;
                    if !has_work().await { break; }
                }
                _ = cfg_rx.changed() => { break; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::Notify;

    fn cfg(enabled: bool) -> EngineRunConfig {
        EngineRunConfig {
            enabled,
            tick_ms: 5,
            failsafe_duty: None,
        }
    }

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
                        ..Default::default()
                    },
                    ..Default::default()
                }),
            )],
            Vec::new(),
        )
        .unwrap();
        let rx = EngineConfigReceiver::new(bus, EngineConfigTopic::Lcd);
        let ticks = Arc::new(AtomicUsize::new(0));
        let has_work = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());

        let (ticks_c, has_work_c, wake_c) = (ticks.clone(), has_work.clone(), wake.clone());
        let handle = tokio::spawn(async move {
            engine_run_loop_idle(
                "test",
                rx,
                MissedTickBehavior::Skip,
                move |_| {
                    let t = ticks_c.clone();
                    async move {
                        t.fetch_add(1, Ordering::SeqCst);
                    }
                },
                move || {
                    let w = wake_c.clone();
                    async move { w.notified().await }
                },
                move || {
                    let h = has_work_c.clone();
                    async move { h.load(Ordering::SeqCst) }
                },
            )
            .await;
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
        let gap = Duration::from_millis(cfg(true).tick_ms * 4);
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
        let mut receiver = EngineConfigReceiver::new(bus.clone(), EngineConfigTopic::Cooling);
        assert!(!receiver.current().enabled);
        assert_eq!(receiver.current().failsafe_duty, Some(70));

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
        assert!(receiver.current().enabled);
        assert_eq!(receiver.current().tick_ms, 1200);
        assert_eq!(receiver.current().failsafe_duty, Some(85));
    }
}
