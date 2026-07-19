// SPDX-License-Identifier: GPL-3.0-or-later
use crate::domain::events::ChangeSink as _;

use anyhow::Result;
use std::sync::Arc;

use crate::application::state::AppState;

pub async fn set_fan_failsafe_duty(duty: u8, app: Arc<AppState>) -> Result<()> {
    let duty = duty.min(100);

    {
        let mut cfg = app.config.write().await;
        cfg.cooling.fan_failsafe_duty = duty;
    }
    app.request_config_save();

    app.record_change(crate::domain::events::Change::Cooling)
        .await;

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
            assert_eq!(app.config.read().await.cooling.fan_failsafe_duty, 100);
        })
        .await;
    }
}
