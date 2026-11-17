// SPDX-License-Identifier: GPL-3.0-or-later
use crate::domain::events::ChangeSink as _;

use anyhow::Result;
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::events::Change;

#[derive(Debug, Default, Clone, Copy)]
pub struct CoolingTuning {
    pub ramp_up_pct_per_s: Option<f32>,
    pub ramp_down_pct_per_s: Option<f32>,
    pub hysteresis_c: Option<f32>,
    pub smoothing_secs: Option<f32>,
    pub sensor_stale_secs: Option<f32>,
}

pub async fn set_cooling_tuning(tuning: CoolingTuning, app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        let cooling = &mut cfg.cooling;
        for (value, field) in [
            (tuning.ramp_up_pct_per_s, &mut cooling.fan_ramp_up_pct_per_s),
            (
                tuning.ramp_down_pct_per_s,
                &mut cooling.fan_ramp_down_pct_per_s,
            ),
            (tuning.hysteresis_c, &mut cooling.fan_hysteresis_c),
            (tuning.smoothing_secs, &mut cooling.fan_smoothing_secs),
            (tuning.sensor_stale_secs, &mut cooling.fan_sensor_stale_secs),
        ] {
            if let Some(v) = value {
                *field = v;
            }
        }
        cooling.sanitize();
    }
    app.request_config_save();
    app.record_change(Change::Cooling).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoolingConfig;
    use crate::test_support::with_tmp_config;

    #[tokio::test]
    async fn sets_only_given_fields_and_clamps() {
        with_tmp_config(|app| async move {
            set_cooling_tuning(
                CoolingTuning {
                    ramp_down_pct_per_s: Some(2.5),
                    hysteresis_c: Some(1_000.0),
                    ..Default::default()
                },
                app.clone(),
            )
            .await
            .unwrap();
            let cfg = app.config.read().await.cooling.clone();
            let default = CoolingConfig::default();
            assert_eq!(cfg.fan_ramp_down_pct_per_s, 2.5);
            assert_eq!(cfg.fan_hysteresis_c, CoolingConfig::MAX_HYSTERESIS_C);
            assert_eq!(cfg.fan_ramp_up_pct_per_s, default.fan_ramp_up_pct_per_s);
            assert_eq!(cfg.fan_smoothing_secs, default.fan_smoothing_secs);
        })
        .await;
    }
}
