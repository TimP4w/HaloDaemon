use anyhow::{Context, Result};
use std::sync::Arc;

use crate::drivers::Device;
use crate::ipc::broadcast_state;
use crate::profiles::device_state::persist_device_state;
use crate::registry::require_device_owned_id;
use crate::state::AppState;

/// Wire-level parameters for the "look up a device, apply a value to one
/// capability, maybe persist/broadcast" family of usecases. One variant per
/// `DaemonCommand` setter; the differences between them (validation, whether
/// the result persists or broadcasts) live in [`apply`].
pub enum CapabilityParam {
    Boolean { key: String, value: bool },
    Range { key: String, value: i32 },
    Choice { key: String, selected: usize },
    Action { key: String },
    DpiSteps { steps: Vec<u32> },
    FanDuty { duty: u8 },
    EqPreset { preset_index: usize },
    EqBands { values: Vec<f32> },
}

struct Effects {
    persist: bool,
    broadcast: bool,
}

pub async fn set_capability_param(
    id: String,
    param: CapabilityParam,
    app: Arc<AppState>,
) -> Result<()> {
    let dev = require_device_owned_id(&id, &app).await?;
    let effects = apply(dev.as_ref(), &param).await?;
    if effects.persist {
        persist_device_state(&app, dev.as_ref()).await;
    }
    if effects.broadcast {
        broadcast_state(&app).await;
    }
    Ok(())
}

