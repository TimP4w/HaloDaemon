use anyhow::{Context, Result};
use std::sync::Arc;

use crate::ipc::broadcast_state;
use crate::registry::require_device_owned_id;
use crate::state::AppState;

fn validate_slot(slot: u8) -> Result<()> {
    if slot == 0 {
        anyhow::bail!("profile slot must be 1..=255");
    }
    Ok(())
}

pub async fn switch_onboard_profile(id: String, slot: u8, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    validate_slot(slot)?;
    let cap = device
        .as_onboard_profiles()
        .context("device does not support onboard profiles")?;
    log::info!(
        "[OnboardProfile] Switching '{}' to slot {slot}",
        device.name()
    );
    cap.switch_profile(slot).await?;
    broadcast_state(&app).await;
    Ok(())
}

pub async fn restore_onboard_profile(id: String, slot: u8, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    validate_slot(slot)?;
    let cap = device
        .as_onboard_profiles()
        .context("device does not support onboard profiles")?;
    log::info!("[OnboardProfile] Restoring '{}' slot {slot}", device.name());
    cap.restore_profile(slot).await?;
    broadcast_state(&app).await;
    Ok(())
}

pub async fn set_onboard_profile_enabled(
    id: String,
    slot: u8,
    enabled: bool,
    app: Arc<AppState>,
) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    validate_slot(slot)?;
    let cap = device
        .as_onboard_profiles()
        .context("device does not support onboard profiles")?;
    log::info!(
        "[OnboardProfile] '{}' slot {slot} enabled={enabled}",
        device.name()
    );
    cap.set_profile_enabled(slot, enabled).await?;
    broadcast_state(&app).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::{CapabilityRef, Device, OnboardProfilesCapability, VisibilitySlot};
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    #[test]
    fn validate_slot_accepts_valid_slot() {
        assert!(validate_slot(3).is_ok());
    }

    #[test]
    fn validate_slot_rejects_zero() {
        assert!(validate_slot(0).is_err());
    }

    // -- mock --

    #[derive(Default)]
    struct Calls {
        switched: Vec<u8>,
        restored: Vec<u8>,
        enabled: Vec<(u8, bool)>,
    }

    struct MockOnboardDevice {
        id: String,
        visibility: VisibilitySlot,
        calls: Mutex<Calls>,
    }

    impl MockOnboardDevice {
        fn new(id: &str) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_string(),
                visibility: VisibilitySlot::default(),
                calls: Mutex::new(Calls::default()),
            })
        }
    }

    #[async_trait]
    impl Device for MockOnboardDevice {
        fn id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            &self.id
        }
        fn vendor(&self) -> &str {
            "mock"
        }
        fn model(&self) -> &str {
            "mock"
        }
        async fn initialize(&self) -> Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::OnboardProfiles(self)]
        }
        fn visibility_slot(&self) -> Option<&VisibilitySlot> {
            Some(&self.visibility)
        }
    }

    #[async_trait]
    impl OnboardProfilesCapability for MockOnboardDevice {
        async fn switch_profile(&self, slot: u8) -> Result<()> {
            self.calls.lock().unwrap().switched.push(slot);
            Ok(())
        }
        async fn restore_profile(&self, slot: u8) -> Result<()> {
            self.calls.lock().unwrap().restored.push(slot);
            Ok(())
        }
        async fn set_profile_enabled(&self, slot: u8, enabled: bool) -> Result<()> {
            self.calls.lock().unwrap().enabled.push((slot, enabled));
            Ok(())
        }
    }

    fn make_app_with_device(device: Arc<dyn Device>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.try_write().unwrap().push(device);
        app
    }

    // -- handler tests --

    #[tokio::test]
    async fn switch_onboard_profile_calls_capability() {
        let dev = MockOnboardDevice::new("mouse0");
        let app = make_app_with_device(dev.clone());
        switch_onboard_profile("mouse0".into(), 2, app)
            .await
            .unwrap();
        assert_eq!(dev.calls.lock().unwrap().switched, vec![2]);
    }

    #[tokio::test]
    async fn switch_onboard_profile_rejects_slot_zero() {
        let dev = MockOnboardDevice::new("mouse0");
        let app = make_app_with_device(dev.clone());
        let err = switch_onboard_profile("mouse0".into(), 0, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("slot"));
        assert!(dev.calls.lock().unwrap().switched.is_empty());
    }

    #[tokio::test]
    async fn restore_onboard_profile_calls_capability() {
        let dev = MockOnboardDevice::new("mouse0");
        let app = make_app_with_device(dev.clone());
        restore_onboard_profile("mouse0".into(), 3, app)
            .await
            .unwrap();
        assert_eq!(dev.calls.lock().unwrap().restored, vec![3]);
    }

    #[tokio::test]
    async fn set_onboard_profile_enabled_calls_capability() {
        let dev = MockOnboardDevice::new("mouse0");
        let app = make_app_with_device(dev.clone());
        set_onboard_profile_enabled("mouse0".into(), 1, false, app)
            .await
            .unwrap();
        assert_eq!(dev.calls.lock().unwrap().enabled, vec![(1, false)]);
    }

    #[tokio::test]
    async fn switch_onboard_profile_errors_on_missing_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let err = switch_onboard_profile("ghost".into(), 1, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }
}
