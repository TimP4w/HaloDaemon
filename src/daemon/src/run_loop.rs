use std::time::Duration;
use tokio::sync::watch;

/// Runtime configuration sent to each engine via a watch channel.
#[derive(Debug, Clone)]
pub struct EngineRunConfig {
    pub enabled: bool,
    /// Interval in milliseconds (engines convert fps → ms themselves).
    pub tick_ms: u64,
}

impl EngineRunConfig {
    /// Run config for the fan-curve engine from global settings.
    pub fn fan_curve(g: &crate::config::GlobalConfig) -> Self {
        Self {
            enabled: g.engine_fan_curve_enabled,
            tick_ms: g.engine_fan_curve_tick_ms,
        }
    }

    /// Run config for the canvas engine (fps → ms).
    pub fn canvas(g: &crate::config::GlobalConfig) -> Self {
        Self {
            enabled: g.engine_canvas_enabled,
            tick_ms: 1000 / g.engine_canvas_fps.clamp(1, 240) as u64,
        }
    }

    /// Run config for the LCD engine (fps → ms).
    pub fn lcd(g: &crate::config::GlobalConfig) -> Self {
        Self {
            enabled: g.engine_lcd_enabled,
            tick_ms: 1000 / g.engine_lcd_fps.clamp(1, 240) as u64,
        }
    }

    /// Run config for the focus-watcher engine.
    pub fn focus_watcher(_g: &crate::config::GlobalConfig) -> Self {
        Self {
            enabled: true,
            tick_ms: 0,
        }
    }
}
use tokio::time::MissedTickBehavior;

/// Shared outer watch-loop + inner interval-tick pattern used by all engines.
/// `tick_fn` runs once per tick; the loop exits when `cfg_rx` closes.
pub async fn engine_run_loop<F, Fut>(
    engine_name: &'static str,
    cfg_rx: watch::Receiver<EngineRunConfig>,
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
    mut cfg_rx: watch::Receiver<EngineRunConfig>,
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
        let cfg = cfg_rx.borrow_and_update().clone();
        if !cfg.enabled {
            log::info!("[{engine_name}] Engine disabled, waiting for re-enable");
            if cfg_rx.changed().await.is_err() {
                break;
            }
            continue;
        }
        // Enabled but nothing to do: park until woken or reconfigured.
        if !has_work().await {
            tokio::select! {
                _ = wait_for_work() => {}
                r = cfg_rx.changed() => if r.is_err() { break; },
            }
            continue;
        }
        let mut interval = tokio::time::interval(Duration::from_millis(cfg.tick_ms));
        interval.set_missed_tick_behavior(missed_tick);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let cfg = cfg_rx.borrow().clone();
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
        }
    }

    #[tokio::test]
    async fn idle_gate_parks_until_woken_then_stops_when_work_gone() {
        let (tx, rx) = watch::channel(cfg(true));
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
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(ticks.load(Ordering::SeqCst) > 0, "should tick once woken");

        // Work removed → ticking stops (allow one trailing tick, then it parks).
        has_work.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let settled = ticks.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            settled,
            "must re-park when work is gone"
        );

        drop(tx);
        let _ = handle.await;
    }
}
