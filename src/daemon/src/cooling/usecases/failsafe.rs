use anyhow::Result;
use std::sync::Arc;

use crate::run_loop::EngineRunConfig;
use crate::state::AppState;

pub async fn set_fan_failsafe_duty(duty: u8, app: Arc<AppState>) -> Result<()> {
    let duty = duty.min(100);

    let cfg_snap = {
        let mut cfg = app.config.write().await;
        cfg.global.fan_failsafe_duty = duty;
        cfg.global.clone()
    };
    app.request_config_save();

    if let Some(tx) = app.cooling.cfg_tx() {
        let _ = tx.send(EngineRunConfig::fan_curve(&cfg_snap));
    }
    if let Some(tx) = app.cooling.failsafe_duty_tx() {
        let _ = tx.send(duty);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tmp_config;

    #[tokio::test]
    async fn set_fan_failsafe_duty_clamps_to_100() {
        with_tmp_config(|app| async move {
            set_fan_failsafe_duty(200, app.clone()).await.unwrap();
            assert_eq!(app.config.read().await.global.fan_failsafe_duty, 100);
        })
        .await;
    }
}
