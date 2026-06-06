use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use super::require_device_owned;
use crate::state::AppState;

pub async fn set_fan_speed(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let fan = device
        .as_fan()
        .ok_or_else(|| anyhow::anyhow!("device does not support fan control"))?;
    let duty = msg["duty"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing or invalid duty"))? as u8;
    fan.set_duty(duty).await?;
    super::persist_device_state(&app, device.as_ref()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    use crate::config::Config;
    use crate::drivers::{CapabilityRef, Device, FanCapability, FanStateSlot};

    // --- helpers ---

    fn make_app(devices: Vec<Arc<dyn Device>>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        // Populate synchronously before the async runtime touches it.
        *app.devices.try_lock().unwrap() = devices;
        app
    }

    // A device with no fan capability.
    struct NoFanDevice;

    #[async_trait]
    impl Device for NoFanDevice {
        fn id(&self) -> String {
            "no_fan".into()
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
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> { vec![] }
    }

    // A device that has fan capability; records the last duty passed to set_duty.
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
        fn id(&self) -> String {
            "fan_dev".into()
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
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> { vec![CapabilityRef::Fan(self)] }
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
        fn fan_state(&self) -> &FanStateSlot { &self.fan }
    }

    // --- tests ---

    #[tokio::test]
    async fn set_fan_speed_calls_set_duty() {
        let fan = FanDevice::new();
        let app = make_app(vec![fan.clone() as Arc<dyn Device>]);
        let msg = json!({"id": "fan_dev", "duty": 75});
        set_fan_speed(msg, app).await.unwrap();
        assert_eq!(fan.last_duty(), Some(75));
    }

    #[tokio::test]
    async fn set_fan_speed_errors_when_device_not_found() {
        let app = make_app(vec![]);
        let msg = json!({"id": "ghost", "duty": 50});
        assert!(set_fan_speed(msg, app).await.is_err());
    }

    #[tokio::test]
    async fn set_fan_speed_errors_when_device_has_no_fan_capability() {
        let app = make_app(vec![Arc::new(NoFanDevice) as Arc<dyn Device>]);
        let msg = json!({"id": "no_fan", "duty": 50});
        let err = set_fan_speed(msg, app).await.unwrap_err();
        assert!(err.to_string().contains("fan control"));
    }

    #[tokio::test]
    async fn set_fan_speed_errors_when_duty_missing() {
        let fan = FanDevice::new();
        let app = make_app(vec![fan.clone() as Arc<dyn Device>]);
        let msg = json!({"id": "fan_dev"});
        assert!(set_fan_speed(msg, app).await.is_err());
    }
}
