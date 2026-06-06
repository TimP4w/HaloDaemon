use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use super::require_device_owned;
use crate::ipc::broadcast_state;
use crate::state::AppState;

fn slot_of(msg: &Value) -> Result<u8> {
    let slot = msg["slot"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing or invalid slot"))?;
    if slot == 0 || slot > 255 {
        anyhow::bail!("profile slot must be 1..=255");
    }
    Ok(slot as u8)
}

pub async fn switch_onboard_profile(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let slot = slot_of(&msg)?;
    let cap = device
        .as_onboard_profiles()
        .ok_or_else(|| anyhow::anyhow!("device does not support onboard profiles"))?;
    log::info!("[OnboardProfile] Switching '{}' to slot {slot}", device.name());
    cap.switch_profile(slot).await?;
    broadcast_state(app).await;
    Ok(())
}

pub async fn restore_onboard_profile(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let slot = slot_of(&msg)?;
    let cap = device
        .as_onboard_profiles()
        .ok_or_else(|| anyhow::anyhow!("device does not support onboard profiles"))?;
    log::info!("[OnboardProfile] Restoring '{}' slot {slot}", device.name());
    cap.restore_profile(slot).await?;
    broadcast_state(app).await;
    Ok(())
}

pub async fn set_onboard_profile_enabled(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let slot = slot_of(&msg)?;
    let cap = device
        .as_onboard_profiles()
        .ok_or_else(|| anyhow::anyhow!("device does not support onboard profiles"))?;
    let enabled = msg["enabled"]
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("missing or invalid enabled"))?;
    log::info!("[OnboardProfile] '{}' slot {slot} enabled={enabled}", device.name());
    cap.set_profile_enabled(slot, enabled).await?;
    broadcast_state(app).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slot_of_accepts_valid_slot() {
        assert_eq!(slot_of(&json!({"slot": 3})).unwrap(), 3);
    }

    #[test]
    fn slot_of_rejects_zero_and_missing() {
        assert!(slot_of(&json!({"slot": 0})).is_err());
        assert!(slot_of(&json!({})).is_err());
    }
}
