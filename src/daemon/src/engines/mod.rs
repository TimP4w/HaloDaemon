pub mod canvas;
pub mod fan_curve;
pub mod lcd;
pub mod action_executor;
pub mod key_remap;
pub mod focus_watcher;

use std::time::Duration;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use crate::state::EngineRunConfig;

/// Shared outer watch-loop + inner interval-tick pattern used by all three engines.
///
/// The caller provides a `tick_fn` that is called once per interval tick.
/// The loop exits when `cfg_rx` is closed.
pub async fn engine_run_loop<F, Fut>(
    engine_name: &'static str,
    mut cfg_rx: watch::Receiver<EngineRunConfig>,
    missed_tick: MissedTickBehavior,
    mut tick_fn: F,
) where
    F: FnMut(EngineRunConfig) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    log::info!("Starting {engine_name} engine");
    loop {
        let cfg = cfg_rx.borrow_and_update().clone();
        if !cfg.enabled {
            log::info!("[{engine_name}] Engine disabled, waiting for re-enable");
            cfg_rx.changed().await.ok();
            continue;
        }
        let mut interval = tokio::time::interval(Duration::from_millis(cfg.tick_ms));
        interval.set_missed_tick_behavior(missed_tick);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let cfg = cfg_rx.borrow().clone();
                    tick_fn(cfg).await;
                }
                _ = cfg_rx.changed() => { break; }
            }
        }
    }
}