async fn apply(dev: &dyn Device, param: &CapabilityParam) -> Result<Effects> {
    match param {
        CapabilityParam::Boolean { key, value } => {
            let cap = dev
                .as_boolean()
                .context("device does not support boolean control")?;
            let booleans = cap.get_booleans().await?;
            if let Some(b) = booleans.iter().find(|b| &b.key == key) {
                if b.read_only {
                    anyhow::bail!("boolean '{key}' is read-only");
                }
            }
            cap.set_boolean(key, *value).await?;
            Ok(Effects {
                persist: true,
                broadcast: true,
            })
        }
        CapabilityParam::Range { key, value } => {
            dev.as_range()
                .context("device does not support range control")?
                .set_range(key, *value)
                .await?;
            Ok(Effects {
                persist: true,
                broadcast: false,
            })
        }
        CapabilityParam::Choice { key, selected } => {
            dev.as_choice()
                .context("device does not support choice control")?
                .set_choice(key, *selected)
                .await?;
            Ok(Effects {
                persist: true,
                broadcast: false,
            })
        }
        CapabilityParam::Action { key } => {
            dev.as_action()
                .context("device does not support actions")?
                .trigger_action(key)
                .await?;
            Ok(Effects {
                persist: false,
                broadcast: false,
            })
        }
        CapabilityParam::DpiSteps { steps } => {
            let steps16 = steps
                .iter()
                .map(|&s| {
                    u16::try_from(s)
                        .map_err(|_| anyhow::anyhow!("DPI step {s} exceeds maximum value 65535"))
                })
                .collect::<Result<Vec<u16>>>()?;
            dev.as_dpi()
                .context("device does not support DPI control")?
                .set_dpi_steps(steps16)
                .await?;
            Ok(Effects {
                persist: false,
                broadcast: false,
            })
        }
        CapabilityParam::FanDuty { duty } => {
            if *duty > 100 {
                anyhow::bail!("fan duty must be 0–100 (got {duty})");
            }
            dev.as_fan()
                .context("device does not support fan control")?
                .set_duty(*duty)
                .await?;
            Ok(Effects {
                persist: true,
                broadcast: false,
            })
        }
        CapabilityParam::EqPreset { preset_index } => {
            dev.as_equalizer()
                .context("device does not support equalizer control")?
                .set_eq_preset(*preset_index)
                .await?;
            Ok(Effects {
                persist: true,
                broadcast: false,
            })
        }
        CapabilityParam::EqBands { values } => {
            let cap = dev
                .as_equalizer()
                .context("device does not support equalizer control")?;
            let eq = cap.get_equalizer().await?;
            anyhow::ensure!(
                values.len() == eq.bands.len(),
                "expected {} EQ bands, got {}",
                eq.bands.len(),
                values.len()
            );
            anyhow::ensure!(
                values.iter().all(|v| v.is_finite()),
                "EQ band values must be finite"
            );
            cap.set_eq_bands(values).await?;
            Ok(Effects {
                persist: true,
                broadcast: false,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::{CapabilityRef, FanCapability, FanStateSlot};
    use crate::test_support::MockDevice;
    use async_trait::async_trait;
    use halod_shared::types::{Boolean, EqBand, Equalizer};
    use std::sync::Mutex;

    fn make_app(devices: Vec<Arc<dyn Device>>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        *app.devices.try_write().unwrap() = devices;
        app
    }

    // ── Boolean ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_boolean_calls_capability_for_writable_key() {
        let dev = Arc::new(MockDevice::new("dev1").with_booleans(vec![Boolean {
            key: "sidetone".into(),
            label: "Sidetone".into(),
            value: false,
            read_only: false,
            category: String::new(),
            visible_when: None,
        }]));
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);
        set_capability_param(
            "dev1".into(),
            CapabilityParam::Boolean {
                key: "sidetone".into(),
                value: true,
            },
            app,
        )
        .await
        .unwrap();
        let last = dev
            .bool_last_set
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        assert_eq!(last, ("sidetone".to_string(), true));
    }

    #[tokio::test]
    async fn set_boolean_rejects_read_only_key() {
        let dev = Arc::new(MockDevice::new("dev1").with_booleans(vec![Boolean {
            key: "muted".into(),
            label: "Muted".into(),
            value: true,
            read_only: true,
            category: String::new(),
            visible_when: None,
        }]));
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);
        let err = set_capability_param(
            "dev1".into(),
            CapabilityParam::Boolean {
                key: "muted".into(),
                value: false,
            },
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("read-only"), "got: {err}");
        assert!(
            dev.bool_last_set
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .is_none(),
            "set_boolean should not have been called"
        );
    }

    // ── Range ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_range_calls_capability() {
        let dev = Arc::new(MockDevice::new("dev1").with_range());
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::Range {
                key: "nc_level".into(),
                value: 50,
            },
            app,
        )
        .await
        .unwrap();

        let last = dev
            .range_last_set
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        assert_eq!(last, ("nc_level".to_string(), 50));
    }

    // ── Choice ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_choice_calls_capability() {
        let dev = Arc::new(MockDevice::new("dev1").with_choice());
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::Choice {
                key: "nc_mode".into(),
                selected: 2,
            },
            app,
        )
        .await
        .unwrap();

        let last = dev
            .choice_last_set
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        assert_eq!(last, ("nc_mode".to_string(), 2));
    }

    // ── Action ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn trigger_action_calls_capability() {
        let dev = Arc::new(MockDevice::new("dev1").with_action());
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::Action {
                key: "pixel_refresh".into(),
            },
            app,
        )
        .await
        .unwrap();

        assert_eq!(
            dev.action_last_key
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .as_deref(),
            Some("pixel_refresh")
        );
    }

    // ── DpiSteps ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_dpi_steps_calls_capability() {
        let dev = Arc::new(MockDevice::new("dev1").with_dpi());
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::DpiSteps {
                steps: vec![400, 800, 1600],
            },
            app,
        )
        .await
        .unwrap();

        let recorded = dev.dpi_last_steps.as_ref().unwrap().lock().unwrap().clone();
        assert_eq!(recorded, Some(vec![400u16, 800, 1600]));
    }

    #[tokio::test]
    async fn set_dpi_steps_errors_without_dpi_capability() {
        let dev = Arc::new(MockDevice::new("dev1")); // no .with_dpi()
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        let err = set_capability_param(
            "dev1".into(),
            CapabilityParam::DpiSteps {
                steps: vec![400, 800],
            },
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("DPI"));
    }

    #[tokio::test]
    async fn set_dpi_steps_does_not_persist_device_state() {
        let dev = Arc::new(MockDevice::new("dev1").with_dpi());
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::DpiSteps { steps: vec![1200] },
            app.clone(),
        )
        .await
        .unwrap();

        let cfg = app.config.read().await;
        assert!(!cfg.active_profile_data().device_states.contains_key("dev1"));
    }

    // ── FanDuty ──────────────────────────────────────────────────────────

    struct NoFanDevice;

    #[async_trait]
    impl Device for NoFanDevice {
        fn id(&self) -> &str {
            "no_fan"
        }
        fn name(&self) -> &str {
            "no_fan"
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
            vec![]
        }
    }

    struct FanDevice {
        last_duty: Mutex<Option<u8>>,
        fan: FanStateSlot,
    }

    impl FanDevice {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                last_duty: Mutex::new(None),
                fan: FanStateSlot::default(),
            })
        }
        fn last_duty(&self) -> Option<u8> {
            *self.last_duty.lock().unwrap()
        }
    }

    #[async_trait]
    impl Device for FanDevice {
        fn id(&self) -> &str {
            "fan_dev"
        }
        fn name(&self) -> &str {
            "fan_dev"
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
    impl FanCapability for FanDevice {
        async fn get_duty(&self) -> anyhow::Result<u8> {
            Ok(0)
        }
        async fn set_duty(&self, duty: u8) -> anyhow::Result<()> {
            *self.last_duty.lock().unwrap() = Some(duty);
            Ok(())
        }
        async fn get_rpm(&self) -> Option<u32> {
            Some(0)
        }
        fn fan_state(&self) -> &FanStateSlot {
            &self.fan
        }
    }

    #[tokio::test]
    async fn set_fan_speed_calls_set_duty() {
        let fan = FanDevice::new();
        let app = make_app(vec![fan.clone() as Arc<dyn Device>]);
        set_capability_param("fan_dev".into(), CapabilityParam::FanDuty { duty: 75 }, app)
            .await
            .unwrap();
        assert_eq!(fan.last_duty(), Some(75));
    }

    #[tokio::test]
    async fn set_fan_speed_errors_when_device_not_found() {
        let app = make_app(vec![]);
        assert!(
            set_capability_param("ghost".into(), CapabilityParam::FanDuty { duty: 50 }, app,)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn set_fan_speed_errors_when_device_has_no_fan_capability() {
        let app = make_app(vec![Arc::new(NoFanDevice) as Arc<dyn Device>]);
        let err = set_capability_param("no_fan".into(), CapabilityParam::FanDuty { duty: 50 }, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("fan control"));
    }

    #[tokio::test]
    async fn set_fan_speed_rejects_duty_over_100() {
        let fan = FanDevice::new();
        let app = make_app(vec![fan.clone() as Arc<dyn Device>]);
        let err = set_capability_param(
            "fan_dev".into(),
            CapabilityParam::FanDuty { duty: 150 },
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("0–100"));
        assert_eq!(fan.last_duty(), None);
    }

    // ── Equalizer ────────────────────────────────────────────────────────

    fn make_eq(band_count: usize) -> Equalizer {
        Equalizer {
            presets: vec![],
            selected_preset: 0,
            editable: true,
            bands: (0..band_count)
                .map(|i| EqBand {
                    index: i,
                    label: String::new(),
                    min: -10.0,
                    max: 10.0,
                    step: 0.5,
                    value: 0.0,
                })
                .collect(),
        }
    }

    fn eq_dev(id: &str) -> Arc<MockDevice> {
        Arc::new(MockDevice::new(id).with_equalizer(make_eq(10)))
    }

    #[tokio::test]
    async fn set_eq_preset_calls_capability() {
        let dev = eq_dev("dev1");
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::EqPreset { preset_index: 2 },
            app,
        )
        .await
        .unwrap();

        assert_eq!(
            *dev.eq_last_preset.as_ref().unwrap().lock().unwrap(),
            Some(2)
        );
    }

    #[tokio::test]
    async fn set_eq_preset_persists_state() {
        let dev = eq_dev("dev1");
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        set_capability_param(
            "dev1".into(),
            CapabilityParam::EqPreset { preset_index: 1 },
            app.clone(),
        )
        .await
        .unwrap();

        let cfg = app.config.read().await;
        let device_state = cfg
            .active_profile_data()
            .device_states
            .get("dev1")
            .expect("state must be saved after set_eq_preset");
        assert_eq!(device_state["equalizer"]["preset"].as_u64().unwrap(), 1);
    }

    #[tokio::test]
    async fn set_eq_bands_calls_capability() {
        let dev = eq_dev("dev2");
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        let values = vec![1.0f32; 10];
        set_capability_param("dev2".into(), CapabilityParam::EqBands { values }, app)
            .await
            .unwrap();

        assert!(dev
            .eq_last_bands
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn set_eq_bands_persists_state() {
        let dev = eq_dev("dev2");
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        // set a preset first so current_state is non-null
        set_capability_param(
            "dev2".into(),
            CapabilityParam::EqPreset { preset_index: 4 },
            app.clone(),
        )
        .await
        .unwrap();
        let values = vec![0.5f32; 10];
        set_capability_param(
            "dev2".into(),
            CapabilityParam::EqBands { values },
            app.clone(),
        )
        .await
        .unwrap();

        let cfg = app.config.read().await;
        assert!(
            cfg.active_profile_data().device_states.contains_key("dev2"),
            "state must be saved after set_eq_bands"
        );
    }

    #[tokio::test]
    async fn set_eq_bands_rejects_wrong_count() {
        let dev = eq_dev("dev3");
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        // device reports 10 bands; sending 5 must fail
        let result = set_capability_param(
            "dev3".into(),
            CapabilityParam::EqBands {
                values: vec![0.0f32; 5],
            },
            app,
        )
        .await;
        assert!(result.is_err(), "mismatched band count must be rejected");
    }

    #[tokio::test]
    async fn set_eq_bands_rejects_non_finite() {
        let dev = eq_dev("dev4");
        let app = make_app(vec![dev.clone() as Arc<dyn Device>]);

        let mut values = vec![0.0f32; 10];
        values[3] = f32::NAN;
        let result =
            set_capability_param("dev4".into(), CapabilityParam::EqBands { values }, app).await;
        assert!(result.is_err(), "NaN band value must be rejected");
    }
}
