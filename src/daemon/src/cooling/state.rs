// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;
use std::sync::OnceLock;
use tokio::sync::{watch, Mutex};

use crate::run_loop::EngineRunConfig;
use halod_shared::types::FanCurveStatus;

struct Engine {
    cfg_tx: watch::Sender<EngineRunConfig>,
    /// Failsafe duty sent to the engine independently of the run config.
    failsafe_duty_tx: watch::Sender<u8>,
}

/// The fan-curve engine handle plus its runtime config/failsafe channels and
/// the per-fan status cache the serializer reads. The engine is set once at
/// startup; `statuses` lives outside the `OnceLock` since the engine writes
/// it continuously once running.
pub struct CoolingEngineState {
    /// Per-fan curve status written by the engine, read by the serializer.
    pub statuses: Mutex<HashMap<String, FanCurveStatus>>,
    engine: OnceLock<Engine>,
}

impl CoolingEngineState {
    pub fn new() -> Self {
        Self {
            statuses: Mutex::new(HashMap::new()),
            engine: OnceLock::new(),
        }
    }

    pub fn set_engine(
        &self,
        cfg_tx: watch::Sender<EngineRunConfig>,
        failsafe_duty_tx: watch::Sender<u8>,
    ) {
        let _ = self.engine.set(Engine {
            cfg_tx,
            failsafe_duty_tx,
        });
    }

    pub fn cfg_tx(&self) -> Option<&watch::Sender<EngineRunConfig>> {
        self.engine.get().map(|e| &e.cfg_tx)
    }

    pub fn failsafe_duty_tx(&self) -> Option<&watch::Sender<u8>> {
        self.engine.get().map(|e| &e.failsafe_duty_tx)
    }

    /// Join device-collected fan curve records with the engine's live statuses.
    pub async fn snapshot(
        &self,
        fan_curves: Vec<(String, String, crate::cooling::config::FanCurveRecord)>,
    ) -> halod_shared::types::CoolingState {
        let statuses = self.statuses.lock().await;
        let fan_curves = fan_curves
            .into_iter()
            .map(|(device_id, channel_id, record)| {
                let key = curve_key(&device_id, &channel_id);
                let status = statuses
                    .get(&key)
                    .cloned()
                    .unwrap_or(FanCurveStatus::NoSensor);
                record.serialize(device_id, channel_id, status)
            })
            .collect();
        drop(statuses);

        halod_shared::types::CoolingState {
            fan_curves,
            preset_curves: crate::cooling::fan_curve::preset_curves()
                .iter()
                .map(|p| p.serialize())
                .collect(),
            // Overwritten by the serializer from the persisted config.
            config: Default::default(),
        }
    }
}

/// Collision-free key for the internal status map. Device and channel IDs are
/// external strings, so a visible delimiter is not safe here.
pub fn curve_key(device_id: &str, channel_id: &str) -> String {
    format!("{}:{device_id}{channel_id}", device_id.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cooling::config::FanCurveRecord;

    #[tokio::test]
    async fn snapshot_joins_fan_curve_with_status() {
        let state = CoolingEngineState::new();
        state
            .statuses
            .lock()
            .await
            .insert(curve_key("fan_dev", "default"), FanCurveStatus::NoSensor);
        let fan_curves = vec![(
            "fan_dev".to_string(),
            "default".to_string(),
            FanCurveRecord {
                sensor_id: Some("cpu_temp".to_string()),
                points: vec![(0.0, 30.0), (80.0, 90.0), (100.0, 100.0)],
            },
        )];

        let wire = state.snapshot(fan_curves).await;

        assert_eq!(wire.fan_curves.len(), 1);
        assert_eq!(wire.fan_curves[0].fan_id, "fan_dev");
        assert_eq!(wire.fan_curves[0].device_id, "fan_dev");
        assert_eq!(wire.fan_curves[0].channel_id, "default");
        assert_eq!(wire.fan_curves[0].sensor_id.as_deref(), Some("cpu_temp"));
    }
}
