// SPDX-License-Identifier: GPL-3.0-or-later
use crate::domain::events::ChangeSink as _;

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::registry::require_device_owned_id;

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
    app.record_change(crate::domain::events::Change::Device(id))
        .await;
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
    app.record_change(crate::domain::events::Change::Device(id))
        .await;
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
    app.record_change(crate::domain::events::Change::Device(id))
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_with, MockDevice};
    use std::sync::Arc;

    #[test]
    fn validate_slot_accepts_valid_slot() {
        assert!(validate_slot(3).is_ok());
    }

    #[test]
    fn validate_slot_rejects_zero() {
        assert!(validate_slot(0).is_err());
    }

    fn setup() -> (Arc<MockDevice>, Arc<AppState>) {
        let dev = Arc::new(MockDevice::new("mouse0").with_onboard());
        (dev.clone(), app_with(vec![dev]))
    }

    // -- handler tests --

    #[tokio::test]
    async fn switch_onboard_profile_calls_capability() {
        let (dev, app) = setup();
        switch_onboard_profile("mouse0".into(), 2, app)
            .await
            .unwrap();
        assert_eq!(dev.calls(), ["switch 2"]);
    }

    #[tokio::test]
    async fn switch_onboard_profile_rejects_slot_zero() {
        let (dev, app) = setup();
        let err = switch_onboard_profile("mouse0".into(), 0, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("slot"));
        assert!(dev.calls().is_empty());
    }

    #[tokio::test]
    async fn restore_onboard_profile_calls_capability() {
        let (dev, app) = setup();
        restore_onboard_profile("mouse0".into(), 3, app)
            .await
            .unwrap();
        assert_eq!(dev.calls(), ["restore 3"]);
    }

    #[tokio::test]
    async fn set_onboard_profile_enabled_calls_capability() {
        let (dev, app) = setup();
        set_onboard_profile_enabled("mouse0".into(), 1, false, app)
            .await
            .unwrap();
        assert_eq!(dev.calls(), ["enable 1 false"]);
    }

    #[tokio::test]
    async fn switch_onboard_profile_errors_on_missing_device() {
        let app = app_with(vec![]);
        let err = switch_onboard_profile("ghost".into(), 1, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }
}
